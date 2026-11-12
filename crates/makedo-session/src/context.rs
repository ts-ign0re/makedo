//! Бюджет всего запроса и сокращение реплея; append-only журнал не меняется.

use makedo_providers::{AssistantPart, CompletionRequest, Message};

const CLEARED: &str = "[Old tool result content cleared]";
const SHORTENED: &str = "\n[Output shortened to fit context; read the needed range again.]\n";
const KEEP_BYTES: usize = 2_000;

#[derive(Debug, Default)]
pub(super) struct Budget {
    /// Наблюдаемый расход токенов на байт; сбрасывается при смене модели.
    observed: Option<(u64, u64)>,
}

impl Budget {
    pub fn observe(&mut self, req: &CompletionRequest, tokens: u64) {
        let bytes = u64::try_from(request_bytes(req)).unwrap_or(u64::MAX).max(1);
        if tokens > 0
            && self
                .observed
                .is_none_or(|(b, t)| tokens.saturating_mul(b) > t.saturating_mul(bytes))
        {
            self.observed = Some((bytes, tokens));
        }
    }

    pub fn fit(
        &self,
        req: &mut CompletionRequest,
        window: Option<u64>,
    ) -> Result<bool, &'static str> {
        let tokens = window.map_or(100_000, |ctx| {
            ctx.saturating_sub(u64::from(req.max_tokens))
                .saturating_sub(ctx / 20)
        });
        // ponytail: bytes/4 + фактический usage; точный токенизатор — при необходимости.
        let bytes = self.observed.map_or(tokens.saturating_mul(4), |(b, t)| {
            tokens.saturating_mul(4).min(tokens.saturating_mul(b) / t)
        });
        let limit = usize::try_from(bytes).unwrap_or(usize::MAX);
        if request_bytes(req) <= limit {
            return Ok(false);
        }
        // Небольшой запас уменьшает число компакций и разрывов prompt-кэша.
        compact(req, limit.saturating_mul(9) / 10);
        if request_bytes(req) > limit {
            return Err(
                "The current prompt, tool arguments, or system instructions exceed the model's context window even after shortening previous output. Shorten the input or select a model with a larger context.",
            );
        }
        Ok(true)
    }

    pub fn recover(req: &mut CompletionRequest) -> bool {
        let before = request_bytes(req);
        // Сервер может сообщать лишь нижнюю границу числа токенов (vLLM).
        // Убираем существенную часть, а не вычитаем один токен из max_tokens.
        compact(req, before.saturating_mul(3) / 4);
        request_bytes(req) < before
    }
}

fn message_bytes(message: &Message) -> usize {
    if let Message::Assistant { content } = message
        && let Some(items) = content.iter().find_map(|p| match p {
            AssistantPart::OpenAiOutput { items } => Some(items),
            _ => None,
        })
    {
        return 32 + serde_json::to_vec(items).map_or(0, |bytes| bytes.len());
    }
    32 + match message {
        Message::System { content } | Message::User { content } => content.len(),
        Message::ToolResult { content, call_id } => content.len() + call_id.len(),
        Message::Assistant { content } => content
            .iter()
            .map(|part| match part {
                AssistantPart::OpenAiOutput { .. } => 0,
                AssistantPart::Text { text } => text.len(),
                AssistantPart::ToolCall {
                    call_id,
                    name,
                    input,
                } => call_id.len() + name.len() + input.to_string().len() + 64,
            })
            .sum(),
    }
}

fn request_bytes(req: &CompletionRequest) -> usize {
    req.system.len()
        + 32
        + req.messages.iter().map(message_bytes).sum::<usize>()
        + serde_json::to_vec(&req.tools).map_or(0, |json| json.len())
}

fn shorten(text: &mut String) {
    if text.len() <= KEEP_BYTES {
        return;
    }
    let head = text.floor_char_boundary((KEEP_BYTES - SHORTENED.len()) / 2);
    let tail = text.ceil_char_boundary(text.len() - (KEEP_BYTES - SHORTENED.len()) / 2);
    *text = format!("{}{SHORTENED}{}", &text[..head], &text[tail..]);
}

fn compact(req: &mut CompletionRequest, target: usize) {
    let mut size = request_bytes(req);
    let protected = req.messages.len().saturating_sub(2);
    // Сначала убираем старые tool-выводы. ID и пары вызов/результат сохраняются.
    for message in &mut req.messages[..protected] {
        if size <= target {
            return;
        }
        if let Message::ToolResult { content, .. } = message
            && content.len() > CLEARED.len()
        {
            size -= content.len() - CLEARED.len();
            CLEARED.clone_into(content);
        }
    }
    // Даже свежий результат одного bash/read может быть больше всего окна.
    for message in &mut req.messages {
        if size <= target {
            return;
        }
        match message {
            Message::ToolResult { content, .. } => {
                let before = content.len();
                shorten(content);
                size -= before - content.len();
            }
            Message::Assistant { content } => {
                // Responses output is replayed verbatim. Drop an old group below
                // instead of shortening only its neutral/display copy.
                if content
                    .iter()
                    .any(|p| matches!(p, AssistantPart::OpenAiOutput { .. }))
                {
                    continue;
                }
                for part in content {
                    if let AssistantPart::Text { text } = part {
                        let before = text.len();
                        shorten(text);
                        size -= before - text.len();
                    }
                }
            }
            Message::System { .. } | Message::User { .. } => {}
        }
    }
    // Длинные цепочки вызовов убираем только целыми группами, сохраняя последнюю.
    // Пользовательские сообщения, системные инструкции и текущие аргументы не режем.
    while size > target {
        let Some(last) = req
            .messages
            .iter()
            .rposition(|m| matches!(m, Message::Assistant { .. }))
        else {
            break;
        };
        let Some(first) = req.messages[..last]
            .iter()
            .position(|m| matches!(m, Message::Assistant { .. }))
        else {
            break;
        };
        let end = req.messages[first + 1..]
            .iter()
            .position(|m| !matches!(m, Message::ToolResult { .. }))
            .map_or(req.messages.len(), |i| first + 1 + i);
        size -= req.messages[first..end]
            .iter()
            .map(message_bytes)
            .sum::<usize>();
        req.messages.drain(first..end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use makedo_providers::ToolDef;

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: "qwen3.8".into(),
            system: "system instructions".into(),
            messages: vec![
                Message::User {
                    content: "Keep the task and constraints.".into(),
                },
                Message::Assistant {
                    content: vec![AssistantPart::ToolCall {
                        call_id: "read-1".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path":"file.rs"}),
                    }],
                },
                Message::ToolResult {
                    call_id: "read-1".into(),
                    content: "a".repeat(300_000),
                },
            ],
            tools: Vec::new(),
            max_tokens: 8192,
            reasoning_effort: None,
        }
    }

    #[test]
    fn responses_history_is_compacted_as_whole_groups() {
        let mut req = request();
        let output = vec![
            serde_json::json!({"type":"reasoning", "id":"rs_old", "encrypted_content":"x".repeat(10_000)}),
        ];
        req.messages[1] = Message::Assistant {
            content: vec![
                AssistantPart::Text {
                    text: "x".repeat(10_000),
                },
                AssistantPart::OpenAiOutput { items: output },
            ],
        };
        let latest = vec![
            serde_json::json!({"type":"reasoning", "id":"rs_latest", "encrypted_content":"opaque"}),
        ];
        req.messages.push(Message::Assistant {
            content: vec![AssistantPart::OpenAiOutput {
                items: latest.clone(),
            }],
        });
        compact(&mut req, 1_000);
        assert_eq!(req.messages.len(), 2);
        assert!(matches!(&req.messages[1], Message::Assistant { content }
            if matches!(&content[0], AssistantPart::OpenAiOutput { items } if items == &latest)));
        assert!(request_bytes(&req) < 1_000);
    }

    #[test]
    fn budget_includes_system_tools_output_and_handles_oversized_fresh_result() {
        let mut req = request();
        req.system = "s".repeat(80_000);
        req.tools.push(ToolDef {
            name: "read".into(),
            description: "d".repeat(60_000),
            input_schema: serde_json::json!({}),
        });
        assert!(matches!(
            Budget::default().fit(&mut req, Some(65_536)),
            Ok(true)
        ));
        assert!(request_bytes(&req).div_ceil(4) + req.max_tokens as usize + 65_536 / 20 <= 65_536);
        assert_eq!(req.system.len(), 80_000);
        assert!(
            matches!(&req.messages[0], Message::User { content } if content == "Keep the task and constraints.")
        );
        assert!(
            matches!(&req.messages[1], Message::Assistant { content } if matches!(&content[0], AssistantPart::ToolCall { call_id, .. } if call_id == "read-1"))
        );
        assert!(
            matches!(&req.messages[2], Message::ToolResult { call_id, content } if call_id == "read-1" && content.len() <= KEEP_BYTES)
        );
    }

    #[test]
    fn irreducible_user_prompt_is_not_silently_truncated() {
        let mut req = request();
        req.messages = vec![Message::User {
            content: "task".repeat(100_000),
        }];
        assert!(Budget::default().fit(&mut req, Some(65_536)).is_err());
        assert!(!Budget::recover(&mut req));
        assert!(matches!(&req.messages[0], Message::User { content } if content.len() == 400_000));
    }

    #[test]
    fn observed_usage_corrects_dense_text_estimate() {
        let mut req = request();
        if let Message::ToolResult { content, .. } = &mut req.messages[2] {
            *content = "x".repeat(120_000);
        }
        let mut budget = Budget::default();
        assert!(matches!(budget.fit(&mut req, Some(65_536)), Ok(false)));
        budget.observe(&req, 60_000);
        assert!(matches!(budget.fit(&mut req, Some(65_536)), Ok(true)));
    }

    #[test]
    fn shortening_multibyte_output_keeps_both_ends() {
        let mut text = format!("BEGIN{}END", "🦀привет".repeat(2000));
        shorten(&mut text);
        assert!(text.len() <= KEEP_BYTES);
        assert!(text.starts_with("BEGIN") && text.ends_with("END"));
        assert!(text.contains(SHORTENED));
    }

    #[test]
    fn old_tool_groups_are_removed_together() {
        let mut req = request();
        let mut newer = req.messages[1..].to_vec();
        if let Message::Assistant { content } = &mut newer[0]
            && let AssistantPart::ToolCall { call_id, .. } = &mut content[0]
        {
            *call_id = "read-2".into();
        }
        if let Message::ToolResult { call_id, content } = &mut newer[1] {
            *call_id = "read-2".into();
            *content = "latest".into();
        }
        req.messages.extend(newer);
        compact(&mut req, 1);
        assert_eq!(req.messages.len(), 3);
        assert!(
            matches!(&req.messages[1], Message::Assistant { content } if matches!(&content[0], AssistantPart::ToolCall { call_id, .. } if call_id == "read-2"))
        );
        assert!(
            matches!(&req.messages[2], Message::ToolResult { call_id, content } if call_id == "read-2" && content == "latest")
        );
    }
}
