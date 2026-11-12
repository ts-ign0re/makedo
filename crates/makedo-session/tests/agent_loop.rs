//! Интеграционный тест agent loop: mock-провайдер → тул-колл → permission →
//! исполнение → второй шаг → завершение.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::significant_drop_tightening
)]

use std::sync::{Arc, Mutex};

use makedo_providers::{CompletionRequest, EventStream, LlmEvent, Provider, StopReason, Usage};
use makedo_session::permission::{Action, Rule, Ruleset};
use makedo_session::{SessionCommand, SessionConfig, spawn};
use makedo_tools::registry::Registry;
use makedo_tools::{Tool, ToolCtx, ToolError};
use makedo_types::event::SessionEvent;
use schemars::JsonSchema;
use serde::Deserialize;

struct MockProvider {
    script: Mutex<Vec<Vec<LlmEvent>>>,
    seen_requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Provider for MockProvider {
    fn stream(&self, req: CompletionRequest) -> EventStream {
        self.seen_requests.lock().unwrap().push(req);
        let step = self.script.lock().unwrap().remove(0);
        Box::pin(futures_util::stream::iter(step.into_iter().map(Ok)))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MarkArgs {
    value: String,
}

#[derive(Clone)]
struct MarkTool {
    hits: Arc<Mutex<Vec<String>>>,
}

impl Tool for MarkTool {
    type Args = MarkArgs;
    fn name(&self) -> &'static str {
        "mark"
    }
    fn description(&self) -> &'static str {
        "records value"
    }
    async fn run(&self, _ctx: &ToolCtx, args: MarkArgs) -> Result<String, ToolError> {
        self.hits.lock().unwrap().push(args.value.clone());
        Ok(format!("marked {}", args.value))
    }
}

fn cfg() -> SessionConfig {
    SessionConfig {
        provider: makedo_config::ProviderConfig {
            model: "test-model".into(),
            ..makedo_config::ProviderConfig::default()
        },
        system_prompt: "test".into(),
        cwd: std::env::temp_dir(),
        spill_dir: std::env::temp_dir().join("makedo-session-test"),
        concurrency: 2,
        ponytail: makedo_config::PonytailLevel::Off,
        ..SessionConfig::default()
    }
}

fn tool_call_step(value: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::ToolCallStart {
            call_id: "c1".into(),
            name: "mark".into(),
        },
        LlmEvent::ToolCallDelta {
            call_id: "c1".into(),
            args_delta: format!("{{\"value\":\"{value}\"}}"),
        },
        LlmEvent::Finished {
            reason: StopReason::ToolCalls,
            usage: Usage::default(),
        },
    ]
}

fn text_step(text: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::TextDelta(text.into()),
        LlmEvent::Finished {
            reason: StopReason::Stop,
            usage: Usage::default(),
        },
    ]
}

async fn drain_until_finish(handle: &mut makedo_session::SessionHandle) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    while let Some(ev) = handle.events.recv().await {
        let done = matches!(ev, SessionEvent::TurnFinished { .. });
        events.push(ev);
        if done {
            break;
        }
    }
    events
}

#[tokio::test]
async fn full_turn_with_allowed_tool() {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::default();
    registry
        .register(MarkTool {
            hits: Arc::clone(&hits),
        })
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = MockProvider {
        script: Mutex::new(vec![tool_call_step("v1"), text_step("done")]),
        seen_requests: Arc::clone(&seen),
    };
    let mut ruleset = Ruleset::default_policy();
    ruleset.push(Rule::new("mark", "*", Action::Allow).unwrap());

    let mut handle = spawn(Box::new(provider), Arc::new(registry), ruleset, cfg());
    handle
        .commands
        .send(SessionCommand::Prompt("go".into()))
        .await
        .unwrap();
    let events = drain_until_finish(&mut handle).await;

    assert_eq!(*hits.lock().unwrap(), vec!["v1".to_owned()]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolStarted { name, .. } if name == "mark"))
    );
    assert!(events.iter().any(
        |e| matches!(e, SessionEvent::ToolFinished { is_error, preview, .. } if !is_error && preview.contains("marked v1"))
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::TextDelta(t) if t == "done"))
    );
    // второй запрос к LLM видит tool_result в истории и алфавитный манифест
    let reqs = seen.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[1]
        .messages
        .iter()
        .any(|m| matches!(m, makedo_providers::Message::ToolResult { content, .. } if content.contains("marked v1"))));
}

#[tokio::test]
async fn output_limit_skips_unfinished_batch_and_executes_reissued_call_once() {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::default();
    registry
        .register(MarkTool {
            hits: Arc::clone(&hits),
        })
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let output = vec![
        serde_json::json!({"type":"reasoning","id":"rs_limit","summary":[],"encrypted_content":"opaque"}),
        serde_json::json!({"type":"function_call","call_id":"c1","name":"mark","arguments":"{\"value\":\"must-not-run\"}","status":"completed"}),
        serde_json::json!({"type":"function_call","call_id":"partial","name":"mark","arguments":"{\"value\":","status":"incomplete"}),
    ];
    let mut limited = tool_call_step("must-not-run");
    limited.pop();
    limited.extend([
        LlmEvent::TextDelta("Partial answer".into()),
        LlmEvent::ToolCallStart {
            call_id: "partial".into(),
            name: "mark".into(),
        },
        LlmEvent::ToolCallDelta {
            call_id: "partial".into(),
            args_delta: "{\"value\":".into(),
        },
        LlmEvent::OpenAiOutput(output.clone()),
        LlmEvent::Finished {
            reason: StopReason::MaxTokens,
            usage: Usage {
                output_tokens: 8192,
                ..Usage::default()
            },
        },
    ]);
    let mut reissued = tool_call_step("reissued");
    for event in &mut reissued {
        if let LlmEvent::ToolCallStart { call_id, .. } | LlmEvent::ToolCallDelta { call_id, .. } =
            event
        {
            *call_id = "c2".into();
        }
    }
    let provider = MockProvider {
        script: Mutex::new(vec![limited, reissued, text_step("done")]),
        seen_requests: Arc::clone(&seen),
    };
    let mut ruleset = Ruleset::default_policy();
    ruleset.push(Rule::new("mark", "*", Action::Allow).unwrap());
    let mut handle = spawn(Box::new(provider), Arc::new(registry), ruleset, cfg());
    handle
        .commands
        .send(SessionCommand::Prompt("update wiki".into()))
        .await
        .unwrap();
    let events = drain_until_finish(&mut handle).await;
    assert_eq!(*hits.lock().unwrap(), vec!["reissued"]);
    assert!(!events.iter().any(|e| matches!(e, SessionEvent::Error(_))));
    assert!(events.iter().any(
        |e| matches!(e, SessionEvent::TurnFinished {usage,..} if usage.output_tokens == 8192)
    ));
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests.iter().map(|r| r.max_tokens).collect::<Vec<_>>(),
        [8192, 16384, 16384]
    );
    let messages = &requests[1].messages;
    assert!(messages.iter().any(|m| matches!(m, makedo_providers::Message::Assistant {content} if content.iter().any(|p| matches!(p, makedo_providers::AssistantPart::OpenAiOutput {items} if *items == output)))));
    for id in ["c1", "partial"] {
        assert!(messages.iter().any(|m| matches!(m, makedo_providers::Message::ToolResult {call_id,content} if call_id == id && content.contains("not executed"))));
    }
}

#[tokio::test]
async fn ask_permission_deny_synthesizes_result() {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::default();
    registry
        .register(MarkTool {
            hits: Arc::clone(&hits),
        })
        .unwrap();
    let provider = MockProvider {
        script: Mutex::new(vec![tool_call_step("v2"), text_step("ok")]),
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };
    // mark не в дефолтном ruleset → Ask
    let mut handle = spawn(
        Box::new(provider),
        Arc::new(registry),
        Ruleset::default_policy(),
        cfg(),
    );
    handle
        .commands
        .send(SessionCommand::Prompt("go".into()))
        .await
        .unwrap();

    // ждём PermissionAsk и отклоняем
    let mut finished = false;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            SessionEvent::PermissionAsk { request_id, .. } => {
                handle
                    .commands
                    .send(SessionCommand::PermissionReply {
                        request_id,
                        approve: false,
                        always: false,
                    })
                    .await
                    .unwrap();
            }
            SessionEvent::TurnFinished { .. } => {
                finished = true;
                break;
            }
            _ => {}
        }
    }
    assert!(finished);
    // тул НЕ исполнился, но модель получила синтетический результат (2-й шаг случился)
    assert!(hits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn run_tool_direct_missing_image_gen_explains_openrouter() {
    let provider = MockProvider {
        script: Mutex::new(vec![]),
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let mut handle = spawn(
        Box::new(provider),
        Arc::new(Registry::default()),
        Ruleset::default_policy(),
        cfg(),
    );
    handle
        .commands
        .send(SessionCommand::RunTool {
            name: "image_gen".into(),
            args: serde_json::json!({"prompt": "x"}),
        })
        .await
        .unwrap();
    let mut saw_hint = false;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            SessionEvent::Error(e) => saw_hint = e.contains("OPENROUTER_API_KEY"),
            SessionEvent::TurnFinished { .. } => break,
            _ => {}
        }
    }
    assert!(saw_hint, "missing image_gen must explain OpenRouter setup");
}

#[tokio::test]
async fn verify_gate_red_then_green_feeds_failure_back() {
    // маркер: первая проверка красная (создаёт файл), вторая — зелёная
    let dir = std::env::temp_dir().join(format!("makedo-vgate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("ok");
    let cmd = format!(
        "test -f {m} || {{ touch {m}; echo BOOM; exit 1; }}",
        m = marker.display()
    );

    let hits = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::default();
    registry
        .register(MarkTool {
            hits: Arc::clone(&hits),
        })
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = MockProvider {
        // турн с правкой (mark не read-only) → гейт красный → fix-турн → зелёный
        script: Mutex::new(vec![
            tool_call_step("v1"),
            text_step("done"),
            text_step("fixed"),
        ]),
        seen_requests: Arc::clone(&seen),
    };
    let mut ruleset = Ruleset::default_policy();
    ruleset.push(Rule::new("mark", "*", Action::Allow).unwrap());
    let mut c = cfg();
    c.verify_cmds = vec![cmd];

    let mut handle = spawn(Box::new(provider), Arc::new(registry), ruleset, c);
    handle
        .commands
        .send(SessionCommand::Prompt("go".into()))
        .await
        .unwrap();
    let events = drain_until_finish(&mut handle).await;

    // гейт был красным, затем зелёным; вывод проверки ушёл модели
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Notice(n) if n.contains("verify gate: red")))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Notice(n) if n.contains("verify gate: green")))
    );
    let reqs = seen.lock().unwrap();
    assert_eq!(reqs.len(), 3, "tool-турн + текст + fix-турн");
    let last = reqs.last().unwrap();
    let fed_back = last.messages.iter().any(|m| {
        matches!(m, makedo_providers::Message::User { content }
            if content.contains("[verification gate]") && content.contains("BOOM"))
    });
    assert!(fed_back, "вывод красной проверки должен вернуться модели");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn verify_gate_skips_read_only_turn() {
    // без правок гейт не запускается вовсе (cmds красные, но их не гоняют)
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = MockProvider {
        script: Mutex::new(vec![text_step("hi")]),
        seen_requests: Arc::clone(&seen),
    };
    let mut c = cfg();
    c.verify_cmds = vec!["exit 1".into()];
    let mut handle = spawn(
        Box::new(provider),
        Arc::new(Registry::default()),
        Ruleset::default_policy(),
        c,
    );
    handle
        .commands
        .send(SessionCommand::Prompt("go".into()))
        .await
        .unwrap();
    let events = drain_until_finish(&mut handle).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::Notice(n) if n.contains("verify"))),
        "гейт не должен срабатывать на read-only турне"
    );
}

#[tokio::test]
async fn mid_turn_prompt_is_journaled_and_delivered_at_safe_boundary() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = MockProvider {
        script: Mutex::new(vec![text_step("first reply"), text_step("updated reply")]),
        seen_requests: seen.clone(),
    };
    let path = std::env::temp_dir().join(format!("makedo-inbox-test-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut config = cfg();
    config.session_file = Some(path.clone());
    let mut handle = spawn(
        Box::new(provider),
        Arc::new(Registry::default()),
        Ruleset::allow_all(),
        config,
    );
    handle
        .commands
        .send(SessionCommand::Prompt("first".into()))
        .await
        .unwrap();
    handle
        .commands
        .send(SessionCommand::Prompt("steer".into()))
        .await
        .unwrap();
    let events = drain_until_finish(&mut handle).await;
    let accepted: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::PromptAccepted { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(accepted, vec!["first", "steer"]);
    let queued = events
        .iter()
        .position(|e| matches!(e, SessionEvent::PromptQueued { text, .. } if text == "steer"))
        .unwrap();
    let reply = events
        .iter()
        .position(|e| matches!(e, SessionEvent::TextDelta(text) if text == "first reply"))
        .unwrap();
    assert!(
        queued < reply,
        "inbox receipt must be visible before the current response completes"
    );
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].messages.iter().any(
            |m| matches!(m, makedo_providers::Message::User { content } if content == "steer")
        )
    );
    let restored = makedo_session::history::restore(&path).unwrap();
    assert!(restored.pending.is_empty());
    assert_eq!(
        restored
            .messages
            .iter()
            .filter(
                |m| matches!(m, makedo_providers::Message::User { content } if content == "steer")
            )
            .count(),
        1
    );
    std::fs::remove_file(path).unwrap();
}

struct PausedProvider {
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Provider for PausedProvider {
    fn stream(&self, request: CompletionRequest) -> EventStream {
        use futures_util::StreamExt;
        let mut seen = self.seen.lock().unwrap();
        seen.push(request);
        if seen.len() == 1 {
            Box::pin(
                futures_util::stream::iter(vec![
                    Ok(LlmEvent::ToolCallStart {
                        call_id: "partial".into(),
                        name: "mark".into(),
                    }),
                    Ok(LlmEvent::ToolCallDelta {
                        call_id: "partial".into(),
                        args_delta: "{\"value\":".into(),
                    }),
                    Ok(LlmEvent::TextDelta("working".into())),
                ])
                .chain(futures_util::stream::pending()),
            )
        } else {
            Box::pin(futures_util::stream::iter(
                text_step("after steering").into_iter().map(Ok),
            ))
        }
    }
}

#[tokio::test]
async fn force_interrupts_stream_preserving_inbox_and_partial_tool_pairs() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::default();
    registry.register(MarkTool { hits: hits.clone() }).unwrap();
    let path =
        std::env::temp_dir().join(format!("makedo-steer-stream-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut config = cfg();
    config.session_file = Some(path.clone());
    let mut handle = spawn(
        Box::new(PausedProvider { seen: seen.clone() }),
        Arc::new(registry),
        Ruleset::allow_all(),
        config,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        handle.commands.send(SessionCommand::Prompt("first".into())).await.unwrap();
        while !matches!(handle.events.recv().await, Some(SessionEvent::TextDelta(text)) if text == "working") {}
        handle.commands.send(SessionCommand::Prompt("queued first".into())).await.unwrap();
        while !matches!(handle.events.recv().await, Some(SessionEvent::PromptQueued { text, .. }) if text == "queued first") {}
        assert_eq!(seen.lock().unwrap().len(), 1, "ordinary Enter must not interrupt the model");
        let pending = makedo_session::history::restore(&path).unwrap();
        assert_eq!(pending.pending.len(), 1, "visible queued message must already be durable");
        handle.commands.send(SessionCommand::Steer("forced next".into())).await.unwrap();
        let events = drain_until_finish(&mut handle).await;
        assert!(events.iter().any(|e| matches!(e, SessionEvent::TextDelta(t) if t == "after steering")));
        assert!(events.iter().any(|e| matches!(e, SessionEvent::TurnFinished { cancelled: false, .. })));
    }).await.unwrap();
    assert!(
        hits.lock().unwrap().is_empty(),
        "partial calls must never execute"
    );
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let users: Vec<_> = requests[1]
        .messages
        .iter()
        .filter_map(|m| match m {
            makedo_providers::Message::User { content } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(users, ["first", "queued first", "forced next"]);
    assert!(requests[1].messages.iter().any(|m| matches!(m,
        makedo_providers::Message::ToolResult { call_id, .. } if call_id == "partial")));
    let restored = makedo_session::history::restore(&path).unwrap();
    assert!(restored.pending.is_empty() && restored.interrupted.is_empty());
    std::fs::remove_file(path).unwrap();
}

#[derive(Debug)]
struct WaitingTool;

impl Tool for WaitingTool {
    type Args = MarkArgs;
    fn name(&self) -> &'static str {
        "mark"
    }
    fn description(&self) -> &'static str {
        "wait until interrupted"
    }
    async fn run(&self, ctx: &ToolCtx, _args: MarkArgs) -> Result<String, ToolError> {
        ctx.cancel.cancelled().await;
        Err(ToolError::Cancelled)
    }
}

#[tokio::test]
async fn force_flushes_existing_inbox_during_tools_and_permission_wait() {
    for permission in [false, true] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = MockProvider {
            script: Mutex::new(vec![tool_call_step("wait"), text_step("updated")]),
            seen_requests: seen.clone(),
        };
        let mut registry = Registry::default();
        registry.register(WaitingTool).unwrap();
        let rules = if permission {
            Ruleset::default_policy()
        } else {
            Ruleset::allow_all()
        };
        let mut handle = spawn(Box::new(provider), Arc::new(registry), rules, cfg());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            handle.commands.send(SessionCommand::Prompt("first".into())).await.unwrap();
            loop {
                let event = handle.events.recv().await.unwrap();
                if matches!(event, SessionEvent::PermissionAsk { .. }) && permission
                    || matches!(event, SessionEvent::ToolStarted { .. }) && !permission { break; }
            }
            handle.commands.send(SessionCommand::Prompt("queued".into())).await.unwrap();
            while !matches!(handle.events.recv().await, Some(SessionEvent::PromptQueued { .. })) {}
            handle.commands.send(SessionCommand::Steer(String::new())).await.unwrap();
            let events = drain_until_finish(&mut handle).await;
            assert!(events.iter().any(|e| matches!(e, SessionEvent::PromptAccepted { text, queued_id: Some(_) } if text == "queued")));
            assert!(events.iter().any(|e| matches!(e, SessionEvent::TurnFinished { cancelled: false, .. })));
        }).await.unwrap();
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages.iter().any(|m| matches!(m,
            makedo_providers::Message::ToolResult { call_id, content } if call_id == "c1" && content.contains("cancelled"))));
        assert_eq!(
            requests[1]
                .messages
                .iter()
                .filter(|m| matches!(m,
            makedo_providers::Message::User { content } if content == "queued"))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn force_during_verification_resumes_model_then_checks_again() {
    let root = std::env::temp_dir().join(format!("makedo-steer-verify-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let marker = root.join("started");
    let _ = std::fs::remove_file(&marker);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = MockProvider {
        script: Mutex::new(vec![
            tool_call_step("write"),
            text_step("done"),
            text_step("after verification interrupt"),
        ]),
        seen_requests: seen.clone(),
    };
    let mut registry = Registry::default();
    registry
        .register(MarkTool {
            hits: Arc::new(Mutex::new(Vec::new())),
        })
        .unwrap();
    let mut config = cfg();
    config.cwd = root.clone();
    config.verify_cmds =
        vec!["if test -f started; then exit 0; else touch started; sleep 30; fi".into()];
    let mut handle = spawn(
        Box::new(provider),
        Arc::new(registry),
        Ruleset::allow_all(),
        config,
    );
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        handle
            .commands
            .send(SessionCommand::Prompt("first".into()))
            .await
            .unwrap();
        while !marker.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        handle
            .commands
            .send(SessionCommand::Steer("change during verification".into()))
            .await
            .unwrap();
        let events = drain_until_finish(&mut handle).await;
        assert!(events.iter().any(
            |e| matches!(e, SessionEvent::TextDelta(t) if t == "after verification interrupt")
        ));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::Notice(t) if t == "verify gate: green"))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::TurnFinished {
                cancelled: false,
                ..
            }
        )));
    })
    .await
    .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert!(seen.lock().unwrap()[2].messages.iter().any(|m| matches!(m,
        makedo_providers::Message::User { content } if content == "change during verification")));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn resume_marks_interrupted_tool_without_replaying_it() {
    let path =
        std::env::temp_dir().join(format!("makedo-resume-test-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    makedo_session::history::append(
        &path,
        &makedo_providers::Message::User {
            content: "original".into(),
        },
    )
    .unwrap();
    makedo_session::history::append(
        &path,
        &makedo_providers::Message::Assistant {
            content: vec![makedo_providers::AssistantPart::ToolCall {
                call_id: "interrupted".into(),
                name: "mark".into(),
                input: serde_json::json!({"value":"must not replay"}),
            }],
        },
    )
    .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::default();
    registry.register(MarkTool { hits: hits.clone() }).unwrap();
    let provider = MockProvider {
        script: Mutex::new(vec![text_step("restored")]),
        seen_requests: seen.clone(),
    };
    let mut config = cfg();
    config.session_file = Some(path.clone());
    let mut handle = spawn(
        Box::new(provider),
        Arc::new(registry),
        Ruleset::allow_all(),
        config,
    );
    handle
        .commands
        .send(SessionCommand::Prompt("continue".into()))
        .await
        .unwrap();
    drain_until_finish(&mut handle).await;
    assert!(hits.lock().unwrap().is_empty());
    assert!(seen.lock().unwrap()[0].messages.iter().any(|m| matches!(m, makedo_providers::Message::ToolResult { call_id, content } if call_id == "interrupted" && content.contains("NOT replayed"))));
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn direct_command_cancel_publishes_its_log_and_cancelled_finish() {
    let mut registry = Registry::default();
    makedo_tools::builtin::register_all(&mut registry).unwrap();
    let provider = MockProvider {
        script: Mutex::new(Vec::new()),
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let mut handle = spawn(
        Box::new(provider),
        Arc::new(registry),
        Ruleset::allow_all(),
        cfg(),
    );
    handle
        .commands
        .send(SessionCommand::RunTool {
            name: "bash".into(),
            args: serde_json::json!({"command":"printf running; sleep 30","heavy":false}),
        })
        .await
        .unwrap();
    loop {
        if matches!(
            handle.events.recv().await,
            Some(SessionEvent::ToolStarted { .. })
        ) {
            break;
        }
    }
    handle.commands.send(SessionCommand::Cancel).await.unwrap();
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        drain_until_finish(&mut handle),
    )
    .await
    .unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::TurnFinished {
            cancelled: true,
            ..
        }
    )));
    assert!(events.iter().any(
        |e| matches!(e, SessionEvent::ToolLog { path, .. } if std::path::Path::new(path).is_file())
    ));
}

#[tokio::test]
async fn settings_do_not_block_mid_turn_input() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = MockProvider {
        script: Mutex::new(vec![tool_call_step("first"), text_step("updated")]),
        seen_requests: seen.clone(),
    };
    let mut registry = Registry::default();
    registry
        .register(MarkTool {
            hits: Arc::new(Mutex::new(Vec::new())),
        })
        .unwrap();
    let mut handle = spawn(
        Box::new(provider),
        Arc::new(registry),
        Ruleset::allow_all(),
        cfg(),
    );
    handle
        .commands
        .send(SessionCommand::Prompt("first".into()))
        .await
        .unwrap();
    handle
        .commands
        .send(SessionCommand::SetPonytail(
            makedo_config::PonytailLevel::Lite,
        ))
        .await
        .unwrap();
    handle
        .commands
        .send(SessionCommand::Prompt("steering".into()))
        .await
        .unwrap();
    drain_until_finish(&mut handle).await;
    let requests = seen.lock().unwrap();
    assert!(requests[1].system.contains("**lite**"));
    assert!(requests[1].messages.iter().any(
        |m| matches!(m, makedo_providers::Message::User { content } if content == "steering")
    ));
}

#[tokio::test]
async fn standalone_yolo_costs_no_request_and_mixed_prompt_auto_approves() {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::default();
    registry
        .register(MarkTool {
            hits: Arc::clone(&hits),
        })
        .unwrap();
    let provider = MockProvider {
        script: Mutex::new(vec![tool_call_step("done"), text_step("complete")]),
        seen_requests: Arc::clone(&seen),
    };
    let mut handle = spawn(
        Box::new(provider),
        Arc::new(registry),
        Ruleset::default_policy(),
        cfg(),
    );
    handle
        .commands
        .send(SessionCommand::Prompt("YOLO!".into()))
        .await
        .unwrap();
    let events = drain_until_finish(&mut handle).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::PermissionsChanged { allow_all: true }))
    );
    assert!(seen.lock().unwrap().is_empty());
    handle
        .commands
        .send(SessionCommand::Prompt("YOLO: make the change".into()))
        .await
        .unwrap();
    let events = drain_until_finish(&mut handle).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::PermissionAsk { .. }))
    );
    assert_eq!(*hits.lock().unwrap(), ["done"]);
    assert!(seen.lock().unwrap()[0].system.contains("YOLO mode"));
}
