# MAKEDO — план разработки

Rust coding-agent harness для машин 4–6-летней давности (8 GB RAM, 4 ядра, без AVX2-гарантий).
Референсный документ: каждое решение либо обосновано, либо помечено **[открытый вопрос]**.
База: анализ grok-build (Rust, xAI) и opencode (TS/Bun) — берём проверенное, чиним найденные баги, не тащим лишнее.

---

## 1. Принципы проектирования

### 1.1 Каноны строгого Rust

**Edition / MSRV.** Edition 2024. MSRV = stable − 2 релиза, зафиксирован в `rust-toolchain.toml` и проверяется в CI. Обоснование: RPITIT и async fn in traits нужны (≥1.75), гнаться за nightly незачем; «−2» даёт дистрибутивам и контрибьюторам люфт.

**Lint-политика** (workspace-уровневая, `[workspace.lints]`):

```toml
[workspace.lints.rust]
unsafe_code = "deny"          # весь workspace; исключение — только отдельным крейтом с обоснованием в README крейта
missing_debug_implementations = "warn"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }
unwrap_used = "deny"          # в lib-крейтах; тесты — allow
expect_used = "deny"
panic = "deny"
dbg_macro = "deny"
todo = "deny"
large_enum_variant = "deny"   # прямая RAM-метрика
```

Из pedantic точечно `allow`: `module_name_repetitions`, `must_use_candidate`, `missing_errors_doc` (на старте). `nursery` — не включаем скопом (нестабильные false-positives), берём точечно: `significant_drop_tightening` (прямо ловит «guard через .await» — класс багов, задокументированный в grok-build).

`clippy.toml`:
- `disallowed-methods`: `tokio::sync::mpsc::unbounded_channel` (бэкпрешер обязателен — grok-build использует unbounded mpsc для результатов тулов, мы запрещаем), `std::thread::sleep` (в async-коде), `std::process::exit` (кроме bin).
- `disallowed-types`: `std::sync::Mutex` в async-крейтах (либо `parking_lot` с правилом «не держать через .await», либо `tokio::sync`).
- `cognitive-complexity-threshold = 25`.

Обоснование урока: в grok-build security-критичный крейт sandbox несёт `#![allow(dead_code, unreachable_code, ...)]` — у нас per-file allow запрещён ревью-политикой, только per-expression с комментарием.

**Error handling по слоям.**
- Lib-крейты: `thiserror`, типизированные enum-ошибки на границе крейта. Ошибка, видимая модели (текст в tool result), — отдельный тип/поле, не `Display` внутренней ошибки: то, что читает LLM, — это API, его нельзя случайно менять рефакторингом (урок opencode: `InvalidArgumentsError` с model-facing текстом — правильный паттерн).
- `bin`/TUI-верхушка: `anyhow` допустим.
- Паники: запрещены на рантайм-путях (lint выше). `panic = "abort"` в релизе ⇒ любая паника = смерть процесса, значит паника — это только «инвариант программиста нарушен», не обработка ввода.

**Инварианты через типы.** Newtype-и вместо `String`/`usize` там, где перепутывание — реальный баг:
- `QualifiedToolName` — конструктор валидирует ровно одну границу `__` и регекс `^[A-Za-z_][A-Za-z0-9_-]{0,63}$` (строжайший общий знаменатель провайдеров, из grok-build). Это типом чинит два найденных бага: `contains("__")` как признак MCP-тула и молчаливые коллизии имён в opencode.
- `ToolId`, `ServerName`, `WikiNodeName`, `EdgeKind`, `ByteBudget`, `TokenEstimate`.
- Каналы: только bounded, ёмкость — const с комментарием-обоснованием.

**Чек за лень (ponytail-дисциплина).** Нетривиальная логика (парсер, лок-схема, стейт-машина) обязана оставить минимальный runnable-тест. Сознательные упрощения помечаются `// ponytail: <потолок и путь апгрейда>` — и собираются в ledger.

### 1.2 Принципы производительности под старое железо

**Принцип №1: makedo знает, на чём работает.** При старте детектится RAM/cores/disk (`makedo_types::resources`, алгоритмически, без зависимостей) и из них выводится единый `Budget`: потолок параллельных тулов, capture-cap, blocking-пул. <8GB RAM = «тесный» режим (жмёмся сильнее). Экономия токенов — тем же принципом: ponytail-скилл (меньше кода = меньше токенов) и LLM wiki (меньше поисковых итераций), причём **wiki ведётся в ponytail/caveman-стиле** — телеграфные ноды без прозы: каждый сэкономленный токен экономится при каждом будущем чтении.

**Бюджеты — это CI-гейты, не пожелания** (числа — стартовые, уточняются после первого замера на референс-машине; референс: MacBook Pro 2019 i5-8257U / 8 GB и Linux-ноутбук того же класса):

| Метрика | Бюджет |
|---|---|
| Холодный старт → первый кадр TUI | < 150 мс |
| RSS в простое (TUI открыт, сессия пустая) | < 50 MB |
| RSS в типовой сессии (без учёта child-процессов) | < 300 MB |
| Детерминированный wiki-seed, репо 10k файлов | < 5 с |
| `search_tool` p99 | < 5 мс |
| Бинарь на таргет (stripped) | < 15 MB |
| Чистая сборка CI | < 5 мин; инкрементальная локально < 15 с |

**Правила аллокаций и данных:**
- Большое — на диск, не в RAM: вывод тулов сверх лимита спиллится в файл (обе кодовые базы так делают; берём и «рулящие» подсказки grok-build: JSON → «используй jq»).
- Стриминг: rolling window для вывода bash (модель opencode: 2×лимит в памяти, дальше — файловый sink); дельты в TUI — только append, cap на кадр 16 KiB (модель grok-build, включая UTF-8-safe границы).
- `Cow<'_, str>` fast-path в трункации (паттерн grok-build), `with_capacity` при известном размере, стартовая ёмкость буферов ≤ 64 KiB даже при больших потолках (паттерн grok-build grep).
- Никаких полных клонов снапшотов на запрос (анти-паттерн: BM25 в grok-build клонирует весь каталог схем на каждый `search_tool`) — индексы кэшируются по generation.
- Никакой пересериализации мира после каждого тул-колла (анти-паттерн: `Resources::serialize` в grok-build) — персистится только изменившийся компонент, dirty-флагами.

**Планировщик.** tokio, `worker_threads = min(4, cores)`, `max_blocking_threads` ограничен (по умолчанию 512 — на 8 GB это потенциальные гигабайты стеков). `spawn_blocking` — только для реального блокирующего I/O (walk файловой системы, git-команды); CPU-задачи > 1 мс (BM25-построение, парсинг vault) — тоже туда, чтобы не давить реактор. Поллинг запрещён: `Notify`/`watch` вместо sleep-циклов (анти-паттерн: 50 мс interjection-поллинг в grok-build).

**Политика зависимостей.** Каждый dep — строка в `docs/DEPS.md`: зачем, что отвергнуто, стоимость (transitive count, вклад в бинарь по `cargo bloat`). Метрики в CI: количество transitive deps (гейт: рост требует обоснования в PR), `cargo deny` (лицензии + advisories). Feature-минимализм: `default-features = false` всюду, включаем поимённо.

---

## 2. Архитектура workspace

```
makedo/
├── crates/
│   ├── makedo-types      # общий словарь: newtype-и, события, ошибки-границы. Зависит только от serde
│   ├── makedo-config     # TOML, merge global→project, схема ~/.makedo
│   ├── makedo-tools      # трейт Tool, реестр, встроенные тулы, трункация, спиллы
│   ├── makedo-wiki       # парсер vault, граф, BM25, init/seed, check
│   ├── makedo-skills     # SKILL.md-лоадер, embedded ponytail
│   ├── makedo-providers  # трейт Provider, Anthropic + OpenAI-compat, SSE-декодер
│   ├── makedo-mcp        # rmcp-клиент, lifecycle, meta-tools
│   ├── makedo-session    # актор сессии, agent loop, permissions, компакция
│   └── makedo-tui        # ratatui-рендер, спиннеры, темы
└── src/main.rs         # bin `makedo`: CLI-парсинг, wiring, anyhow
```

**Направление зависимостей** (строго вниз, циклы = ошибка сборки):

```
makedo-types ← makedo-config ← {tools, wiki, skills, providers} ← makedo-mcp ← makedo-session ← makedo-tui ← bin
```

- `makedo-mcp` зависит от `makedo-tools` (MCP-тулы регистрируются в общий реестр; BM25-движок берёт из `makedo-wiki`? — нет: **BM25 живёт в `makedo-types` как маленький модуль без зависимостей**, его используют и wiki, и mcp. ~150 строк, общий).
- `makedo-tui` не знает про providers/mcp напрямую — только события сессии (`makedo-types::Event`).
- Public API крейтов минимален: `pub(crate)` по умолчанию, `pub` — осознанно. `#[non_exhaustive]` на событиях и конфиг-структурах.

**Трейты-швы — только там, где вторая реализация реальна:**
- `Provider` (реализаций ≥ 2 с первого дня).
- `TokenCounter` (bytes/4 сейчас, BPE потом — шов из grok-build, который они построили и не использовали; мы хотя бы не заплатим за него лишним, это один trait-object в session).
- `ToolDispatch` (object-safe граница для рекурсии use_tool → реестр).
Всё остальное — конкретные типы. Никаких «interface с одной реализацией».

---

## 3. Ключевые подсистемы

### 3.1 Tool runtime (`makedo-tools`)

**Дизайн.** Типизированный трейт по образцу grok-build (лучшее, что есть в обеих кодовых базах):

```rust
trait Tool: Send + Sync {
    type Args: DeserializeOwned + JsonSchema + Send;
    type Output: ToolOutput + Send;
    fn id(&self) -> ToolId;
    fn description(&self) -> &str;
    fn is_read_only(&self) -> bool;
    async fn run(&self, ctx: &ToolCtx, args: Self::Args) -> Result<Self::Output, ToolError>;
}
```

RPITIT на типизированном пути (ноль боксинга), `#[async_trait]`-эквивалент только на erasure-границе `ToolDyn` (blanket impl делает JSON-декод + вызов). Прогрессив-стриминг (`ToolStream` с инвариантом `[Progress*, Terminal]`) — **отложен**: в M0 тулы возвращают один результат, стриминг вывода — только у bash через его собственный канал. `// ponytail: single-shot tool results; ToolStream когда появится второй тул с прогрессом`.

**Схемы.** `schemars` draft-07, `inline_subschemas = true` (обе базы инлайнят `$ref` — провайдеры их не переваривают), плюс LLM-нормализации из opencode: убрать `null` из anyOf опциональных полей, схлопнуть одноэлементные anyOf, гарантировать `type/properties/required`. Генерация схем — один раз при построении реестра, кэш по `ToolId`.

**Реестр.** `IndexMap<ToolId, Arc<dyn ToolDyn>>` (порядок = порядок объявления, O(1) lookup — чиним линейный скан по Vec из grok-build). Тулы модели отдаются в **алфавитном порядке имён** (opencode: стабильность prompt-кэша). Runtime-мутация (hot-add MCP-тулов) — через RwLock, guard никогда не живёт через `.await` (инвариант grok-build, у нас — под lint `significant_drop_tightening`).

**Параллелизм.** То, чего нет ни у кого:
- `Semaphore` с `min(4, cores)` пермитов на батч тул-коллов (opencode вообще без потолка: 30 параллельных bash = 30 процессов; на 8 GB это смерть).
- Per-path write-локи из grok-build: пути из аргументов не-read-only коллов батча получают `Mutex`; захват — **в отсортированном порядке путей** (grok лочит один путь на колл; если колл затрагивает несколько — сортировка исключает deadlock).
- Permission-фаза — последовательная до батча, отказ синтезирует tool_result для остальных (пара tool_use/tool_result не рвётся — инвариант, ради которого grok это сделал).

**Отмена.** `CancellationToken` реально прокинут в каждый `ToolCtx` (grok построил и не подключил — у нас подключение = часть DoD M0): отмена турна → cancel token → тулы с процессами убивают children (SIGTERM → 3 с → SIGKILL, процесс-группа через setsid, паттерн grok-build), остальные — drop future. Reconciliation: после отмены каждому незавершённому коллу — синтетический `interrupted`-результат (модель opencode, 250 мс grace).

**Вывод.** Лимит по умолчанию 40 KB / 2000 строк (сходится у обеих баз ≈ 10k токенов); сверх — спилл в `~/.makedo/spill/<session>/<call>.{txt,json}` + классификация содержимого и рулящая подсказка (JSON → jq, длинные строки → bash-слайсинг; из grok-build) + agent-aware вариант (из opencode). Ретенция спиллов 7 дней, фоновая уборка.

**Встроенные тулы M0:** `read` (лимиты: 2000 строк, 2000 симв/строка), `write`, `edit` (string-replace), `bash` (fg/bg, таймаут дефолт 120 с, потолок 10 мин, rolling-window вывод), `grep`, `glob`, `todo`. Затем: `webfetch` (cap 5 MB, таймаут 30 с), wiki-тулы (§3.3), meta-tools MCP (§3.2). Отложено: task/subagents, lsp, websearch, медиа.

**Permissions.** Модель opencode — упорядоченный ruleset `{permission, pattern, action: allow|ask|deny}`, последнее совпадение побеждает, дефолт ask; сессионные «always»-правила авторезолвят ожидающие запросы. Doom-loop-guard (N одинаковых коллов подряд → принудительный ask) — дёшев и полезен, берём. LLM-классификатор Auto-режима из grok-build — **отложен** (YAGNI: ещё один LLM-вызов на каждый bash — дорого для нашей аудитории). Разбор bash-команды для префиксов: консервативный сплит по `;|&&`, без tree-sitter в M0. **[открытый вопрос]** tree-sitter-bash (точность) vs самописный сплит (0 зависимостей): решаем по фактическим false-positive после dogfood.

**Риски:** semaphore + per-path локи + permission-очередь — три механизма ожидания; тестом фиксируем отсутствие deadlock (loom не тянем — property-тест с таймаутом).

### 3.2 MCP (`makedo-mcp`)

**Клиент.** `rmcp` (официальный SDK; отвергнуто: самописный клиент — протокол движется, не наша работа). Если версия rmcp потянет конфликт reqwest — карантиним в крейте, как grok-build (не ре-экспортируем reqwest наружу).

**Экспозиция модели — meta-tools, схемы MCP не попадают в манифест.** Причины (из grok-build, подтверждены их продом): стабильный тул-сет = KV-кэш не инвалидируется сервером, подключившимся посреди сессии; контекст не раздувается схемами; RAM не держит N×schema в каждом запросе.
- `search_tool(query, limit)` — BM25 по каталогу; чиним оба бага grok-build: индекс кэшируется по generation каталога (пересборка — только на `tools/list_changed`), `total_hidden_tools` = действительно скрытые, не все.
- `use_tool(name, input)` — диспатч по `QualifiedToolName`; «native tool correction» (подсказка «вызови напрямую», если имя совпало со встроенным) берём.
- Коллизия qualified-имён при регистрации = громкая ошибка статуса сервера, не молчаливая перезапись (баг opencode).

**Транспорты.** stdio + streamable HTTP (SSE-конфиг маппится на streamable HTTP, как в grok-build — отдельного SSE-транспорта нет).
- stdio-фрейминг — resilient (битая строка не роняет соединение, мусорные нотификации игнорируются, на мусор не отвечаем `-32600` — обоснование grok-build: сервер может эхо-зациклиться), **но с потолком строки 10 MB** — `read_until` без лимита в grok-build = OOM от враждебного сервера. Превышение → decode-error событие + разрыв.
- Child-процессы: своя process group (setsid), на drop — killpg + reap; stderr — в лог-файл сервера.

**Lifecycle.** Стейт-машина grok-build берётся как референс почти дословно (это код «после серии инцидентов»): `Empty → Pending → Initializing → Ready`; single-flight handshake (конкуренты паркуются на `Notify` с bounded-wait), RAII-guard восстанавливает состояние при отмене, generation-счётчик отбрасывает результаты устаревших инициализаций, `client_id` в событиях против stale teardown. Старт: `buffer_unordered(4)` (grok: 8; наши машины слабее), стратегия Progressive (первый промпт не ждёт все серверы; `search_tool` честно отвечает `status: "partial"`).

**Рестарт.** Авто-рестарт с backoff — stdio `[1, 4, 16] с`, HTTP до ~2.5 мин (rolling deploy) — берём из grok-build (в opencode рестарта нет вообще — упавший сервер мёртв до ручного вмешательства). Гард от двойного рестарта — атомарный claim + RAII.

**Нотификации.** `tools/list_changed` → коалесинг 50 мс окном по `(server, kind)` (grok-build: burst из 100 нотификаций = 1 событие) → инвалидация BM25-generation + событие в TUI.

**Таймауты.** startup 30 с; tool call **60 с** дефолт (не 100 минут, как в grok-build) с per-tool override в конфиге и `resetTimeoutOnProgress`-эквивалентом.

**Конфиг** — в `~/.makedo/config.toml` / `.makedo/config.toml`:

```toml
[mcp.github]
command = ["npx", "-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "..." }
startup_timeout_s = 30
tool_timeout_s = 60

[mcp.linear]
url = "https://mcp.linear.app/mcp"
headers = { Authorization = "Bearer ..." }
```

**Совместимость с экосистемой Claude Code:** протокол общий (rmcp = та же спека), плюс импорт `.mcp.json` из корня проекта (map `mcpServers`: `command`-строка+`args` → наш массив; `type: http|sse|streamable-http`; `${VAR}` / `${VAR:-default}`-подстановка в command/args/env/url/headers). Прецедент — grok-build импортирует `~/.claude.json` и `~/.cursor/mcp.json`. WebSocket-транспорт не поддерживаем (YAGNI).

**Отложено:** OAuth (PKCE+DCR — большой кусок; bearer-заголовки покрывают старт; дизайн-референсы готовы: двухслойный дедуп grok-build, pending-provider opencode) — фаза M3; MCP resources/prompts/sampling/elicitation (grok прекрасно живёт без них); on-disk дескрипторы тулов.

### 3.3 LLM Wiki (`makedo-wiki`)

**Формат.** Obsidian-совместимый vault `.makedo/wiki/`: markdown-ноды, `[[wikilinks]]`, типизированные рёбра inline-полями `key:: [[Target]]` (грамматика traceability-graph: `satisfies`, `implemented-by`, `verified-by`, … — дефолтный набор, переопределяемый в `schema.md`). Совместимость с Obsidian — фича: человек открывает vault и видит граф.

**Парсер.** Самописный (~100–200 строк): frontmatter (`serde_yaml`? — нет: **[решение]** минимальный key: value парсер, YAML целиком не нужен — `// ponytail: плоский frontmatter; serde_yaml если появятся вложенные структуры`), wikilinks, inline-поля. Граф — `HashMap<NodeId, Node>` + списки рёбер; petgraph отвергнут (нужны только lookup и BFS-обход).

**Индекс.** Общий BM25-модуль (см. §2): корпус = имя + заголовки + описание ноды; кэш по generation (mtime-хэш файлов vault); построение в `spawn_blocking`. Сотни нод = миллисекунды и мегабайты — измеряем, не оптимизируем заранее.

**Тулы:** `wiki_node(name)` (нода + входящие/исходящие рёбра), `wiki_trace(from, edge, depth)` (BFS по типу ребра). Поиск по нодам — единый `memory_search` (wiki импортируется в индекс памяти).

**Init flow (обязателен при отсутствии wiki):**
1. Нет `.git` → предложить `git init`; отказ = выход (требование: wiki живёт в git).
2. `.makedo/wiki` в `.gitignore` → предупредить/убрать.
3. Детерминированный seed без LLM: walk через `ignore` (уважает .gitignore), манифесты, дерево модулей, точки входа → каркасные ноды. Прогресс: spinner `scan` + счётчик файлов.
4. Один LLM-проход **только по seed-сводке** (не по коду) → `index.md`-карта.
5. `git add .makedo/wiki`; коммит — вместе с рабочим коммитом пользователя.
6. Обогащение — инкрементально в ходе работы (обязанность агента, зашита в system prompt).

**`makedo wiki check`** — порт trace-check.js: нет мёртвых ссылок, обязательные рёбра по грамматике; warnings vs errors. Запускается перед коммитом wiki и доступен как CI-команда пользователю.

**Инварианты:** wiki-операции никогда не блокируют agent loop (всё через spawn_blocking + канал); vault читается лениво, полный парс — только при построении индекса.

**Риск:** качество инкрементального обогащения зависит от дисциплины промпта — метрика «покрытие»: % изменённых в PR файлов, имеющих ноду. Меряем на dogfood.

### 3.4 Skills (`makedo-skills`)

Claude-совместимый формат: `SKILL.md` с frontmatter (name, description) + references/. Источники: embedded (в бинаре, `include_str!`) → `~/.makedo/skills/` → `.makedo/skills/`; локальный переопределяет глобальный по имени.

**Ponytail** — embedded, активен по умолчанию, уровень в конфиге (`skills.ponytail = "full" | "lite" | "ultra" | false`). Перед вшиванием: проверка лицензии DietrichGebert/ponytail (MIT ожидается) + NOTICE-файл.

Инжекция: секция system prompt. Динамическая подгрузка скилла по требованию (`skill`-тул) — M2; в M0 скиллы просто в системном промпте (их мало).

### 3.5 Session / agent loop (`makedo-session`)

**Актор.** Одна сессия = один task, владеющий своим состоянием (история, todo, статусы) — никаких shared-локов на горячем пути (у grok-build это ChatState-актор; у них же анти-пример — глобальный `Mutex<Resources>`, который душит их собственный параллелизм; у нас состояние тулов раздельно и передаётся в `ToolCtx` явными `Arc`-хэндлами только там, где нужно). Вход — bounded mpsc команд, выход — `watch`/broadcast событий для TUI. **[решение]** Актор Send, на общем рантайме; `LocalSet`+`!Send` (как grok) — оптимизация, к которой вернёмся, только если границы Send начнут стоить заметных клонов.

**Loop:** `prepare tools → stream LLM → батч тул-коллов (см. 3.1) → push результатов → повторять до stop`. Толерантность к `finish: stop` при эмитированных tool-calls (реальные провайдеры так делают — opencode). Repair неверного регистра имени тула + `invalid`-sink для мусорных коллов (opencode) — дёшево, спасает турны.

**Токены.** `bytes/4` (+765/изображение) через трейт `TokenCounter` — обе базы так живут в проде. Никакого BPE в M0.

**Компакция** — двухуровневая в M1, третий уровень позже:
1. Prune: скрытие старых tool-выводов от модели (не удаление — UI видит всё), защита последних ~40k токенов и последних 2 турнов (числа opencode, проверенные продом).
2. Truncate при реплее: старые tool-выводы до 2000 символов.
3. Summarize-агент — отложен (YAGNI до реальных упоров в контекст на dogfood).
Инвариант: пары tool_use/tool_result не рвутся никогда (оба движка это чтут; Anthropic reject'ит нарушение).

**Стриминг:** SSE от провайдера → события → дельты в TUI (append-only, cap 16 KiB/кадр, UTF-8-safe границы — контракт grok-build).

### 3.6 TUI (`makedo-tui`)

Полноэкранный, по образцу grok-build. `ratatui` + `crossterm` (отвергнуто: cursive — тяжелее и менее живой; самописный рендер — не наша работа).

- Рендер только по dirty + тику активного спиннера; без «60 fps всегда» — это батарея и CPU старых машин.
- Спиннеры: наборы кадров из cli-loaders (`unicode-animations` → статический `loaders.rs`, MIT + NOTICE). Маппинг `process_kind → spinner` — фиксированная таблица (braillewave→LLM, snake→tool, columns→батч, breathe→idle). ASCII-fallback для терминалов без braille-глифов (детект по TERM/локали + ручной флаг) — наша аудитория со старыми окружениями.
- Каждая долгая операция обязана показывать: спиннер + фазу + счётчик (`⡿ индексация wiki — 142/380 файлов`).
- Кастомизация из `~/.makedo/config.toml`: тема (палитра), спиннер-маппинг, keybindings.

### 3.7 Worktree-режим (session)

Параллельная работа над фичами без коллизий — через git worktrees:

- `makedo --worktree <name>` (или `/worktree <name>` в TUI): создаёт ветку `makedo/<name>` и `git worktree add .makedo/worktrees/<name>`; сессия работает в этой директории. Два запуска makedo в двух терминалах = две изолированные фичи, per-path локи их не касаются — файлы физически разные.
- `.makedo/.gitignore` создаётся автоматически и содержит `worktrees/` и `spill/` (пользователь может убрать — тогда его выбор уважается: файл не перезаписывается, только дополняется при отсутствии записи).
- **Завершение — только по явному запросу** (`/finish` в TUI или `makedo finish <name>`): merge ветки `makedo/<name>` в основную ветку репозитория (ту, что checked out в главном дереве), затем `git worktree remove` + удаление ветки. Незакоммиченные изменения в worktree — коммитятся перед merge с сообщением-сводкой. Merge предварительно проверяется без изменения основного дерева (Git >= 2.38). Конфликт исправляется в worktree; основное дерево должно быть чистым и оставаться на записанной базовой ветке. Worktree с логами или историей сохраняется после merge. Полные правила: [WORKTREES.md](WORKTREES.md).
- Wiki (`.makedo/wiki`) — часть дерева: правки wiki в worktree приезжают в main вместе с merge ветки, историю понимания фичи ничто не теряет.
- Реализация — git CLI-команды (`git worktree add/remove`, `git merge`), без libgit2 (лишний dep; git есть у целевой аудитории по определению — wiki уже требует его).

### 3.8 Config (`makedo-config`)

TOML. Merge: `~/.makedo/config.toml` → `.makedo/config.toml` (проект побеждает **целиком по записи** — семантика grok-build: без глубокого field-merge, предсказуемость важнее гибкости; документируем явно). Env-переопределения `MAKEDO_*`. Неизвестные ключи — не фатальны, но собираются в diagnostics (`makedo doctor`), паттерн grok-build. `$schema`-подобная валидация — просто `serde(deny_unknown_fields)` на каждой секции + отчёт.

Секреты (API-ключи): `~/.makedo/credentials.toml`, chmod 0600, отдельно от конфига (паттерн grok-build).

### 3.9 Providers (`makedo-providers`)

```rust
trait Provider: Send + Sync {
    fn stream(&self, req: CompletionRequest) -> BoxStream<'_, Result<LlmEvent, ProviderError>>;
}
```

M0: **Anthropic Messages API** + **OpenAI-compatible chat/completions**. M3: **openai-chatgpt** — ChatGPT-подписка через Codex-бэкенд (механизм opencode: OAuth PKCE на auth.openai.com c client_id Codex CLI + device flow; запросы на chatgpt.com/backend-api/codex/responses с заголовком ChatGPT-Account-Id; диалект — Responses API, третий EventMapper). OAuth-инфраструктура общая с MCP OAuth (M3). Риск: неофициальный канал, OpenAI может закрыть — второй бесплатно покрывает xAI, OpenRouter, Ollama/llama.cpp (локальные модели — релевантно аудитории без бюджета на API). SSE-декодер самописный (~80 строк; отвергнуто: reqwest-eventsource — лишний dep ради простого формата). **Каталог провайдеров/моделей — как у opencode: база models.dev** (M3): embed снапшота в бинарь + фоновое обновление в `~/.makedo/cache/models.json`; из каталога берутся context limits (для компакции), цены (для usage-строки TUI) и валидация имени модели; `makedo models` — список доступного. Тест-провайдер: DeepSeek (`deepseek-v4-flash`, openai-compat, base_url api.deepseek.com/v1). Ретраи: ≤3 с экспонентой + jitter на 429/5xx, respect Retry-After; отмена пробрасывается в HTTP abort. Нормализация tool-calls обоих диалектов в единый `LlmEvent`.

---

## 4. Зависимости (shortlist)

| Крейт | Зачем | Отвергнуто | Стоимость |
|---|---|---|---|
| `tokio` (rt-multi-thread, macros, process, io-util, fs, sync, time, signal) | рантайм | async-std (мёртв), smol (экосистема rmcp/reqwest = tokio) | большой, но безальтернативный |
| `serde`, `serde_json` | всё | — | базовый |
| `toml` | конфиг | — | мал |
| `thiserror` | ошибки lib | snafu | нулевой (макро) |
| `anyhow` | bin | — | мал |
| `schemars` (draft-07) | схемы тулов | самописные схемы (ошибкоопасно) | средний, compile-time |
| `rmcp` (client, минимальные транспорт-фичи) | MCP | самописный клиент | средний; риск конфликта версий reqwest — карантин |
| `reqwest` (rustls-tls, json, stream; default-features=false) | HTTP/SSE | ureq (нет async-стриминга), hyper напрямую (много ручной работы) | большой; rustls вместо openssl — кросс-компиляция без системных либ |
| `ratatui` + `crossterm` | TUI | cursive, самописный | средний |
| `ignore` + `globset` | walk/glob (движок ripgrep как lib) | subprocess rg (нет на старых машинах, +бинарь), walkdir (не уважает gitignore) | мал |
| `grep-searcher` + `grep-regex` | grep-тул | subprocess rg | средний |
| `tokio-util` | CancellationToken | — | мал |
| `parking_lot` | sync-локи вне .await | std Mutex | мал |
| `indexmap` | реестр тулов | — | мал |
| `tracing` + `tracing-subscriber` (fmt, env-filter) | логи в файл | log | средний; в релизе — только file-логгер |
| `dirs` | ~/.makedo | самописный | мал |
| BM25 | **самописный модуль** (~150 строк в makedo-types) | tantivy (mmap-индексы, огромный compile time — на корпус из сотен документов это молоток для мухи), крейт `bm25` (лишний dep ради формулы) | нулевой |
| frontmatter/wikilinks | самописный парсер | комбинаторы (nom/winnow) — грамматика тривиальна | нулевой |

Не берём в M0: `notify` (file-watching wiki — YAGNI, generation по mtime при обращении), tree-sitter (см. 3.1), `criterion`, OpenTelemetry.

Гейт: `cargo deny` + `cargo bloat` отчёт в CI; каждый новый dep — строка в DEPS.md.

---

## 5. Сборка и релизы

**Профили:**

```toml
[profile.release]          # локальная проверка
opt-level = 3
lto = "thin"

[profile.dist]             # релизные бинарники
inherits = "release"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
```

**[открытый вопрос]** `opt-level = 3` vs `"s"`: старые CPU с маленьким кэшем иногда быстрее на компактном коде. Решение — только по bench harness (стартап + wiki-seed + BM25), не по вкусу.

**Матрица таргетов:**

| Таргет | Аудитория | Примечание |
|---|---|---|
| `x86_64-unknown-linux-gnu` | Linux-ноутбуки | glibc-baseline старая (сборка на старом образе / zig cc) |
| `x86_64-unknown-linux-musl` | старые/нестандартные дистрибутивы | полностью статический — главный бинарь для «старых устройств» |
| `x86_64-apple-darwin` | Mac Intel | `MACOSX_DEPLOYMENT_TARGET` = минимально разумный **[открытый вопрос: 10.15 или 11.0]** |
| `aarch64-apple-darwin` | M1/8GB | deployment target 11.0 |

Без `target-cpu` надстроек: дефолтный baseline x86-64 уже не требует AVX2. **[открытый вопрос]** x86-64-v2 (SSE4.2, ~2009+) — допустимо и даёт немного скорости; замерить.

**CI (GitHub Actions):** fmt + clippy `-D warnings` + tests + `cargo deny` на каждый PR; матрица сборки + гейты бюджетов (размер бинаря, hyperfine-стартап на linux-раннере) на тег. Релиз: бинарники + checksums; установка — `curl | sh`-скрипт и прямые ссылки. Никаких npm/brew в M0.

---

## 6. Roadmap

**M0 — «ходячий скелет»** (агент, которым можно чинить его самого):
workspace + lints + CI; config (global→project merge); providers (Anthropic + OpenAI-compat, стриминг, отмена); tool runtime (read/write/edit/bash/grep/glob/todo, semaphore, per-path локи, CancellationToken, трункация+спилл); session loop (актор, стриминг, permissions ask/allow/deny + doom-loop); TUI (рендер, ввод, спиннеры, тема).
*Exit:* dogfood — makedo самостоятельно выполняет реальную задачу в собственном репо на референс-машине; бюджеты старта/RSS в зелёной зоне; отмена турна убивает все child-процессы (тест).

**M1 — LLM Wiki + worktrees:**
парсер vault, граф, BM25-модуль, wiki_search/wiki_node/wiki_trace, init flow с git-enforcement, `makedo wiki check`, спиннер-прогресс индексации, промпт-дисциплина обогащения; компакция (prune + truncate); worktree-режим (§3.7): `--worktree`/`finish`, `.makedo/.gitignore`, merge по явному запросу.
*Exit:* первый запуск в чужом репо на 10k файлов: init < бюджета, wiki в git, поисковые задачи решаются через wiki быстрее, чем grep-ом (замер: число тул-коллов до ответа на 10 эталонных вопросов о кодовой базе).

**M2 — MCP:**
rmcp-клиент, stdio+HTTP, стейт-машина handshake, resilient-фрейминг с потолком строки, рестарт-backoff, коалесинг нотификаций, search_tool/use_tool с кэшированным BM25, статусы в TUI, импорт `.mcp.json` (совместимость с экосистемой Claude Code) + `${VAR}`-экспансия.
*Exit:* 3 реальных сервера (github, файловый, один remote HTTP) переживают: kill -9 сервера (авторестарт), мусор в stdout (соединение живо), 100 list_changed-нотификаций (1 событие), гигантская строка (разрыв без OOM).

**M3 — Skills + polish:**
SKILL.md-лоадер, embedded ponytail, skill-тул; OAuth-инфраструктура (PKCE, localhost-callback, device flow, refresh) — используется дважды: MCP OAuth (PKCE+DCR, двухслойный дедуп) и провайдер openai-chatgpt (ChatGPT-подписка через Codex-бэкенд, §3.8); `makedo doctor`; ASCII-fallback спиннеров.
*Exit:* ponytail активен из коробки и отключается конфигом; OAuth-сервер (Linear) проходит цикл auth→refresh→re-auth.

**M3.5 — Providers × Prompt Caching (отдельный этап, критичен для стоимости):**
Цель: пользователь НИКОГДА не вылетает из prompt-кэша по нашей вине — некэшированные input-токены дороже в 4–10×.

*Провайдеры:* покрыть все провайдеры opencode. Каталог (models.dev, 191 провайдер) уже embedded; работа — диалекты API: (a) anthropic Messages, (b) openai-compat — уже есть; добавить (c) Google Gemini generateContent, (d) AWS Bedrock (sigv4), (e) Azure OpenAI (api-key/deployment-роутинг), (f) GitHub Copilot, (g) openai-chatgpt (Codex, §3.9). Выбор диалекта — по `api`-полю каталога + override в конфиге.

*Prompt caching — инварианты корректности:*
1. **Стабильный префикс байт-в-байт**: system prompt, манифест тулов (уже алфавитный), сериализация сообщений — детерминированы; никакой волатильной информации (время, счётчики) в префиксе.
2. **Компакция не рвёт кэш**: текущий prune мутирует старые сообщения — это инвалидация префикса и удар по деньгам. Решение: prune только СИНХРОННО с summarize-компакцией (один разрыв кэша вместо постоянных) либо отслеживание cache-breakpoint и мутация только за его пределами. [открытый вопрос — спроектировать в этом этапе]
3. **Anthropic**: явные `cache_control` breakpoints — tools, system, хвост истории (до 4 точек); обновление breakpoint'а каждый турн.
4. **OpenAI-compat**: автоматический кэш требует стабильного префикса ≥1024 токенов — гарантируется п.1; `prompt_cache_key`/user-параметр где поддержан.
5. **Метрики**: `Usage` расширяется `cache_read/cache_write`; TUI показывает cache-hit-rate турна; bench-сценарий в `benchmarks/`: 20-турновая сессия, гейт «≥90% input-токенов из кэша со 2-го турна».
*Exit:* на Anthropic и DeepSeek 20-турновый сценарий держит cache-hit ≥90%; смена модели/провайдера не требует правок session-кода (только диалект).

**M4 — Релиз 0.1:**
матрица бинарей, install-скрипт, bench harness как CI-гейт, DEPS.md/NOTICE полные, доки.
*Exit:* все бюджеты §1.2 — CI-гейты; установка на чистую старую машину (Ubuntu 18.04-класс, macOS Intel) одной командой.

**Сознательно отложено (YAGNI, с точкой возврата):** subagents/task (вернёмся при упоре в контекст), summarize-компакция (при реальных переполнениях), OS-sandbox Landlock/Seatbelt (при запросе на untrusted-код), LSP-тул, websearch, code mode, MCP resources/prompts, hooks-система, HTTP-сервер/headless-режим, tree-sitter-разбор bash, BPE-токенайзер, file-watching wiki.

---

## 7. Метрики успеха и как мерим

Бюджеты §1.2 = контракт. Инструментарий:

- **Стартап:** `hyperfine 'makedo --version'` и `makedo --bench-startup` (до первого кадра, self-timed) — CI-гейт на linux-раннере, ручной прогон на референс-Mac.
- **RSS:** Linux — `/proc/self/smaps_rollup` self-report по команде `makedo doctor --mem`; macOS — `/usr/bin/time -l`. Снимается в 3 точках: после старта, после init wiki, после 20-турновой сессии (скриптованный сценарий в makedo-bench).
- **Функциональная метрика wiki:** «стоимость ответа» — число тул-коллов и wall-time на эталонных 10 вопросах о кодовой базе, wiki vs grep-only; цель ≥ 2× меньше коллов.
- **Размер:** `cargo bloat` топ-20 в CI-артефактах; бинарь < 15 MB — гейт.
- Регрессия любого гейта — красный CI, обоснованное повышение бюджета — отдельный PR с одним изменением.

---

*Документ — живой: решения меняются только через PR с обновлением соответствующей секции и пометкой, какой замер/инцидент это мотивировал.*
