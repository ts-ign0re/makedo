# Plan: reasoning modes of the connected model

Date: 2026-10-08. Status: partly implemented. `/effort` selects a level, saves it for the
model and endpoint, and passes `reasoning_effort` in OpenAI-compatible requests.
The reasoning stream and its history are still to be done.

## Goal

In makedo the user chooses only from the reasoning modes that the specific model
supports on the specific endpoint. The chosen value applies from the next
request and is shown in the TUI. For Swift 1.5 / Qwen3.8-27B the modes are
`low`, `medium`, `xhigh`; the initial value is `low`. The value `high` is not
a synonym for `xhigh` and is not valid for this model.

Turning thinking off is a separate model capability, not an extra level:
for Qwen it is passed as `chat_template_kwargs.enable_thinking=false`.
Hiding reasoning in the interface must not turn off its generation.

## State before implementation

- `crates/makedo-config/src/lib.rs`: `ProviderConfig` stores kind, model, base_url,
  api_key_env; there are no reasoning settings.
- `crates/makedo-providers/src/catalog.rs`: `ModelInfo` holds limits, prices and tools,
  but not the supported modes or the way to pass them.
- `crates/makedo-providers/src/lib.rs`: `CompletionRequest`, `AssistantPart` and
  `LlmEvent` do not represent reasoning.
- `crates/makedo-providers/src/openai.rs`: the request does not pass reasoning; the stream
  handles content and tool_calls but skips reasoning/reasoning_content.
- `crates/makedo-session/src/lib.rs`: `turn` builds the request; `switch_model`
  creates a ProviderConfig with `base_url=None` and `api_key_env=None`. Switching
  a custom model this way can lose the Vast address and the token source.
- `crates/makedo-tui/src/{commands,app,lib}.rs`: there is a model picker but no
  mode picker. `src/main.rs` builds SessionConfig at startup.

## 1. Model capabilities and config

Add a small reasoning profile tied to provider + endpoint + model:

- a list of the exact string identifiers of the modes;
- the default and the currently selected mode;
- how the value is passed: for the current vLLM, `reasoning_effort` at the root of the request;
- whether thinking can be turned off, and the parameter that does it;
- whether reasoning is kept in the history, if the model requires it.

Profile source: explicit user config > a verified profile for the specific
model/endpoint > trustworthy provider metadata, where available. The vLLM
`GET /v1/models` endpoint usually does not list reasoning modes: do not try to guess
them from the model name or from the generic OpenAI-compatible API. If the profile is
unknown, do not show a made-up list and do not send a reasoning parameter until it is configured.

For Vast, add an explicit Swift profile with `low / medium / xhigh`, default `low`.
Take the token from env or the credentials store; do not put it in the plan, git or logs.

## 2. End-to-end passing and validation

Thread the selected mode through Config → SessionConfig → CompletionRequest →
OpenAiCompat::body. Validate it against the profile before the network request.
For the current vLLM use a single source for the value, `reasoning_effort`;
do not leave a conflicting `reasoning_effort` in `chat_template_kwargs`.

When the model changes, keep the selection only if the new profile supports it;
otherwise apply the new profile's default and show a notice. Keep the
base_url and api_key_env of the selected custom provider. A mode change applies
to the following requests; do not automatically restart a generation that is already running.
A retry after an error uses the same validated mode.

Do not put Qwen parameters into the Anthropic/Gemini/CLI delegates. Their profiles and
adapters are added according to the API each one actually supports; the shared schema
does not define a universal low/medium/high enumeration for all models.

## 3. Interface and history

- `/effort` is implemented: the picker is built from the current model's profile;
  `/effort <level>` sets the level directly.
- Show the current mode next to the model name; Qwen shows `xhigh`.
- Remember the selection for the specific endpoint/model, including across session restore.
- Add a ReasoningDelta event and a separate reasoning block in the history/TUI.
- When the conversation continues, pass reasoning in the field that the model's API
  expects, including in messages with tool_calls; do not mix it with the final content.
- Keep old sessions compatible: missing reasoning is allowed.
- Show reasoning tokens separately only if the server reports them reliably;
  do not present the text length in characters as a token count.

## 4. Checks during implementation

1. The Qwen config and picker contain exactly `low`, `medium`, `xhigh`; `high`
   is rejected locally with a list of the allowed values.
2. A local HTTP listener records the selected value in the JSON for each
   mode. Check the real outgoing request, not only the TUI state.
3. Changing the model does not carry over an unsupported mode and does not lose the custom endpoint.
4. The reasoning stream, the answer, the tool_call and the next turn are saved
   correctly and restored from the history. Old sessions can still be read.
5. On Vast the server accepts all three allowed modes; a short coding
   task and a tool call both complete. Do not use the thinking-token count of
   a single answer as proof that a mode is effective.

## Rollout order

1. Swift profile + config + request serialization + local check of the parameter.
2. TUI picker and keeping the selection across model switches and session restore.
3. Reasoning stream and history, then a short check against the real Swift API.
4. Profiles for the other models that can be connected, based on their documentation.

Definition of done: selecting `xhigh` in makedo reaches Swift as `xhigh`, selecting
`high` is impossible for this model, and the user sees the mode that is actually
applied. Managing the instance and deleting its disk after 17:00 on October 9 remain
the job of `qwen_vast.py`, separate from makedo user sessions.

## Sources

- [Swift 1.5](https://huggingface.co/ukisai/Swift-1.5-Qwen3.8-27b)
- [The selected uncensored model](https://huggingface.co/ajgazin/Swift-1.5-Qwen3.8-27B-Uncensored-MTP)
- [Qwen3.8 template](https://huggingface.co/huihui-ai/Huihui-Qwen3.8-27B-abliterated/raw/main/chat_template.jinja)
- [OpenCode 2.0.20 variants schema](https://github.com/anomalyco/opencode/blob/v2.0.20/packages/schema/src/config/provider.ts)

A check of the installed OpenCode 2.0.20 with a local HTTP listener confirmed that
`#low` sends `reasoning_effort=low` and `#high` sends `reasoning_effort=high`.
OpenCode supports custom variants; the problem with our original config was that it
had no exact list of Qwen modes and duplicated the value in two fields.

The corrected OpenCode config was checked with the same listener: `low`, `medium` and
`xhigh` are each sent as the `reasoning_effort` of the same name, and the conflicting
field in `chat_template_kwargs` is gone. The default is set to `low`.
