//! Native `OpenAI` Responses: streamed text, complete function calls and stateless replay.

use serde_json::{Value, json};

use crate::http::EventMapper;
use crate::sse::SseEvent;
use crate::{
    AssistantPart, CompletionRequest, LlmEvent, Message, ProviderError, StopReason, Usage,
};

pub(crate) fn body(req: &CompletionRequest) -> Value {
    let mut input = Vec::new();
    for message in &req.messages {
        match message {
            Message::System { content } => {
                input.push(json!({"role": "developer", "content": content}));
            }
            Message::User { content } => input.push(json!({"role": "user", "content": content})),
            Message::ToolResult { call_id, content } => input.push(json!({
                "type": "function_call_output", "call_id": call_id, "output": content,
            })),
            Message::Assistant { content } => {
                if let Some(items) = content.iter().find_map(|part| match part {
                    AssistantPart::OpenAiOutput { items } => Some(items),
                    _ => None,
                }) {
                    // Preserve ordering, IDs, message phases and encrypted reasoning.
                    input.extend(items.iter().cloned());
                    continue;
                }
                // History from an older MAKEDO or another provider has neutral parts only.
                for part in content {
                    match part {
                        AssistantPart::Text { text } => {
                            input.push(json!({"role": "assistant", "content": text}));
                        }
                        AssistantPart::ToolCall {
                            call_id,
                            name,
                            input: args,
                        } => input.push(json!({
                            "type": "function_call", "call_id": call_id, "name": name,
                            "arguments": args.to_string(),
                        })),
                        AssistantPart::OpenAiOutput { .. } => {}
                    }
                }
            }
        }
    }
    let mut body = json!({
        "model": req.model,
        "instructions": req.system,
        "input": input,
        "max_output_tokens": req.max_tokens,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function", "name": tool.name, "description": tool.description,
                    "parameters": tool.input_schema,
                    // Keep existing optional tool arguments optional.
                    "strict": false,
                })
            })
            .collect();
    }
    if let Some(effort) = &req.reasoning_effort {
        body["reasoning"] = json!({"effort": effort});
    }
    body
}

#[derive(Debug, Default)]
pub(crate) struct Mapper {
    done: bool,
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    value[key]
        .as_str()
        .ok_or_else(|| ProviderError::Protocol(format!("Responses item missing {key}")))
}

impl Mapper {
    fn response(
        &mut self,
        response: &Value,
        mut reason: StopReason,
    ) -> Result<Vec<LlmEvent>, ProviderError> {
        let output = response["output"]
            .as_array()
            .ok_or_else(|| ProviderError::Protocol("Responses completion missing output".into()))?;
        let mut events = Vec::new();
        for item in output.iter().filter(|item| item["type"] == "function_call") {
            let call_id = required(item, "call_id")?.to_owned();
            let name = required(item, "name")?.to_owned();
            let arguments = required(item, "arguments")?;
            // A token-limited response is journaled verbatim, including truncated
            // arguments. The session must pair these calls with unexecuted results.
            if reason != StopReason::MaxTokens {
                serde_json::from_str::<Value>(arguments).map_err(|e| {
                    ProviderError::Protocol(format!("invalid Responses tool arguments: {e}"))
                })?;
                reason = StopReason::ToolCalls;
            }
            events.push(LlmEvent::ToolCallStart {
                call_id: call_id.clone(),
                name,
            });
            events.push(LlmEvent::ToolCallDelta {
                call_id,
                args_delta: arguments.to_owned(),
            });
        }
        let usage = &response["usage"];
        // Deliver the snapshot after all neutral calls so cancellation can
        // synthesize results for every call referenced by the stored output.
        events.push(LlmEvent::OpenAiOutput(output.clone()));
        events.push(LlmEvent::Finished {
            reason,
            usage: Usage {
                input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                cache_read_tokens: usage["input_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0),
                ..Usage::default()
            },
        });
        self.done = true;
        Ok(events)
    }
}

impl EventMapper for Mapper {
    fn map(&mut self, ev: SseEvent) -> Result<Vec<LlmEvent>, ProviderError> {
        if self.done || ev.data.is_empty() {
            return Ok(Vec::new());
        }
        let data: Value = serde_json::from_str(&ev.data)
            .map_err(|e| ProviderError::Protocol(format!("bad Responses event: {e}")))?;
        match data["type"].as_str().or(ev.event.as_deref()) {
            Some("response.output_text.delta" | "response.refusal.delta") => {
                Ok(vec![LlmEvent::TextDelta(
                    required(&data, "delta")?.to_owned(),
                )])
            }
            Some("response.completed") => self.response(&data["response"], StopReason::Stop),
            Some("response.incomplete")
                if data["response"]["incomplete_details"]["reason"] == "max_output_tokens" =>
            {
                self.response(&data["response"], StopReason::MaxTokens)
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                self.done = true;
                let error = match data["type"].as_str() {
                    Some("response.failed") => data["response"]["error"].clone(),
                    Some("response.incomplete") => {
                        json!({"message": "OpenAI response incomplete", "details": data["response"]["incomplete_details"]})
                    }
                    _ => data,
                };
                Err(ProviderError::Api {
                    status: 400,
                    body: error.to_string(),
                })
            }
            _ => Ok(Vec::new()),
        }
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, ProviderError> {
        if self.done {
            Ok(Vec::new())
        } else {
            Err(ProviderError::Protocol(
                "Responses stream ended before completion".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn event(value: &Value) -> SseEvent {
        SseEvent {
            event: None,
            data: value.to_string(),
        }
    }

    #[test]
    fn completed_tools_usage_and_opaque_reasoning_survive_replay() {
        let output = vec![
            json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "opaque"}),
            json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}"}),
            json!({"type": "function_call", "id": "fc_2", "call_id": "call_2", "name": "read", "arguments": "{\"path\":\"b\"}"}),
        ];
        let mut mapper = Mapper::default();
        assert_eq!(
            mapper
                .map(event(
                    &json!({"type":"response.output_text.delta", "delta":"checking"})
                ))
                .unwrap(),
            vec![LlmEvent::TextDelta("checking".into())]
        );
        let events = mapper.map(event(&json!({"type":"response.completed", "response": {
            "output": output, "usage": {"input_tokens":100, "output_tokens":20, "input_tokens_details":{"cached_tokens":80}}
        }}))).unwrap();
        assert_eq!(events[4], LlmEvent::OpenAiOutput(output.clone()));
        assert!(
            matches!(&events[0], LlmEvent::ToolCallStart { call_id, .. } if call_id == "call_1")
        );
        assert!(
            matches!(&events[2], LlmEvent::ToolCallStart { call_id, .. } if call_id == "call_2")
        );
        assert!(
            matches!(events.last(), Some(LlmEvent::Finished { reason: StopReason::ToolCalls, usage }) if usage.input_tokens == 100 && usage.output_tokens == 20 && usage.cache_read_tokens == 80)
        );
        assert!(mapper.finish().unwrap().is_empty());
        let req = CompletionRequest {
            model: "gpt-6-sol".into(),
            system: "system".into(),
            max_tokens: 8192,
            reasoning_effort: Some("medium".into()),
            tools: Vec::new(),
            messages: vec![
                Message::Assistant {
                    content: vec![
                        AssistantPart::Text {
                            text: "checking".into(),
                        },
                        AssistantPart::OpenAiOutput {
                            items: output.clone(),
                        },
                    ],
                },
                Message::ToolResult {
                    call_id: "call_1".into(),
                    content: "a".into(),
                },
                Message::ToolResult {
                    call_id: "call_2".into(),
                    content: "b".into(),
                },
            ],
        };
        let request = body(&req);
        assert_eq!(&request["input"].as_array().unwrap()[..3], output);
        assert_eq!(request["input"][3]["type"], "function_call_output");
        assert_eq!(request["input"].as_array().unwrap().len(), 5);
        assert_eq!(request["reasoning"]["effort"], "medium");
        assert_eq!(request["store"], false);
    }

    #[test]
    fn token_limit_preserves_raw_output_and_usage_for_recovery() {
        let output = vec![
            json!({"type":"reasoning", "id":"rs_1", "summary":[], "encrypted_content":"opaque"}),
            json!({"type":"function_call", "id":"fc_1", "call_id":"c1", "name":"write", "arguments":"{\"file_path\":\"wiki.md\",\"content\":\"partial", "status":"incomplete"}),
        ];
        let mut mapper = Mapper::default();
        let events = mapper.map(event(&json!({"type":"response.incomplete", "response": {
            "output":output, "incomplete_details":{"reason":"max_output_tokens"},
            "usage":{"input_tokens":100,"output_tokens":8192,"input_tokens_details":{"cached_tokens":80}}
        }}))).unwrap();
        assert_eq!(events[2], LlmEvent::OpenAiOutput(output));
        assert!(
            matches!(events.last(), Some(LlmEvent::Finished {reason:StopReason::MaxTokens,usage}) if usage.input_tokens == 100 && usage.output_tokens == 8192 && usage.cache_read_tokens == 80)
        );
        assert!(mapper.finish().unwrap().is_empty());
    }

    #[test]
    fn filtered_failed_and_truncated_streams_never_execute_tools() {
        for kind in ["response.incomplete", "response.failed", "error"] {
            let mut mapper = Mapper::default();
            assert!(mapper.map(event(&json!({"type":"response.output_item.added", "item":{"type":"function_call", "call_id":"c1", "name":"bash"}}))).unwrap().is_empty());
            assert!(mapper.map(event(&json!({"type":kind, "response":{"error":{"message":"failed"}, "incomplete_details":{"reason":"content_filter"}}}))).is_err());
            assert!(mapper.finish().unwrap().is_empty());
        }
        assert!(Mapper::default().finish().is_err());
    }
}
