# DEPS — реестр зависимостей

Каждый dep: зачем, что отвергнуто, стоимость. Гейт: рост transitive deps требует обоснования в PR. См. PLAN §4.

| Крейт | Зачем | Отвергнуто | Стоимость |
|---|---|---|---|
| serde, serde_json | сериализация всего | — | базовый |
| thiserror | типизированные ошибки lib-крейтов | snafu | нулевой (макро) |
| anyhow | ошибки в bin | — | мал |
| toml | конфиг | — | мал |
| tokio (min features) + tokio-util | рантайм, CancellationToken | async-std (мёртв) | большой, безальтернативный |
| reqwest (rustls, no default) | HTTP/SSE провайдеров, models.dev | ureq (нет async) | большой; rustls = кросс-сборка без системных либ |
| futures-util, bytes | стримы | — | мал |
| schemars | JSON-схемы тулов | самописные схемы (ошибкоопасно) | средний, compile-time |
| ignore + globset + grep-searcher/grep-regex | walk/glob/grep (движок ripgrep как lib) | subprocess rg (нет на старых машинах) | средний |
| rmcp (client + 2 транспорта) | MCP | самописный клиент (протокол движется) | средний |
| rustls (ring) | криптопровайдер для HTTP-клиента rmcp: без него MCP по https не работает | aws-lc-rs (тяжёлая C-сборка) | нулевой: уже в дереве через reqwest |
| ratatui + crossterm | TUI | cursive (тяжелее) | средний |
| ratatui-wgpu + winit (только фича `gui`) | оконный режим `makedo app` и `Makedo.app` | libghostty (эмулятор не нужен: TUI рисуем сами), egui_ratatui (тянет весь egui) | большой: +3.8 MB к бинарю (15.2 против 11.4), поэтому вне сборки по умолчанию |
| Самописные: BM25, SSE-декодер, frontmatter/wikilinks-парсер, spinner-кадры (codegen) | — | tantivy, reqwest-eventsource, serde_yaml, nom | нулевой |
