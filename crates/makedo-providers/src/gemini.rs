//! Google Gemini `generateContent` (стриминг через `alt=sse`). PLAN M3.5.
//! Особенности диалекта: тул-коллы приходят целиком (не дельтами), у коллов
//! нет id — синтезируем `name#N`, а при реплее истории достаём имя обратно.

use serde_json::{Value, json};

use crate::http::{EventMapper, event_stream, send_with_retries};
use crate::sse::SseEvent;
use crate::{
    AssistantPart, CompletionRequest, EventStream, LlmEvent, Message, Provider, ProviderError,
    StopReason, Usage,
};

const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

#[derive(Debug)]
pub struct Gemini {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

/// Имя функции из синтетического id `name#N`.
fn name_from_call_id(call_id: &str) -> &str {
    call_id.split_once('#').map_or(call_id, |(n, _)| n)
}

impl Gemini {
    #[must_use]
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE.to_owned()),
        }
    }

    fn body(req: &CompletionRequest) -> Value {
        let mut contents = Vec::new();
        for m in &req.messages {
            match m {
                Message::System { content } => {
                    // system из истории — как user-текст (у gemini один system_instruction)
                    contents.push(json!({"role": "user", "parts": [{"text": content}]}));
                }
                Message::User { content } => {
                    contents.push(json!({"role": "user", "parts": [{"text": content}]}));
                }
                Message::Assistant { content } => {
                    let parts: Vec<Value> = content
                        .iter()
                        .filter_map(|p| match p {
                            AssistantPart::OpenAiOutput { .. } => None,
                            AssistantPart::Text { text } => Some(json!({"text": text})),
                            AssistantPart::ToolCall { call_id, input, .. } => {
                                Some(json!({"functionCall": {
                                    "name": name_from_call_id(call_id), "args": input
                                }}))
                            }
                        })
                        .collect();
                    if !parts.is_empty() {
                        contents.push(json!({"role": "model", "parts": parts}));
                    }
                }
                Message::ToolResult { call_id, content } => {
                    // Gemini требует, чтобы N functionResponse шли в ОДНОМ user-content
                    // после модельного хода с N functionCall (параллельные коллы),
                    // иначе 400. Мержим последовательные tool_result в один content.
                    let part = json!({"functionResponse": {
                        "name": name_from_call_id(call_id),
                        "response": {"output": content},
                    }});
                    if let Some(last) = contents.last_mut()
                        && last["role"] == "user"
                        && last["parts"][0].get("functionResponse").is_some()
                        && let Some(parts) = last["parts"].as_array_mut()
                    {
                        parts.push(part);
                    } else {
                        contents.push(json!({"role": "user", "parts": [part]}));
                    }
                }
            }
        }
        let declarations: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({"name": t.name, "description": t.description, "parameters": t.input_schema})
            })
            .collect();
        let mut body = json!({
            "system_instruction": {"parts": [{"text": req.system}]},
            "contents": contents,
            "generationConfig": {"maxOutputTokens": req.max_tokens},
        });
        if !declarations.is_empty() {
            body["tools"] = json!([{"function_declarations": declarations}]);
        }
        body
    }
}

impl Provider for Gemini {
    fn stream(&self, req: CompletionRequest) -> EventStream {
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, req.model
        );
        let client = self.client.clone();
        let key = self.api_key.clone();
        let body = Self::body(&req);
        let fut = Box::pin(async move {
            send_with_retries(|| client.post(&url).header("x-goog-api-key", &key).json(&body)).await
        });
        event_stream(fut, Mapper::default())
    }
}

#[derive(Debug, Default)]
struct Mapper {
    calls_seen: u64,
    finish: Option<StopReason>,
    usage: Usage,
}

impl EventMapper for Mapper {
    fn map(&mut self, ev: SseEvent) -> Result<Vec<LlmEvent>, ProviderError> {
        if ev.data.is_empty() {
            return Ok(vec![]);
        }
        let data: Value = serde_json::from_str(&ev.data)
            .map_err(|e| ProviderError::Protocol(format!("bad gemini chunk: {e}")))?;
        let mut out = Vec::new();
        if let Some(u) = data.get("usageMetadata") {
            let cached = u["cachedContentTokenCount"].as_u64().unwrap_or(0);
            self.usage = Usage {
                // promptTokenCount уже включает кэшированные — инвариант makedo соблюдён
                input_tokens: u["promptTokenCount"].as_u64().unwrap_or(0),
                output_tokens: u["candidatesTokenCount"].as_u64().unwrap_or(0),
                cache_read_tokens: cached,
                cache_write_tokens: 0,
                cost_usd: None,
            };
        }
        let Some(candidate) = data["candidates"].get(0) else {
            return Ok(out);
        };
        if let Some(parts) = candidate["content"]["parts"].as_array() {
            for part in parts {
                if let Some(text) = part["text"].as_str() {
                    out.push(LlmEvent::TextDelta(text.to_owned()));
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc["name"].as_str().unwrap_or_default().to_owned();
                    self.calls_seen += 1;
                    let call_id = format!("{name}#{}", self.calls_seen);
                    out.push(LlmEvent::ToolCallStart {
                        call_id: call_id.clone(),
                        name,
                    });
                    // аргументы приходят целиком — одна финальная дельта
                    out.push(LlmEvent::ToolCallDelta {
                        call_id,
                        args_delta: fc["args"].to_string(),
                    });
                }
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.finish = Some(match reason {
                "STOP" => {
                    if self.calls_seen > 0 {
                        StopReason::ToolCalls
                    } else {
                        StopReason::Stop
                    }
                }
                "MAX_TOKENS" => StopReason::MaxTokens,
                _ => StopReason::Other,
            });
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, ProviderError> {
        Ok(vec![LlmEvent::Finished {
            reason: self.finish.unwrap_or(StopReason::Other),
            usage: self.usage,
        }])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn ev(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_owned(),
        }
    }

    #[test]
    fn maps_text_calls_and_usage() {
        let mut m = Mapper::default();
        let out = m
            .map(ev(
                r#"{"candidates":[{"content":{"parts":[{"text":"hi"},{"functionCall":{"name":"grep","args":{"pattern":"x"}}}]}}]}"#,
            ))
            .unwrap();
        assert_eq!(out[0], LlmEvent::TextDelta("hi".into()));
        assert!(matches!(&out[1], LlmEvent::ToolCallStart { call_id, name }
            if name == "grep" && call_id == "grep#1"));
        assert!(matches!(&out[2], LlmEvent::ToolCallDelta { args_delta, .. }
            if args_delta.contains("pattern")));
        let out = m
            .map(ev(
                r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":7,"cachedContentTokenCount":80}}"#,
            ))
            .unwrap();
        assert!(out.is_empty());
        let fin = m.finish().unwrap();
        // STOP при наличии коллов = ToolCalls (гемини не различает)
        assert!(
            matches!(&fin[0], LlmEvent::Finished { reason: StopReason::ToolCalls, usage }
            if usage.cache_read_tokens == 80 && usage.input_tokens == 100)
        );
    }

    #[test]
    fn parallel_tool_results_merge_into_one_content() {
        // H: два подряд ToolResult (параллельные коллы) → один user-content с 2 parts
        let req = CompletionRequest {
            model: "g".into(),
            system: "s".into(),
            messages: vec![
                Message::ToolResult {
                    call_id: "grep#1".into(),
                    content: "a".into(),
                },
                Message::ToolResult {
                    call_id: "read#2".into(),
                    content: "b".into(),
                },
            ],
            tools: vec![],
            max_tokens: 100,
            reasoning_effort: None,
        };
        let body = Gemini::body(&req);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(
            contents.len(),
            1,
            "два tool_result должны слиться в один content"
        );
        assert_eq!(contents[0]["parts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn history_roundtrip_restores_function_names() {
        let req = CompletionRequest {
            model: "gemini-test".into(),
            system: "sys".into(),
            messages: vec![
                Message::Assistant {
                    content: vec![AssistantPart::ToolCall {
                        call_id: "grep#1".into(),
                        name: "grep".into(),
                        input: serde_json::json!({"p": 1}),
                    }],
                },
                Message::ToolResult {
                    call_id: "grep#1".into(),
                    content: "found".into(),
                },
            ],
            tools: vec![],
            max_tokens: 100,
            reasoning_effort: None,
        };
        let body = Gemini::body(&req);
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["name"],
            "grep"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["name"],
            "grep"
        );
        assert_eq!(body["system_instruction"]["parts"][0]["text"], "sys");
    }
}
