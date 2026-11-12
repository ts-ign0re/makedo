//! OpenAI-compatible chat/completions (стриминг): `OpenAI`, `xAI`, `OpenRouter`, `Ollama`, `llama.cpp`.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::http::{EventMapper, event_stream, send_with_retries};
use crate::sse::SseEvent;
use crate::{
    AssistantPart, CompletionRequest, EventStream, LlmEvent, Message, Provider, ProviderError,
    StopReason, Usage,
};

const DEFAULT_BASE: &str = "https://api.openai.com/v1";

#[derive(Debug)]
pub struct OpenAiCompat {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    responses: bool,
}

impl OpenAiCompat {
    #[must_use]
    pub fn new(api_key: String, base_url: Option<String>, provider: &str) -> Self {
        let base_url = base_url.unwrap_or_else(|| DEFAULT_BASE.to_owned());
        // Native OpenAI uses Responses for reasoning + tools. Compatible endpoints
        // keep Chat Completions; an explicit provider also covers OpenAI proxies.
        let is_openai = provider == "openai"
            || reqwest::Url::parse(&base_url)
                .is_ok_and(|url| url.host_str() == Some("api.openai.com"));
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url,
            responses: is_openai,
        }
    }

    fn body(&self, req: &CompletionRequest) -> Value {
        if self.responses {
            return crate::responses::body(req);
        }
        let mut messages = vec![json!({"role": "system", "content": req.system})];
        for m in &req.messages {
            match m {
                Message::System { content } => {
                    messages.push(json!({"role": "system", "content": content}));
                }
                Message::User { content } => {
                    messages.push(json!({"role": "user", "content": content}));
                }
                Message::Assistant { content } => {
                    let mut text = String::new();
                    let mut tool_calls = Vec::new();
                    for p in content {
                        match p {
                            AssistantPart::OpenAiOutput { .. } => {}
                            AssistantPart::Text { text: t } => text.push_str(t),
                            AssistantPart::ToolCall {
                                call_id,
                                name,
                                input,
                            } => tool_calls.push(json!({
                                "id": call_id,
                                "type": "function",
                                "function": {"name": name, "arguments": input.to_string()},
                            })),
                        }
                    }
                    let mut msg = json!({"role": "assistant", "content": text});
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = Value::Array(tool_calls);
                    }
                    messages.push(msg);
                }
                Message::ToolResult { call_id, content } => {
                    messages.push(json!({
                        "role": "tool", "tool_call_id": call_id, "content": content
                    }));
                }
            }
        }
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": t.name, "description": t.description, "parameters": t.input_schema
                }})
            })
            .collect();
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "max_tokens": req.max_tokens,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(effort) = &req.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        body
    }

    /// `OpenRouter`: дефолтная балансировка прыгает между апстрим-провайдерами,
    /// а prompt-кэш живёт у апстрима — прыжок = cache miss = деньги (PLAN M3.5).
    /// `sort: price` отключает балансировку: стабильно самый дешёвый провайдер
    /// (cache-affinity + минимальная цена разом). Явный `provider` в модели
    /// (`:floor`/`:nitro`-суффиксы) пользователь задаёт сам — мы не трогаем.
    fn is_openrouter(&self) -> bool {
        self.base_url.contains("openrouter.ai")
    }
}

impl Provider for OpenAiCompat {
    fn stream(&self, req: CompletionRequest) -> EventStream {
        let endpoint = if self.responses {
            "responses"
        } else {
            "chat/completions"
        };
        let url = format!("{}/{endpoint}", self.base_url.trim_end_matches('/'));
        let client = self.client.clone();
        let key = self.api_key.clone();
        let mut body = self.body(&req);
        if self.is_openrouter() && !req.model.contains(':') {
            body["provider"] = json!({"sort": "price"});
        }
        let fut = Box::pin(async move {
            send_with_retries(|| client.post(&url).bearer_auth(&key).json(&body)).await
        });
        if self.responses {
            event_stream(fut, crate::responses::Mapper::default())
        } else {
            event_stream(fut, Mapper::default())
        }
    }
}

#[derive(Debug, Default)]
struct Mapper {
    /// index → `call_id` (дельты аргументов приходят по индексу)
    calls: HashMap<u64, String>,
    finish: Option<StopReason>,
    usage: Usage,
    done: bool,
}

impl EventMapper for Mapper {
    fn map(&mut self, ev: SseEvent) -> Result<Vec<LlmEvent>, ProviderError> {
        if ev.data == "[DONE]" {
            self.done = true;
            return Ok(vec![LlmEvent::Finished {
                reason: self.finish.unwrap_or(StopReason::Other),
                usage: self.usage,
            }]);
        }
        if ev.data.is_empty() || self.done {
            return Ok(vec![]);
        }
        let data: Value = serde_json::from_str(&ev.data)
            .map_err(|e| ProviderError::Protocol(format!("bad chunk json: {e}")))?;
        if let Some(u) = data.get("usage").filter(|u| !u.is_null()) {
            // cached-токены: OpenAI кладёт в prompt_tokens_details.cached_tokens,
            // DeepSeek — в prompt_cache_hit_tokens (оба формата встречаются в дикой природе)
            let cached = u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .or_else(|| u["prompt_cache_hit_tokens"].as_u64())
                .unwrap_or(0);
            self.usage = Usage {
                input_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
                cache_read_tokens: cached,
                cache_write_tokens: 0,
                cost_usd: None,
            };
        }
        let mut out = Vec::new();
        let Some(choice) = data["choices"].get(0) else {
            return Ok(out);
        };
        if let Some(r) = choice["finish_reason"].as_str() {
            self.finish = Some(match r {
                "stop" => StopReason::Stop,
                "tool_calls" => StopReason::ToolCalls,
                "length" => StopReason::MaxTokens,
                _ => StopReason::Other,
            });
        }
        let delta = &choice["delta"];
        if let Some(t) = delta["content"].as_str()
            && !t.is_empty()
        {
            out.push(LlmEvent::TextDelta(t.to_owned()));
        }
        if let Some(tcs) = delta["tool_calls"].as_array() {
            for tc in tcs {
                let index = tc["index"].as_u64().unwrap_or(0);
                if let Some(id) = tc["id"].as_str() {
                    let name = tc["function"]["name"].as_str().unwrap_or_default();
                    self.calls.insert(index, id.to_owned());
                    out.push(LlmEvent::ToolCallStart {
                        call_id: id.to_owned(),
                        name: name.to_owned(),
                    });
                }
                if let (Some(call_id), Some(args)) =
                    (self.calls.get(&index), tc["function"]["arguments"].as_str())
                    && !args.is_empty()
                {
                    out.push(LlmEvent::ToolCallDelta {
                        call_id: call_id.clone(),
                        args_delta: args.to_owned(),
                    });
                }
            }
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, ProviderError> {
        if self.done {
            Ok(vec![])
        } else {
            Ok(vec![LlmEvent::Finished {
                reason: self.finish.unwrap_or(StopReason::Other),
                usage: self.usage,
            }])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_is_sent_once_and_omitted_when_unconfigured() {
        let provider = OpenAiCompat::new(
            String::new(),
            Some("http://localhost:8000/v1".into()),
            "openai-compat",
        );
        let mut req = CompletionRequest {
            model: "qwen3.8".into(),
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: 8192,
            reasoning_effort: None,
        };
        assert!(provider.body(&req).get("reasoning_effort").is_none());
        for value in ["low", "medium", "xhigh"] {
            req.reasoning_effort = Some(value.into());
            let body = provider.body(&req);
            assert_eq!(body["reasoning_effort"], value);
            assert!(body.get("chat_template_kwargs").is_none());
        }
    }

    #[test]
    fn token_limit_uses_provider_contract_not_model_name() {
        for (kind, base, model, completion_tokens) in [
            ("openai", None, "gpt-6-sol", true),
            (
                "openai",
                Some("http://localhost:8000/v1"),
                "gpt-6-sol",
                true,
            ),
            (
                "openai-compat",
                Some("https://api.openai.com/v1/"),
                "gpt-6-sol",
                true,
            ),
            ("openai", None, "gpt-4.1", true),
            (
                "openrouter",
                Some("https://openrouter.ai/api/v1"),
                "openai/gpt-6-sol",
                false,
            ),
            ("xai", Some("https://api.x.ai/v1"), "grok-4", false),
            (
                "deepseek",
                Some("https://api.deepseek.com/v1"),
                "deepseek-chat",
                false,
            ),
            (
                "ollama",
                Some("http://localhost:11434/v1"),
                "gpt-6-sol",
                false,
            ),
            (
                "openai-compat",
                Some("http://localhost:8000/v1"),
                "qwen3.8",
                false,
            ),
            (
                "openai-compat",
                Some("https://api.openai.com.example/v1"),
                "gpt-6-sol",
                false,
            ),
        ] {
            let provider = OpenAiCompat::new(String::new(), base.map(str::to_owned), kind);
            let req = CompletionRequest {
                model: model.into(),
                system: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
                max_tokens: 8192,
                reasoning_effort: None,
            };
            let body = provider.body(&req);
            let (present, absent) = if completion_tokens {
                ("max_output_tokens", "max_tokens")
            } else {
                ("max_tokens", "max_output_tokens")
            };
            assert_eq!(body[present], 8192, "{kind}: {base:?}");
            assert!(body.get(absent).is_none(), "{kind}: {base:?}");
            assert!(body.get("max_completion_tokens").is_none());
            assert_eq!(body["model"], model);
        }
    }

    fn ev(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_owned(),
        }
    }

    #[test]
    fn maps_tool_call_deltas_by_index() {
        let mut m = Mapper::default();
        let out = m
            .map(ev(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"grep","arguments":""}}]}}]}"#,
            ))
            .unwrap_or_default();
        assert!(matches!(&out[0], LlmEvent::ToolCallStart { name, .. } if name == "grep"));
        let out = m
            .map(ev(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":1}"}}]}}]}"#,
            ))
            .unwrap_or_default();
        assert!(
            matches!(&out[0], LlmEvent::ToolCallDelta { call_id, args_delta }
            if call_id == "c1" && args_delta == "{\"q\":1}")
        );
        m.map(ev(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#,
        ))
        .unwrap_or_default();
        let out = m.map(ev("[DONE]")).unwrap_or_default();
        assert_eq!(
            out,
            vec![LlmEvent::Finished {
                reason: StopReason::ToolCalls,
                usage: Usage {
                    input_tokens: 7,
                    output_tokens: 3,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    cost_usd: None,
                }
            }]
        );
        assert!(m.finish().is_ok_and(|v| v.is_empty()));
    }

    #[test]
    fn text_deltas() {
        let mut m = Mapper::default();
        let out = m
            .map(ev(r#"{"choices":[{"delta":{"content":"hel"}}]}"#))
            .unwrap_or_default();
        assert_eq!(out, vec![LlmEvent::TextDelta("hel".into())]);
    }
}
