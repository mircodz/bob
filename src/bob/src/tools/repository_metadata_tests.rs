use super::super::tests::{executor, run, TestDir};
use super::super::{Bound, ToolService};
use super::*;
use bob_kernel::{PreparedTool, ThreadId};
use serde_json::json;
use std::fs;

const PRIVATE_METADATA: &str = "private-metadata-comment-never-returned";
const PRIVATE_CONTENT: &str = "private-sibling-content-never-returned";

struct Worktree {
    workspace: TestDir,
    external: TestDir,
}

impl Worktree {
    fn new(common: bool) -> Self {
        let fixture = Self {
            workspace: TestDir::new(),
            external: TestDir::new(),
        };
        fs::create_dir_all(fixture.git().join("info")).unwrap();
        fs::write(
            fixture.workspace.file(".git"),
            format!("gitdir: {}\n", fixture.git().display()),
        )
        .unwrap();
        let excludes = if common {
            fs::create_dir_all(fixture.common().join("info")).unwrap();
            fs::write(fixture.git().join("commondir"), "../common\n").unwrap();
            fs::write(fixture.git().join("info/exclude"), "visible.txt\n").unwrap();
            fixture.common()
        } else {
            fixture.git()
        };
        fs::write(
            excludes.join("info/exclude"),
            format!("# {PRIVATE_METADATA}\nignored.txt\n"),
        )
        .unwrap();
        fs::write(fixture.workspace.file("visible.txt"), "hit visible\n").unwrap();
        fs::write(fixture.workspace.file("ignored.txt"), "hit ignored\n").unwrap();
        fs::write(fixture.external.file("sibling.txt"), PRIVATE_CONTENT).unwrap();
        fs::write(fixture.git().join("unrelated.txt"), PRIVATE_CONTENT).unwrap();
        fixture
    }

    fn git(&self) -> PathBuf {
        self.external.file("git")
    }

    fn common(&self) -> PathBuf {
        self.external.file("common")
    }

    async fn prepared(&self, tools: &ToolService) -> PreparedTool {
        tools
            .prepare(
                self.workspace
                    .invocation(&ThreadId::new(), "grep", json!({"pattern":"hit"})),
            )
            .await
            .unwrap()
    }
}

fn bound(tool: &PreparedTool) -> Bound {
    serde_json::from_value(tool.specification.clone()).unwrap()
}

fn metadata(tool: &PreparedTool) -> Vec<PreparedGitMetadata> {
    let Action::Repository { metadata, .. } = bound(tool).action else {
        panic!("expected repository action")
    };
    metadata
}

fn assert_no_private_text(text: &str) {
    assert!(!text.contains(PRIVATE_METADATA), "{text}");
    assert!(!text.contains(PRIVATE_CONTENT), "{text}");
}

#[tokio::test]
async fn prepared_external_gitdir_and_commondir_apply_exact_excludes() {
    for common in [false, true] {
        let fixture = Worktree::new(common);
        let tools = executor();
        for (name, input) in [
            ("grep", json!({"pattern":"hit"})),
            ("glob", json!({"pattern":"*.txt"})),
        ] {
            let prepared = tools
                .prepare(fixture.workspace.invocation(&ThreadId::new(), name, input))
                .await
                .unwrap();
            let outcome = run(&tools, prepared).await;
            assert!(!outcome.is_error, "{}", outcome.text);
            let report: Value = serde_json::from_str(&outcome.text).unwrap();
            assert_eq!(report["entries"].as_array().unwrap().len(), 1);
            assert_eq!(report["entries"][0]["path"], "visible.txt");
            assert_eq!(report["truncated"], false);
            assert!(report.get("preparation_required").is_none());
            assert_no_private_text(&outcome.text);
            assert_no_private_text(&serde_json::to_string(&outcome.presentation).unwrap());
        }
    }
}

#[tokio::test]
async fn approval_binds_exact_roots_fixed_files_and_read_only_capabilities() {
    let fixture = Worktree::new(true);
    let tools = executor();
    let prepared = fixture.prepared(&tools).await;
    let bound = bound(&prepared);
    assert_eq!(bound.root, fixture.workspace.0);
    assert_eq!(bound.workspace_root, fixture.workspace.0);
    let manifests = metadata(&prepared);
    assert_eq!(manifests.len(), 1);
    assert_eq!(
        manifests[0].requirements,
        vec![
            GitMetadataPreparation {
                source: fixture.workspace.file(".git"),
                root: fixture.git(),
                kind: GitMetadataKind::GitDir,
            },
            GitMetadataPreparation {
                source: fixture.git().join("commondir"),
                root: fixture.common(),
                kind: GitMetadataKind::CommonDir,
            },
        ]
    );
    let files: Vec<_> = manifests[0]
        .nodes
        .iter()
        .filter_map(|node| match node {
            MetadataNode::File { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        files,
        vec![
            fixture.workspace.file(".git"),
            fixture.git().join("commondir"),
            fixture.common().join("info/exclude"),
        ]
    );
    let caps = tools.prepared_capabilities(&prepared).unwrap();
    assert!(caps.external_access && caps.requires_approval && caps.plan_safe);
    assert!(!caps.mutates_state && !caps.mutates_workspace && !caps.executes_process);
    assert!(!tools.capabilities("grep").external_access);
    let presentation = serde_json::to_string(&prepared.presentation).unwrap();
    for requirement in &manifests[0].requirements {
        assert!(prepared
            .preview
            .contains(&requirement.root.display().to_string()));
        assert!(prepared
            .preview
            .contains(&requirement.source.display().to_string()));
    }
    assert!(presentation.contains("Only pinned pointer files and info/exclude"));
    assert_no_private_text(&prepared.preview);
    assert_no_private_text(&presentation);
    assert_no_private_text(&prepared.specification.to_string());
}

#[tokio::test]
async fn changed_preview_presentation_or_manifest_cannot_reuse_approval() {
    let fixture = Worktree::new(true);
    let tools = executor();
    let prepared = fixture.prepared(&tools).await;
    for variant in 0..4 {
        let mut changed = prepared.clone();
        match variant {
            0 => changed.preview.push_str(" altered"),
            1 => changed.presentation.as_mut().unwrap().details.clear(),
            2 => changed.specification["action"]["metadata"] = json!([]),
            _ => {
                changed.specification["action"]["metadata"][0]["requirements"][0]["root"] =
                    json!(fixture.external.0)
            }
        }
        assert!(tools.prepared_capabilities(&changed).is_err());
        let outcome = run(&tools, changed).await;
        assert!(outcome.is_error, "{}", outcome.text);
        assert_no_private_text(&outcome.text);
    }
}

#[tokio::test]
async fn pointer_and_exclude_changes_require_a_new_prepared_revision() {
    for target in ["gitdir", "commondir", "exclude"] {
        let fixture = Worktree::new(true);
        let tools = executor();
        let prepared = fixture.prepared(&tools).await;
        match target {
            "gitdir" => fs::write(
                fixture.workspace.file(".git"),
                format!("gitdir: {}\n", fixture.common().display()),
            )
            .unwrap(),
            "commondir" => fs::write(
                fixture.git().join("commondir"),
                format!("{}\n", fixture.common().display()),
            )
            .unwrap(),
            _ => fs::write(fixture.common().join("info/exclude"), "visible.txt\n").unwrap(),
        }
        let outcome = run(&tools, prepared.clone()).await;
        assert!(outcome.is_error, "{target}: {}", outcome.text);
        assert!(
            outcome.text.contains("prepare a new revision"),
            "{}",
            outcome.text
        );
        assert_no_private_text(&outcome.text);
        let refreshed = tools.prepare(prepared.invocation).await.unwrap();
        assert_ne!(refreshed.specification, prepared.specification);
        assert!(
            tools
                .prepared_capabilities(&refreshed)
                .unwrap()
                .requires_approval
        );
        let outcome = run(&tools, refreshed).await;
        assert!(!outcome.is_error, "{target}: {}", outcome.text);
    }
}

#[tokio::test]
async fn replaced_metadata_root_or_file_rejects_even_identical_contents() {
    for directory in [false, true] {
        let fixture = Worktree::new(true);
        let tools = executor();
        let prepared = fixture.prepared(&tools).await;
        if directory {
            fs::rename(fixture.common(), fixture.external.file("old-common")).unwrap();
            fs::create_dir_all(fixture.common().join("info")).unwrap();
            fs::copy(
                fixture.external.file("old-common/info/exclude"),
                fixture.common().join("info/exclude"),
            )
            .unwrap();
        } else {
            let file = fixture.common().join("info/exclude");
            fs::rename(&file, fixture.common().join("info/old-exclude")).unwrap();
            fs::copy(fixture.common().join("info/old-exclude"), file).unwrap();
        }
        let outcome = run(&tools, prepared.clone()).await;
        assert!(outcome.is_error, "{}", outcome.text);
        assert_no_private_text(&outcome.text);
        let refreshed = tools.prepare(prepared.invocation).await.unwrap();
        assert_ne!(refreshed.specification, prepared.specification);
        assert!(!run(&tools, refreshed).await.is_error);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_replacements_never_follow_unapproved_metadata() {
    use std::os::unix::fs::symlink;
    for target in ["pointer", "root", "exclude"] {
        let fixture = Worktree::new(true);
        let tools = executor();
        let prepared = fixture.prepared(&tools).await;
        let path = match target {
            "pointer" => fixture.workspace.file(".git"),
            "root" => fixture.common(),
            _ => fixture.common().join("info/exclude"),
        };
        let moved = fixture.external.file("moved-metadata");
        fs::rename(&path, &moved).unwrap();
        symlink(&moved, &path).unwrap();
        let outcome = run(&tools, prepared.clone()).await;
        assert!(outcome.is_error, "{target}: {}", outcome.text);
        assert_no_private_text(&outcome.text);
        assert!(tools.prepare(prepared.invocation).await.is_err());
    }
}

#[tokio::test]
async fn newly_appearing_metadata_invalidates_pinned_absence() {
    for target in ["commondir", "info", "exclude"] {
        let fixture = Worktree::new(false);
        let tools = executor();
        if target == "info" {
            fs::remove_file(fixture.git().join("info/exclude")).unwrap();
            fs::remove_dir(fixture.git().join("info")).unwrap();
        } else if target == "exclude" {
            fs::remove_file(fixture.git().join("info/exclude")).unwrap();
        }
        let prepared = fixture.prepared(&tools).await;
        match target {
            "commondir" => {
                fs::create_dir_all(fixture.common().join("info")).unwrap();
                fs::write(fixture.git().join("commondir"), "../common\n").unwrap();
            }
            "info" => fs::create_dir(fixture.git().join("info")).unwrap(),
            _ => fs::write(fixture.git().join("info/exclude"), "ignored.txt\n").unwrap(),
        }
        let outcome = run(&tools, prepared).await;
        assert!(outcome.is_error, "{target}: {}", outcome.text);
        assert!(
            outcome.text.contains("prepare a new revision"),
            "{}",
            outcome.text
        );
    }
}

#[test]
fn metadata_change_during_search_discards_the_entire_result() {
    let fixture = Worktree::new(true);
    let context = DiscoveryContext::from_workspace(&fixture.workspace.0, None).unwrap();
    let input = json!({"pattern":"hit"});
    let Action::Repository {
        path,
        ancestors,
        metadata,
    } = prepare_with_context(&fixture.workspace.0, "grep", input.clone(), &context).unwrap()
    else {
        panic!("expected repository action")
    };
    let exclude = fixture.common().join("info/exclude");
    SEARCH_READ_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            fs::write(exclude, "visible.txt\n").unwrap();
        }));
    });
    let (_sender, cancel) = watch::channel(false);
    let result = execute_prepared_with_context(
        &fixture.workspace.0,
        "grep",
        input,
        PreparedScope {
            path: &path,
            ancestors: &ancestors,
        },
        &cancel,
        &context,
        &metadata,
    );
    let error = result.err().expect("raced search must not return results");
    assert!(
        error.to_string().contains("prepare a new revision"),
        "{error:#}"
    );
    SEARCH_READ_HOOK.with(|slot| assert!(slot.borrow().is_none()));
}

#[test]
fn strict_execution_apis_never_load_external_gitdir_or_commondir() {
    for variant in 0..3 {
        let common_only = variant != 0;
        let fixture = Worktree::new(true);
        if common_only {
            let git = if variant == 1 {
                fs::write(fixture.workspace.file(".git"), "gitdir: internal-git\n").unwrap();
                fixture.workspace.file("internal-git")
            } else {
                fs::remove_file(fixture.workspace.file(".git")).unwrap();
                fixture.workspace.file(".git")
            };
            fs::create_dir(&git).unwrap();
            fs::write(
                git.join("commondir"),
                format!("{}\n", fixture.common().display()),
            )
            .unwrap();
        }
        fs::write(fixture.common().join("info/exclude"), [0xff, 0xfe]).unwrap();
        let input = json!({"pattern":PRIVATE_CONTENT});
        let Action::Repository {
            path, ancestors, ..
        } = prepare(&fixture.workspace.0, "grep", input.clone()).unwrap()
        else {
            panic!("expected repository action")
        };
        let context = DiscoveryContext::from_workspace(&fixture.workspace.0, None).unwrap();
        let (_sender, cancel) = watch::channel(false);
        let reports = [
            execute(
                &fixture.workspace.0,
                "grep",
                input.clone(),
                &path,
                &ancestors,
                &cancel,
            )
            .unwrap(),
            execute_with_context(
                &fixture.workspace.0,
                "grep",
                input.clone(),
                &path,
                &ancestors,
                &cancel,
                &context,
            )
            .unwrap(),
            execute_prepared_with_context(
                &fixture.workspace.0,
                "grep",
                input,
                PreparedScope {
                    path: &path,
                    ancestors: &ancestors,
                },
                &cancel,
                &context,
                &[],
            )
            .unwrap(),
        ];
        for report in reports {
            assert!(report.entries.is_empty());
            assert!(report.truncated);
            assert_eq!(report.preparation_required.len(), 1);
            assert_eq!(
                report.preparation_required[0].kind,
                if common_only {
                    GitMetadataKind::CommonDir
                } else {
                    GitMetadataKind::GitDir
                }
            );
            assert_eq!(
                report.preparation_required[0].root,
                if common_only {
                    fixture.common()
                } else {
                    fixture.git()
                }
            );
            assert_no_private_text(&serde_json::to_string(&report).unwrap());
            assert!(report
                .errors
                .iter()
                .all(|error| error.contains("fresh approval")));
        }
    }
}

#[tokio::test]
async fn cancelled_or_unexecuted_preparation_exposes_no_external_contents() {
    let fixture = Worktree::new(true);
    let tools = executor();
    let prepared = fixture.prepared(&tools).await;
    assert_no_private_text(&prepared.specification.to_string());
    assert_no_private_text(&prepared.preview);
    assert_no_private_text(&serde_json::to_string(&prepared.presentation).unwrap());
    assert!(tools.files.lock().await.reads.is_empty());
    fs::write(fixture.common().join("info/exclude"), [0xff, 0xfe]).unwrap();
    let (_sender, cancel) = watch::channel(true);
    let outcome = tools.execute(prepared, cancel).await.unwrap();
    assert!(outcome.is_error);
    assert!(outcome.text.contains("cancelled before execution"));
    assert_no_private_text(&outcome.text);
    assert!(outcome.presentation.is_none());
    assert!(tools.files.lock().await.reads.is_empty());
}

#[tokio::test]
async fn metadata_approval_never_widens_external_search_effect_root() {
    let workspace = TestDir::new();
    let fixture = Worktree::new(true);
    let tools = executor();
    let prepared = tools
        .prepare(workspace.invocation(
            &ThreadId::new(),
            "grep",
            json!({"path":fixture.workspace.0, "pattern":"hit|private-sibling"}),
        ))
        .await
        .unwrap();
    assert_eq!(bound(&prepared).root, fixture.workspace.0);
    assert_eq!(bound(&prepared).workspace_root, workspace.0);
    let outcome = run(&tools, prepared).await;
    assert!(!outcome.is_error, "{}", outcome.text);
    let report: Value = serde_json::from_str(&outcome.text).unwrap();
    assert_eq!(report["entries"].as_array().unwrap().len(), 1);
    assert_eq!(report["entries"][0]["path"], "visible.txt");
    assert_no_private_text(&outcome.text);
}

#[tokio::test]
async fn internal_metadata_and_list_need_no_fresh_metadata_approval() {
    let tools = executor();
    for common in [false, true] {
        let root = TestDir::new();
        fs::create_dir_all(root.file(".git/info")).unwrap();
        fs::write(root.file(".git/info/exclude"), "ignored.txt\n").unwrap();
        if common {
            fs::create_dir_all(root.file(".git/common/info")).unwrap();
            fs::write(root.file(".git/commondir"), "common\n").unwrap();
            fs::write(root.file(".git/common/info/exclude"), "ignored.txt\n").unwrap();
            fs::write(root.file(".git/info/exclude"), "visible.txt\n").unwrap();
        }
        fs::write(root.file("ignored.txt"), "hit\n").unwrap();
        fs::write(root.file("visible.txt"), "hit\n").unwrap();
        let prepared = tools
            .prepare(root.invocation(&ThreadId::new(), "grep", json!({"pattern":"hit"})))
            .await
            .unwrap();
        assert!(metadata(&prepared).is_empty());
        let caps = tools.prepared_capabilities(&prepared).unwrap();
        assert!(!caps.external_access && !caps.requires_approval);
        let outcome = run(&tools, prepared).await;
        assert!(!outcome.is_error, "{}", outcome.text);
        let report: Value = serde_json::from_str(&outcome.text).unwrap();
        assert_eq!(report["entries"].as_array().unwrap().len(), 1);
        assert_eq!(report["entries"][0]["path"], "visible.txt");
    }
    let fixture = Worktree::new(true);
    let list = tools
        .prepare(
            fixture
                .workspace
                .invocation(&ThreadId::new(), "list_dir", json!({})),
        )
        .await
        .unwrap();
    assert!(metadata(&list).is_empty());
    assert!(
        !tools
            .prepared_capabilities(&list)
            .unwrap()
            .requires_approval
    );
    let outcome = run(&tools, list).await;
    assert!(!outcome.is_error, "{}", outcome.text);
    let report: Value = serde_json::from_str(&outcome.text).unwrap();
    assert_eq!(report["bytes_read"], 0);
}

#[test]
fn metadata_preparation_enforces_file_and_aggregate_byte_limits() {
    let fixture = Worktree::new(false);
    let directory = Directory::open(&fixture.workspace.0).unwrap();
    let exclude = fixture.git().join("info/exclude");
    fs::write(&exclude, "#".repeat(METADATA_FILE_LIMIT)).unwrap();
    let mut bytes = 0;
    assert!(prepare_git_metadata(&directory, &fixture.workspace.0, &mut bytes).is_ok());
    let consumed = bytes;
    let mut bytes = METADATA_TOTAL_LIMIT - consumed;
    assert!(prepare_git_metadata(&directory, &fixture.workspace.0, &mut bytes).is_ok());
    assert_eq!(bytes, METADATA_TOTAL_LIMIT);
    let mut bytes = METADATA_TOTAL_LIMIT - consumed + 1;
    let error = prepare_git_metadata(&directory, &fixture.workspace.0, &mut bytes).unwrap_err();
    assert!(error.to_string().contains("byte limit"), "{error:#}");
    fs::write(exclude, "#".repeat(METADATA_FILE_LIMIT + 1)).unwrap();
    assert!(prepare_git_metadata(&directory, &fixture.workspace.0, &mut 0).is_err());
}

#[test]
fn metadata_preparation_rejects_recursive_or_oversized_pointers() {
    for pointer in [".".to_owned(), "../common".to_owned(), "x".repeat(4097)] {
        let fixture = Worktree::new(true);
        fs::write(fixture.git().join("commondir"), &pointer).unwrap();
        if pointer == "../common" {
            fs::write(fixture.common().join("commondir"), "../git\n").unwrap();
        }
        let directory = Directory::open(&fixture.workspace.0).unwrap();
        let error = prepare_git_metadata(&directory, &fixture.workspace.0, &mut 0).unwrap_err();
        assert!(
            error.to_string().contains("cyclic")
                || error.to_string().contains("chained")
                || error.to_string().contains("pointer length"),
            "{error:#}"
        );
    }
}

#[test]
fn metadata_discovery_enforces_chain_and_directory_depth_limits() {
    let fixture = Worktree::new(false);
    for index in 0..METADATA_CHAIN_LIMIT {
        let nested = fixture.workspace.file(&format!("nested-{index}"));
        fs::create_dir(&nested).unwrap();
        fs::write(
            nested.join(".git"),
            format!("gitdir: {}\n", fixture.git().display()),
        )
        .unwrap();
    }
    let context = DiscoveryContext::from_workspace(&fixture.workspace.0, None).unwrap();
    let error = prepare_with_context(
        &fixture.workspace.0,
        "glob",
        json!({"pattern":"**"}),
        &context,
    )
    .unwrap_err();
    assert!(error.to_string().contains("chain limit"), "{error:#}");
    let root = TestDir::new();
    let mut nested = root.0.clone();
    for _ in 0..=DEPTH_LIMIT {
        nested.push("d");
        fs::create_dir(&nested).unwrap();
    }
    let context = DiscoveryContext::from_workspace(&root.0, None).unwrap();
    let error =
        prepare_with_context(&root.0, "glob", json!({"pattern":"**"}), &context).unwrap_err();
    assert!(error.to_string().contains("traversal limit"), "{error:#}");
}
