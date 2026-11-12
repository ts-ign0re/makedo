# Справочник / Reference

Поведение агента в деталях. Часть разделов на русском, часть на английском.

## Провайдеры и модели

Каталог моделей обновляется командой `makedo models --refresh` и автоматически
раз в час. Пользовательские OpenAI-compatible провайдеры (например, Qwen на
Vast) задаются в `~/.makedo/models.json` (`$MAKEDO_HOME/models.json` при override):

```json
{
  "qwen-vast": {
    "api": "http://YOUR_HOST:PORT/v1",
    "env": ["QWEN_VAST_TOKEN"],
    "models": {
      "YOUR_SERVED_MODEL_ID": {
        "context": 0, "output": 0,
        "in": 0, "out": 0, "tools": true
      }
    }
  }
}
```

Укажите ID из `/v1/models` и реальные лимиты сервера; `0` означает неизвестный
лимит. Эта запись заменяет одноимённого провайдера каталога и сохраняется при
обновлении. Провайдер появится в `/models` после запуска makedo; токен вводится
там же или берётся из указанной переменной/`~/.makedo/credentials.toml`.

Для `provider.kind = "openai"` (включая свой `base_url`) и адреса
`api.openai.com` используется Responses API с `max_output_tokens`: reasoning
работает вместе с инструментами, его зашифрованное состояние сохраняется при
`--resume`. Запросы идут с `store: false`. Остальные совместимые провайдеры
используют Chat Completions с `max_tokens`.

### Подписка ChatGPT

Подписка ChatGPT: `makedo login chatgpt` открывает вход OpenAI в браузере (OAuth с
PKCE, ответ приходит на `127.0.0.1`). После входа `provider.kind = "chatgpt"`
работает через Responses API в счёт квоты плана — собственным циклом Makedo, со
всеми его инструментами, в отличие от делегата `codex-cli`. `makedo login chatgpt
<model>` сразу делает модель текущей; доступные модели видны в `/models`. Токены
лежат в `~/.makedo/chatgpt.json` (права 0600) и обновляются автоматически; чтобы
сменить аккаунт, удалите этот файл и войдите заново. `max_output_tokens` на этом
маршруте не отправляется: OpenAI его там не принимает. Кэш префикса на подписке
не действует (сервер сообщает ноль записанных и прочитанных токенов), поэтому
каждый шаг расходует квоту полностью; долларовая цена для неё не показывается.

### Лимит ответа

При исчерпании лимита ответа Makedo сохраняет полученный контекст и продолжает
работу автоматически (до двух попыток), увеличивая бюджет ответа в пределах
лимитов модели и окна контекста. Вызовы инструментов из оборванного ответа
не исполняются: агент получает их статус и формирует полные вызовы заново.

### Уровень reasoning

`/effort` открывает выбор уровня reasoning; `/effort xhigh` задаёт его напрямую.
Уровень отображается рядом с моделью, действует со следующего запроса и
сохраняется в `~/.makedo/effort.json` отдельно для провайдера, endpoint и модели.
Модель по умолчанию при этом не меняется.

Для проверенного OpenAI-compatible endpoint профиль добавляется в его запись
в `~/.makedo/models.json`, рядом с `api`, `env` и `models`. Например, для Swift
1.5 / Qwen3.8 на Vast:

```json
"effort": {
  "qwen3.8": { "levels": ["low", "medium", "xhigh"], "default": "low" }
}
```

Без явного профиля параметр `reasoning_effort` не отправляется. Уровни не
угадываются по имени модели; `high` не является заменой `xhigh` для этого Qwen.

## Очередь сообщений

Во время работы агента Enter ставит сообщение в очередь: оно сразу видно в
ленте с отметкой `queued`. Перед следующим запросом к модели отметка снимается,
сообщение не дублируется. Ctrl+Enter прерывает текущий шаг и передаёт агенту
очередь вместе с новым текстом. Ctrl+Enter с пустым полем отправляет уже
накопленную очередь. Результаты завершённых инструментов сохраняются;
прерываемые вызовы получают результат отмены перед продолжением работы.

## Versioned file editing

`read` returns a SHA-256 `version` of the entire file, including when reading a
line range. Existing files require that version in `edit.expected_version` or
`write.expected_version`. Without a version, `write` only creates missing files.
Successful writes return the new version; a conflict requires another read.

`apply_patch` accepts a `patch` string with `*** Begin Patch` / `*** End Patch`,
`*** Add File: path`, `*** Update File: path` with `@@` context hunks, and
`*** Delete File: path`. Pass `expected_versions: {"path": "sha256:…"}` for
every update/delete. Matching is exact; ambiguous context is rejected. Moves
use add + delete. Deletion through a symlink requires its resolved path explicitly.
Updates preserve LF/CRLF and the final newline convention;
use `edit` for mixed line endings.

Native writes share per-path OS locks across sessions/processes of the same user
(`~/.makedo/file-locks`, independent of `MAKEDO_HOME`). Different files proceed
independently. Temporary files are synced and published atomically on the same
filesystem; existing permission bits and valid symlinks are preserved. New files
start with private permissions (0600). Readers
see the old or new file. All patch versions/hunks are validated before staging.
A multi-file patch is atomic per file, not a transaction: a later I/O failure
reports already-applied paths. Advisory locks cannot serialize external editors
or shell scripts; use worktrees for independent tasks with overlapping edits.

## Execution logs, Heavy and embedded skills

Shell commands write complete stdout/stderr directly to unique files under
`.makedo/spill/commands/` (or the project's external MAKEDO data directory). The
model receives `OK`/`FAIL`, exit status, elapsed time and a log path. On failure
it searches the log and reads the relevant lines. For commands whose output is
the requested data, such as `git diff`, use `bash` with `output="content"`;
its preview is bounded to 8000 bytes. Shell calls are foreground operations;
detached children that outlive the shell are terminated and reported as a failure. `/logs` displays complete artifact paths.
Verification uses the same runner, retains logs on cancellation/timeout and
supports cancellation while checks are running.

Status-mode commands and verification automatically use the installed `heavy`
binary for machine-wide slots. `heavy=false` opts a cheap command out;
`heavy=true` also applies slots to content-mode commands. The existing
`HEAVY_*` and CI behavior belongs to Heavy. If Heavy is absent, the result says
`heavy=unavailable`; MAKEDO's own per-batch concurrency limit still applies.

Ponytail, Open Code Review, its delegate mode, Heavy and worktree guidance are embedded
in the binary. `skill_search` finds a skill and `skill_load` loads its instructions
on demand. Only Ponytail (full by default) and a selected doctrine are always
included in the system prompt. Global Claude/Codex/plugin skill directories are
not read at runtime. `.makedo/skills/<name>/SKILL.md` can add project skills or
override a built-in; disabling Ponytail also disables a project override.
`ocr` remains an external executable with its existing provider configuration.

Text submitted during a turn is queued and delivered between model steps. Esc
cancels the active turn and its still-pending messages. Queued input and its
acceptance are recorded in the shared project SQLite database, including subagent
transcripts. `makedo --resume latest`, `makedo --resume <session-id>` or a legacy
JSONL path restores history into a new session and preserves the original.
Completed tool calls are not replayed; interrupted
calls are marked for inspection before any retry. This is recovery, not an
exactly-once guarantee for external side effects. `/clear` clears the screen,
while keeping model context.

Project memory lives at `<git-common-dir>/makedo/memory.sqlite`, shared by all
worktrees. `memory_search`, `memory_read` and `memory_write` expose ranked facts
and conversation excerpts through native tools and `makedo mcp-serve`. Retrieval
combines FTS5 with cached multilingual embeddings from local Ollama
(`ollama pull embeddinggemma`; run `ollama serve`). If the embedding service is
unavailable, results explicitly report `lexical_only` and a setup hint.
`makedo memory status|search|read|write|export` provides the same access from the CLI.
Notes retain revisions, corrections and independently observed merge/check
evidence; relevance scores are not probabilities of correctness.
See [memory behavior and configuration](MEMORY.md).

Details: [upstream integration](UPSTREAM.md), [independent UI audit](UI-AUDIT.md).

`makedo skills` lists the built-in catalog; `makedo skills <name>` prints one skill.
`/ponytail [lite|full|ultra|off]` changes intensity for the session without an LLM call.
Worktree ownership and integration rules: [WORKTREES.md](WORKTREES.md).
Finish uses Git >= 2.38 for merge preview, requires a clean parent on the recorded
base branch, and retains worktrees containing runtime logs/history.

## Agent progress

The main plan is a horizontal line above the input. Subagents appear in a right
panel with `name │ elapsed` and an indented horizontal plan. Plans come from
`todo_write`; tool completions appear as they finish, independently of batch order.
No additional model request is needed to render progress.

F6 cycles agent details (activity, worktree, latest log), F7 or `/agents` toggles
the panel. Below 114 terminal columns it collapses to a compact summary. `/logs`
also includes subagent log paths. `makedo demo` previews the production interface
with labelled fixture events and no model requests or commands.

## YOLO in chat

Send `YOLO` on its own, or include it as a word anywhere in a prompt, to grant
all tool permissions for the current session. Matching ignores case. A standalone
`YOLO` switches the mode locally without a model request; a prompt containing
`YOLO` still runs normally. The input border shows `YOLO` once enabled.

This also works while a tool is waiting for approval: type `YOLO` and press Enter.
In that dialog `y` + Enter approves once; `a`, `A` and `n` retain their shortcuts.
`/yolo`, `/allow-all` and `--yolo` enable the same mode. Repeated-call approval
guards do not interrupt YOLO. The mode is session-only; restored transcripts,
files and tool output do not activate it.

While a turn is running, the status uses a one-cell spinner and a compact elapsed
time (for example `⠋ bash · 9s`). Text is muted; parallel tools and waiting calls
add short counts. The completed-turn token/cache/cost summary is unchanged.
