//! End-to-end: реальный stdio MCP-сервер (python-фикстура) → connect →
//! каталог → `search_tool` → `use_tool`.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use makedo_config::McpServerConfig;
use makedo_mcp::McpManager;
use makedo_types::QualifiedToolName;

fn fixture_config() -> BTreeMap<String, McpServerConfig> {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/echo_server.py");
    let cfg = McpServerConfig {
        command: Some(vec!["python3".into(), script.into()]),
        startup_timeout_s: 10,
        tool_timeout_s: 10,
        ..Default::default()
    };
    BTreeMap::from([("fixture".to_owned(), cfg)])
}

#[tokio::test]
async fn connect_catalog_call_roundtrip() {
    let manager = McpManager::new(&fixture_config());
    manager.connect_all().await;

    let statuses = manager.statuses();
    assert!(
        matches!(
            statuses["fixture"],
            makedo_mcp::ServerStatus::Ready { tools: 1 }
        ),
        "status: {statuses:?}"
    );

    let catalog = manager.catalog().await;
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].qualified.as_str(), "fixture__echo");
    assert!(catalog[0].input_schema["properties"]["text"].is_object());

    let out = manager
        .call(
            &QualifiedToolName::parse("fixture__echo").unwrap(),
            serde_json::json!({"text": "hi"}),
        )
        .await
        .unwrap();
    assert_eq!(out, "echo: hi");

    // неизвестный сервер — типизированная ошибка
    let err = manager
        .call(
            &QualifiedToolName::parse("nope__echo").unwrap(),
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown MCP server"));
}

#[tokio::test]
async fn cancellation_reaches_server_and_keeps_connection_alive() {
    let manager = McpManager::new(&fixture_config());
    manager.connect_all().await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let marker = std::env::temp_dir().join(format!("makedo-mcp-cancel-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let task_manager = manager.clone();
    let token = cancel.clone();
    let task_marker = marker.clone();
    let task = tokio::spawn(async move {
        task_manager
            .call_with_cancel(
                &QualifiedToolName::parse("fixture__echo").unwrap(),
                serde_json::json!({"wait_for_cancel":task_marker}),
                &token,
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();
    let error = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    // The next response proves that the preceding cancellation notification was consumed.
    let result = manager
        .call(
            &QualifiedToolName::parse("fixture__echo").unwrap(),
            serde_json::json!("{\"text\":\"still alive\"}"),
        )
        .await
        .unwrap();
    assert_eq!(result, "echo: still alive");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "cancelled");
    std::fs::remove_file(marker).unwrap();
}

#[tokio::test]
async fn configured_headers_reach_remote_mcp_requests() {
    use tokio::io::AsyncBufReadExt;
    let mut child = tokio::process::Command::new("python3")
        .args([
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/echo_server.py"),
            "--http",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut reader = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let mut port = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut port),
    )
    .await
    .unwrap()
    .unwrap();
    let config = BTreeMap::from([(
        "remote".into(),
        McpServerConfig {
            url: Some(format!("http://127.0.0.1:{}/mcp", port.trim())),
            headers: BTreeMap::from([("Authorization".into(), "Bearer fixture-token".into())]),
            ..Default::default()
        },
    )]);
    let manager = McpManager::new(&config);
    manager.connect_all().await;
    let text = manager
        .call(
            &QualifiedToolName::parse("remote__echo").unwrap(),
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert_eq!(text, "authenticated");
    child.kill().await.unwrap();
}
