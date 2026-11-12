//! Anthropic Messages API (стриминг). <https://docs.anthropic.com/en/api/messages-streaming>

use serde_json::{Value, json};

use crate::http::{EventMapper, event_stream, send_with_retries};
use crate::sse::SseEvent;
use crate::{
    AssistantPart, CompletionRequest, EventStream, LlmEvent, Message, Provider, ProviderError,
    StopReason, Usage,
};

const DEFAULT_BASE: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

#[derive(Debug)]
pub struct Anthropic {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl Anthropic {
    #[must_use]
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE.to_owned()),
        }
    }

    fn body(req: &CompletionRequest) -> Value {
        let mut system = req.system.clone();
        let mut messages = Vec::new();
        for m in &req.messages {
            match m {
                // system из истории склеивается в system-параметр
                Message::System { content } => {
                    if !system.is_empty() {
                        system.push('\n');
                    }
                    system.push_str(content);
                }
                Message::User { content } => {
                    // массив блоков (не строка): иначе хвостовой cache_control
                    // не встанет на user-сообщении = вся история по полной цене
                    messages.push(json!({
                        "role": "user",
                        "content": [{"type": "text", "text": content}],
                    }));
                }
                Message::Assistant { content } => {
                    let blocks: Vec<Value> = content
                        .iter()
                        .filter_map(|p| match p {
                            AssistantPart::OpenAiOutput { .. } => None,
                            AssistantPart::Text { text } => {
                                Some(json!({"type": "text", "text": text}))
                            }
                            AssistantPart::ToolCall {
                                call_id,
                                name,
                                input,
                            } => Some(json!({
                                "type": "tool_use", "id": call_id, "name": name, "input": input
                            })),
                        })
                        .collect();
                    if !blocks.is_empty() {
                        messages.push(json!({"role": "assistant", "content": blocks}));
                    }
                }
                Message::ToolResult { call_id, content } => {
                    messages.push(json!({
                        "role": "user",
                        "content": [{"type": "tool_result", "tool_use_id": call_id, "content": content}]
                    }));
                }
            }
        }
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({"name": t.name, "description": t.description, "input_schema": t.input_schema})
            })
            .collect();
        // Prompt caching (PLAN M3.5): breakpoints на стабильном префиксе —
        // (1) последний тул (манифест алфавитный = байт-стабильный),
        // (2) system, (3) хвост истории (двигается каждый турн, кэшируя префикс).
        let ephemeral = json!({"type": "ephemeral"});
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = ephemeral.clone();
        }
        let system_blocks = json!([
            {"type": "text", "text": system, "cache_control": ephemeral.clone()}
        ]);
        if let Some(last_msg) = messages.last_mut()
            && let Some(blocks) = last_msg["content"].as_array_mut()
            && let Some(last_block) = blocks.last_mut()
        {
            last_block["cache_control"] = ephemeral;
        }
        json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "system": system_blocks,
            "messages": messages,
            "tools": tools,
            "stream": true,
        })
    }
}

impl Provider for Anthropic {
    fn stream(&self, req: CompletionRequest) -> EventStream {
        let url = format!("{}/v1/messages", self.base_url);
        let client = self.client.clone();
        let key = self.api_key.clone();
        let body = Self::body(&req);
        let fut = Box::pin(async move {
            send_with_retries(|| {
                client
                    .post(&url)
                    .header("x-api-key", &key)
                    .header("anthropic-version", API_VERSION)
                    .json(&body)
            })
            .await
        });
        event_stream(fut, Mapper::default())
    }
}

#[derive(Debug, Default)]
struct Mapper {
    current_tool: Option<String>,
    usage: Usage,
    stop: Option<StopReason>,
    emitted_finish: bool,
}

impl EventMapper for Mapper {
    fn map(&mut self, ev: SseEvent) -> Result<Vec<LlmEvent>, ProviderError> {
        let Some(kind) = ev.event.as_deref() else {
            return Ok(vec![]);
        };
        let data: Value = if ev.data.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&ev.data)
                .map_err(|e| ProviderError::Protocol(format!("bad json in `{kind}`: {e}")))?
        };
        let mut out = Vec::new();
        match kind {
            "message_start" => {
                let u = &data["message"]["usage"];
                self.usage.cache_read_tokens = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                self.usage.cache_write_tokens =
                    u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                // anthropic input_tokens = ТОЛЬКО некэшированный остаток; наш инвариант
                // Usage.input_tokens = все input (включая cached) — складываем, иначе
                // hit% > 100% и стоимость считается неверно (деньги!)
                self.usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0)
                    + self.usage.cache_read_tokens
                    + self.usage.cache_write_tokens;
            }
            "content_block_start" => {
                let block = &data["content_block"];
                if block["type"] == "tool_use" {
                    let call_id = block["id"].as_str().unwrap_or_default().to_owned();
                    let name = block["name"].as_str().unwrap_or_default().to_owned();
                    self.current_tool = Some(call_id.clone());
                    out.push(LlmEvent::ToolCallStart { call_id, name });
                }
            }
            "content_block_delta" => {
                let delta = &data["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(t) = delta["text"].as_str() {
                            out.push(LlmEvent::TextDelta(t.to_owned()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(id), Some(part)) =
                            (&self.current_tool, delta["partial_json"].as_str())
                        {
                            out.push(LlmEvent::ToolCallDelta {
                                call_id: id.clone(),
                                args_delta: part.to_owned(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => self.current_tool = None,
            "message_delta" => {
                if let Some(r) = data["delta"]["stop_reason"].as_str() {
                    self.stop = Some(match r {
                        "end_turn" => StopReason::Stop,
                        "tool_use" => StopReason::ToolCalls,
                        "max_tokens" => StopReason::MaxTokens,
                        _ => StopReason::Other,
                    });
                }
                if let Some(o) = data["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = o;
                }
            }
            "message_stop" => {
                self.emitted_finish = true;
                out.push(LlmEvent::Finished {
                    reason: self.stop.unwrap_or(StopReason::Other),
                    usage: self.usage,
                });
            }
            "error" => {
                return Err(ProviderError::Protocol(format!(
                    "server error event: {data}"
                )));
            }
            // ping и незнакомые событийные типы игнорируем
            _ => {}
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, ProviderError> {
        if self.emitted_finish {
            Ok(vec![])
        } else {
            // стрим оборвался без message_stop — честно закрываем турн
            Ok(vec![LlmEvent::Finished {
                reason: self.stop.unwrap_or(StopReason::Other),
                usage: self.usage,
            }])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, data: &str) -> SseEvent {
        SseEvent {
            event: Some(kind.to_owned()),
            data: data.to_owned(),
        }
    }

    #[test]
    fn user_message_is_block_array_for_cache_breakpoint() {
        // C2: последнее user-сообщение обязано быть массивом блоков, иначе
        // хвостовой cache_control не встаёт и история читается по полной цене
        let req = CompletionRequest {
            model: "m".into(),
            system: "sys".into(),
            messages: vec![Message::User {
                content: "hi".into(),
            }],
            tools: vec![],
            max_tokens: 100,
            reasoning_effort: None,
        };
        let body = Anthropic::body(&req);
        let last = &body["messages"][0];
        assert!(
            last["content"].is_array(),
            "user content must be a block array"
        );
        assert_eq!(last["content"][0]["cache_control"]["type"], "ephemeral");
        // system тоже блок с cache_control
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn usage_folds_cache_into_input() {
        // H: input_tokens должен включать кэшированные (иначе hit% > 100%)
        let mut m = Mapper::default();
        let _ = m.map(ev(
            "message_start",
            r#"{"message":{"usage":{"input_tokens":12,"cache_read_input_tokens":15000,"cache_creation_input_tokens":800}}}"#,
        ));
        assert_eq!(m.usage.input_tokens, 12 + 15000 + 800);
        assert_eq!(m.usage.cache_read_tokens, 15000);
    }

    #[test]
    fn maps_text_and_tool_flow() {
        let mut m = Mapper::default();
        assert!(
            m.map(ev(
                "message_start",
                r#"{"message":{"usage":{"input_tokens":10}}}"#
            ))
            .is_ok_and(|v| v.is_empty())
        );
        let out = m
            .map(ev(
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":"hi"}}"#,
            ))
            .unwrap_or_default();
        assert_eq!(out, vec![LlmEvent::TextDelta("hi".into())]);
        let out = m
            .map(ev(
                "content_block_start",
                r#"{"content_block":{"type":"tool_use","id":"c1","name":"read"}}"#,
            ))
            .unwrap_or_default();
        assert!(matches!(&out[0], LlmEvent::ToolCallStart { name, .. } if name == "read"));
        let out = m
            .map(ev(
                "content_block_delta",
                r#"{"delta":{"type":"input_json_delta","partial_json":"{\"p\""}}"#,
            ))
            .unwrap_or_default();
        assert!(matches!(&out[0], LlmEvent::ToolCallDelta { call_id, .. } if call_id == "c1"));
        m.map(ev(
            "message_delta",
            r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
        ))
        .unwrap_or_default();
        let out = m.map(ev("message_stop", "")).unwrap_or_default();
        assert_eq!(
            out,
            vec![LlmEvent::Finished {
                reason: StopReason::ToolCalls,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    cost_usd: None,
                }
            }]
        );
        // finish() после явного message_stop ничего не дублирует
        assert!(m.finish().is_ok_and(|v| v.is_empty()));
    }
}
