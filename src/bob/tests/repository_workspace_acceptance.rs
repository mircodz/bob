use bob::adapters::ToolService;
use bob_kernel::{Invocation, OperationId, RunId, ThreadId};
use serde_json::json;
use std::time::Instant;

#[tokio::test]
#[ignore = "read-only acceptance against an explicitly selected real workspace"]
async fn reported_root_globs_prepare_without_traversing_excluded_checkouts() {
    let root = std::fs::canonicalize(
        std::env::var("BOB_TEST_WORKSPACE")
            .expect("set BOB_TEST_WORKSPACE to the repository being checked"),
    )
    .unwrap();
    let service = ToolService::default();
    for pattern in ["**/*kernel*", "**/*session*"] {
        let started = Instant::now();
        let prepared = service
            .prepare(Invocation {
                operation_id: OperationId::new(),
                run_id: RunId::new(),
                thread_id: ThreadId::new(),
                workspace: root.to_string_lossy().into_owned(),
                name: "glob".into(),
                input: json!({"path":".", "pattern":pattern}),
            })
            .await
            .unwrap_or_else(|error| panic!("{pattern}: {error:#}"));
        assert_eq!(prepared.invocation.name, "glob");
        println!("{pattern}: preparation passed in {:?}", started.elapsed());
    }
}

#[tokio::test]
#[ignore = "read-only search execution against an explicitly selected Bob checkout; requires ripgrep"]
async fn repository_search_execution_matches_ripgrep_with_truthful_pages() {
    use std::collections::BTreeSet;
    use std::time::Duration;
    use tokio::{process::Command, sync::watch, time::timeout};

    let root = std::fs::canonicalize(
        std::env::var("BOB_TEST_WORKSPACE").expect("select the Bob checkout explicitly"),
    )
    .unwrap();
    let service = ToolService::default();
    for scope in ["src/kernel/src", "src/bob/src", "src/core/src"] {
        let reference = timeout(
            Duration::from_secs(30),
            Command::new("rg")
                .args([
                    "--files-with-matches",
                    "--fixed-strings",
                    "--glob",
                    "*.rs",
                    "--max-filesize",
                    "16M",
                    "--",
                    "pub fn",
                    scope,
                ])
                .env_remove("RIPGREP_CONFIG_PATH")
                .current_dir(&root)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            reference.status.success(),
            "ripgrep reference failed: {}",
            String::from_utf8_lossy(&reference.stderr)
        );
        let expected: BTreeSet<_> = String::from_utf8(reference.stdout)
            .unwrap()
            .lines()
            .map(|path| root.join(path).canonicalize().unwrap())
            .collect();
        assert!(!expected.is_empty());
        let start = Instant::now();
        let mut found = BTreeSet::new();
        let mut offset = 1;
        let mut complete = false;
        let mut pages = 0;
        for _ in 0..128 {
            let prepared = timeout(
                Duration::from_secs(10),
                service.prepare(Invocation {
                    operation_id: OperationId::new(),
                    run_id: RunId::new(),
                    thread_id: ThreadId::new(),
                    workspace: root.to_string_lossy().into_owned(),
                    name: "grep".into(),
                    input: json!({"path":scope,"pattern":"pub fn","literal":true,"glob":"*.rs",
                    "output_mode":"files_with_matches","max_results":7,"offset":offset}),
                }),
            )
            .await
            .unwrap()
            .unwrap();
            let (_cancel, receiver) = watch::channel(false);
            let result = timeout(Duration::from_secs(10), service.execute(prepared, receiver))
                .await
                .unwrap()
                .unwrap();
            assert!(!result.is_error, "search execution failed: {}", result.text);
            let report: serde_json::Value = serde_json::from_str(&result.text).unwrap();
            assert_eq!(report["cancelled"], false);
            assert!(report["errors"].as_array().unwrap().is_empty());
            assert!(report["visited"].as_u64().unwrap() <= 10_000);
            assert!(report["bytes_read"].as_u64().unwrap() <= 16 * 1024 * 1024);
            assert!(result.text.len() <= 512 * 1024);
            let entries = report["entries"].as_array().unwrap();
            assert!(entries.len() <= 7);
            for entry in entries {
                let path = root
                    .join(entry["path"].as_str().unwrap())
                    .canonicalize()
                    .unwrap();
                assert!(expected.contains(&path), "unexpected search match");
                assert!(found.insert(path), "page repeated a result");
            }
            pages += 1;
            if let Some(next) = report["next_offset"].as_u64() {
                assert!(next > offset);
                offset = next;
            } else {
                assert_eq!(
                    report["truncated"], false,
                    "incomplete scan must not claim completeness: {report}"
                );
                assert!(report["limits_reached"].as_array().unwrap().is_empty());
                complete = true;
                break;
            }
        }
        assert!(complete, "bounded acceptance page budget exhausted");
        assert_eq!(
            found, expected,
            "complete search omitted matches in {scope}"
        );
        println!(
            "{scope}: {} matching files, {pages} pages, {:?} prepare+execute total",
            found.len(),
            start.elapsed()
        );
    }
}
