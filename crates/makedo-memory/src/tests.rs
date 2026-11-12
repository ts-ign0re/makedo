#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::tools::{Assessment, WriteArgs};

fn fixture() -> Memory {
    let root = std::env::temp_dir().join(format!("makedo-memory-{}", unique_id()));
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-b", "main"]).unwrap();
    git(&root, &["config", "user.email", "test@example.invalid"]).unwrap();
    git(&root, &["config", "user.name", "Memory Test"]).unwrap();
    std::fs::write(root.join("code.txt"), "base\n").unwrap();
    git(&root, &["add", "code.txt"]).unwrap();
    git(&root, &["commit", "-m", "base"]).unwrap();
    Memory::open(
        &root,
        makedo_config::MemoryConfig {
            embedding_url: "http://127.0.0.1:1/api/embed".into(),
            ..Default::default()
        },
    )
    .unwrap()
}

fn note(text: &str) -> WriteArgs {
    WriteArgs {
        title: text.into(),
        text: text.into(),
        sources: vec!["code.txt".into()],
        id: None,
        expected_revision: None,
        assessment: None,
        supersedes: None,
    }
}

fn search(task: &str) -> SearchArgs {
    SearchArgs {
        task: task.into(),
        context: None,
        hypothesis: None,
        kind: None,
        limit: Some(10),
        all_branches: None,
    }
}

#[tokio::test]
async fn durable_notes_cas_corrections_and_literal_search() {
    let memory = fixture();
    let session = memory.start_session("main", &[]).unwrap();
    let saved = session
        .write_note(&note("resume session recovery"))
        .unwrap();
    let id = saved["id"].as_i64().unwrap();
    let mut revision = note("resume session uses database recovery");
    revision.id = Some(id);
    revision.expected_revision = Some(1);
    revision.assessment = Some(Assessment::Disputed);
    session.write_note(&revision).unwrap();
    assert!(
        session
            .write_note(&revision)
            .unwrap_err()
            .to_string()
            .contains("revision conflict")
    );
    let mut correction = note("resume session corrected after interrupted tool");
    correction.supersedes = Some(id);
    session.write_note(&correction).unwrap();
    let read = memory.read(id).unwrap();
    assert_eq!(read["revisions"].as_array().unwrap().len(), 2);
    assert_eq!(read["superseded_by"].as_array().unwrap().len(), 1);
    let result = memory.search(search("resume \" OR NOT (")).await.unwrap();
    assert_eq!(result.mode, "lexical_only");
    assert!(result.notice.is_some());
    assert_eq!(result.results.len(), 2);
    assert!(result.results[0]["relevance_score"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn wiki_index_follows_startup_and_tool_writes_and_reads_only_changed_files() {
    let memory = fixture();
    std::fs::create_dir_all(&memory.vault).unwrap();
    let node = memory.vault.join("Feed Service.md");
    let write = |body: &str| {
        std::fs::write(
            &node,
            format!("---\ntype: module\ndescription: comfort layer\n---\n\n{body}\n"),
        )
        .unwrap();
    };
    let found = |query: &'static str| {
        let memory = memory.clone();
        async move {
            let mut args = search(query);
            args.kind = Some("wiki".into());
            memory.search(args).await.unwrap().results
        }
    };
    write("Ingests the rubberband odds feed.");
    assert!(
        found("rubberband").await.is_empty(),
        "search itself must not read the vault"
    );

    memory.import_wiki().unwrap(); // startup
    for query in ["rubberband", "comfort"] {
        let hits = found(query).await;
        assert_eq!(hits[0]["title"], "Feed Service", "{query}");
        assert!(hits[0]["summary"].as_str().unwrap().starts_with("[module]"));
    }

    // Changed content behind an old mtime: the startup pass must not re-read the file...
    write("Ingests the zebra feed.");
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    std::fs::File::options()
        .write(true)
        .open(&node)
        .unwrap()
        .set_modified(old)
        .unwrap();
    memory.import_wiki().unwrap();
    assert!(found("zebra").await.is_empty());
    // ...while a tool write indexes exactly that file, even through an aliased path.
    memory
        .import_wiki_files(&[
            memory.vault.join("../wiki/Feed Service.md"),
            memory.cwd.join("code.txt"),
        ])
        .unwrap();
    assert_eq!(found("zebra").await[0]["title"], "Feed Service");
    assert!(found("rubberband").await.is_empty());

    // Seeded scaffolding matches the words but knows nothing: it must not become a hint.
    let stub = format!(
        "---\ntype: module\ndescription: Top-level directory\n---\n\n{} parsed\n",
        makedo_wiki::seed::SEED_MARKER
    );
    for (name, text) in [
        (
            "schema",
            "---\ntype: schema\ndescription: parsed\n---\n\nparsed\n",
        ),
        (
            "index",
            "---\ntype: index\ndescription: parsed\n---\n\nparsed\n",
        ),
        ("src", stub.as_str()),
    ] {
        std::fs::write(memory.vault.join(format!("{name}.md")), text).unwrap();
    }
    memory.import_wiki().unwrap();
    assert_eq!(
        found("parsed").await.len(),
        3,
        "still searchable on request"
    );

    // The harness pre-search: one line per hit, nothing for stop words only.
    let hints = memory
        .hints("where does the zebra feed get parsed?")
        .unwrap();
    assert!(
        hints.starts_with("- wiki #")
            && hints.contains("Feed Service — [module]")
            && hints.lines().count() == 1,
        "{hints}"
    );
    assert_eq!(memory.hints("is it on?").unwrap(), "");

    // The same checkout on another branch still sees its wiki.
    git(&memory.cwd, &["switch", "-c", "feature"]).unwrap();
    let on_feature = Memory::open(&memory.cwd, memory.config.clone()).unwrap();
    on_feature.import_wiki().unwrap();
    assert!(
        on_feature
            .hints("zebra feed")
            .unwrap()
            .contains("Feed Service")
    );

    std::fs::remove_file(&node).unwrap();
    memory.import_wiki().unwrap();
    assert!(
        found("zebra").await.is_empty(),
        "rows of deleted files are dropped"
    );
}

#[tokio::test]
async fn wiki_rows_of_a_checkout_that_moved_away_are_dropped() {
    let memory = fixture();
    let mut elsewhere = memory.clone();
    elsewhere.vault = memory.cwd.join("elsewhere/wiki");
    for vault in [&memory.vault, &elsewhere.vault] {
        std::fs::create_dir_all(vault).unwrap();
    }
    std::fs::write(
        elsewhere.vault.join("Ghost.md"),
        "---\ntype: module\ndescription: quokka\n---\n\nquokka\n",
    )
    .unwrap();
    elsewhere.import_wiki().unwrap();
    let found = || async {
        let mut args = search("quokka");
        args.kind = Some("wiki".into());
        memory.search(args).await.unwrap().results.len()
    };
    // Another checkout's rows live as long as its files...
    memory.import_wiki().unwrap();
    assert_eq!(found().await, 1);
    // ...and go once that checkout is gone.
    std::fs::remove_dir_all(memory.cwd.join("elsewhere")).unwrap();
    memory.import_wiki().unwrap();
    assert_eq!(found().await, 0);
}

#[test]
fn concurrent_writers_preserve_every_event_and_readers_see_snapshot() {
    let memory = fixture();
    let session = memory.start_session("main", &[]).unwrap();
    let id = session.write_note(&note("initial")).unwrap()["id"]
        .as_i64()
        .unwrap();
    let mut writer = memory.connect().unwrap();
    let tx = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    tx.execute("UPDATE entries SET body='uncommitted' WHERE id=?1", [id])
        .unwrap();
    assert_eq!(memory.read(id).unwrap()["entry"]["text"], "initial");
    tx.rollback().unwrap();
    let workers: Vec<_> = (0..4)
        .map(|n| {
            let session = memory.start_session(&format!("agent-{n}"), &[]).unwrap();
            std::thread::spawn(move || {
                for i in 0..25 {
                    session
                        .record(&json!({"role":"user","content":format!("agent-{n} event-{i}")}))
                        .unwrap();
                }
                assert_eq!(session.memory.events(&session.id).unwrap().len(), 25);
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let count: i64 = memory
        .connect()
        .unwrap()
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 100);
}

#[test]
fn worktrees_share_database_and_merge_revert_do_not_erase_history() {
    let main = fixture();
    let path = main.cwd.join("feature-worktree");
    git(
        &main.cwd,
        &["worktree", "add", "-b", "feature", path.to_str().unwrap()],
    )
    .unwrap();
    let feature = Memory::open(&path, main.config.clone()).unwrap();
    assert_eq!(main.path, feature.path);
    let session = feature.start_session("subagent", &[]).unwrap();
    let id = session.write_note(&note("feature fix")).unwrap()["id"]
        .as_i64()
        .unwrap();
    std::fs::write(path.join("code.txt"), "fixed\n").unwrap();
    git(&path, &["commit", "-am", "fix"]).unwrap();
    git(
        &main.cwd,
        &["merge", "--no-ff", "feature", "-m", "integrate"],
    )
    .unwrap();
    let sha = git(&main.cwd, &["rev-parse", "HEAD"]).unwrap();
    main.record_merge("feature", "refs/heads/main", &sha)
        .unwrap();
    assert_eq!(main.evidence(id).unwrap()[0].kind, "merged");
    git(&main.cwd, &["revert", "--no-edit", "-m", "1", &sha]).unwrap();
    assert!(git(&main.cwd, &["merge-base", "--is-ancestor", &sha, "main"]).is_some());
    main.refresh_delivery().unwrap();
    let evidence = main.evidence(id).unwrap();
    assert!(evidence.iter().any(|e| e.kind == "merged"));
    assert!(evidence.iter().any(|e| e.kind == "reverted"));
    assert_eq!(main.read(id).unwrap()["entry"]["assessment"], "hypothesis");
}

#[test]
fn legacy_import_is_idempotent_and_transcript_preserves_opaque_payload() {
    let memory = fixture();
    let path = memory.cwd.join("legacy.jsonl");
    let message = json!({"role":"assistant","content":[{"type":"open_ai_output","items":[{"id":"reasoning-1","encrypted_content":"opaque"}]}]});
    std::fs::write(&path, format!("{message}\n{{\"partial\":")).unwrap();
    memory.import_journal(&path).unwrap();
    memory.import_journal(&path).unwrap();
    let id = memory.latest_session().unwrap().unwrap();
    assert_eq!(memory.events(&id).unwrap(), vec![message.clone()]);
    let next = memory
        .start_session("main", &memory.events(&id).unwrap())
        .unwrap();
    next.record(&json!({"role":"user","content":"resume"}))
        .unwrap();
    assert_eq!(memory.events(&id).unwrap(), vec![message]);
    assert_eq!(memory.events(&next.id).unwrap().len(), 2);
}

#[test]
fn model_cannot_forge_delivery_or_passed_checks() {
    assert!(serde_json::from_value::<WriteArgs>(json!({"title":"claim","text":"fixed","sources":["code.txt"],"assessment":"confirmed","delivery":"merged"})).is_err());
    let memory = fixture();
    let session = memory.start_session("main", &[]).unwrap();
    let id = session.write_note(&note("a proposed fix")).unwrap()["id"]
        .as_i64()
        .unwrap();
    assert!(memory.evidence(id).unwrap().is_empty());
    session
        .record_check("cargo test", true, "/tmp/test.log")
        .unwrap();
    assert_eq!(memory.evidence(id).unwrap()[0].kind, "check_passed");
    assert!(
        memory
            .record_merge("main", "refs/heads/nonexistent", "bad-sha")
            .is_err()
    );
}

#[test]
fn shared_note_identity_survives_revisions_and_differs_between_clones() {
    let first = fixture().start_session("main", &[]).unwrap();
    let second = fixture().start_session("main", &[]).unwrap();
    let a = first.write_note(&note("first developer")).unwrap();
    let b = second.write_note(&note("second developer")).unwrap();
    assert_eq!(a["id"], b["id"]);
    assert_ne!(a["uid"], b["uid"]);
    let mut update = note("corrected by first developer");
    update.id = a["id"].as_i64();
    update.expected_revision = Some(1);
    assert_eq!(first.write_note(&update).unwrap()["uid"], a["uid"]);
}
