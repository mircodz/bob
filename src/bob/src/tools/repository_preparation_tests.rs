use super::super::tests::TestDir;
use super::*;
use serde_json::json;
use std::collections::BTreeSet;
use std::fs;

fn write(root: &Path, path: &str, text: impl AsRef<[u8]>) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn prepare_search(root: &Path, name: &str, input: Value) -> Result<Action> {
    let context = DiscoveryContext::from_workspace(root, None)?;
    prepare_with_context(root, name, input, &context)
}

fn execute_search(root: &Path, name: &str, input: Value, action: Action) -> SearchReport {
    let Action::Repository {
        path,
        ancestors,
        metadata,
    } = action
    else {
        panic!("expected repository action")
    };
    let context = DiscoveryContext::from_workspace(root, None).unwrap();
    let (_sender, cancel) = watch::channel(false);
    execute_prepared_with_context(
        root,
        name,
        input,
        PreparedScope {
            path: &path,
            ancestors: &ancestors,
        },
        &cancel,
        &context,
        &metadata,
    )
    .unwrap()
}

fn assert_search(root: &Path, name: &str, input: Value, expected: &[&str]) {
    let action = prepare_search(root, name, input.clone()).unwrap();
    let report = execute_search(root, name, input, action);
    assert!(!report.truncated, "{}", render(&report));
    assert!(report.preparation_required.is_empty());
    assert_eq!(
        report
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<BTreeSet<_>>(),
        expected.iter().copied().collect::<BTreeSet<_>>()
    );
}

fn assert_source_prunes(source: &str) {
    let root = TestDir::new();
    fs::create_dir(root.file(".git")).unwrap();
    write(&root.0, source, "/reference/\n");
    write(&root.0, "reference/.git", "malformed nested Git metadata\n");
    write(&root.0, "reference/.ignore", [0xff, 0xfe]);
    write(&root.0, "src/kernel.rs", "hit\n");
    write(&root.0, "src/session.rs", "hit\n");
    for (name, input, expected) in [
        (
            "glob",
            json!({"pattern":"**/*kernel*"}),
            vec!["src/kernel.rs"],
        ),
        (
            "glob",
            json!({"pattern":"**/*session*"}),
            vec!["src/session.rs"],
        ),
        (
            "grep",
            json!({"pattern":"hit"}),
            vec!["src/kernel.rs", "src/session.rs"],
        ),
    ] {
        assert_search(&root.0, name, input, &expected);
    }
    let input = json!({"path":"reference", "pattern":"**"});
    let action = prepare_search(&root.0, "glob", input.clone()).unwrap();
    let report = execute_search(&root.0, "glob", input, action);
    assert!(report.entries.is_empty());
    assert_eq!(report.visited, 0);
    assert!(report.truncated);
    assert!(report
        .errors
        .iter()
        .any(|error| error.contains("scope was not searched")));
}

#[test]
fn preparation_gitignore_prunes_before_nested_metadata() {
    assert_source_prunes(".gitignore");
}

#[test]
fn preparation_ignore_prunes_before_nested_metadata() {
    assert_source_prunes(".ignore");
}

#[test]
fn preparation_git_exclude_prunes_before_nested_metadata() {
    assert_source_prunes(".git/info/exclude");
}

#[test]
fn preparation_ignores_oversized_and_deep_subtrees_for_every_source() {
    let root = TestDir::new();
    fs::create_dir(root.file(".git")).unwrap();
    write(&root.0, "reference/.git", "malformed nested Git metadata\n");
    write(&root.0, "reference/wide/first", "");
    for index in 0..ENTRY_LIMIT {
        fs::write(root.file(&format!("reference/wide/{index}")), "").unwrap();
    }
    let mut deep = root.file("reference/deep");
    for _ in 0..=DEPTH_LIMIT {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join(".git"), "malformed deep metadata\n").unwrap();
    write(&root.0, "src/kernel.rs", "hit\n");
    for source in [".gitignore", ".ignore", ".git/info/exclude"] {
        write(&root.0, source, "/reference/\n");
        assert_search(
            &root.0,
            "glob",
            json!({"pattern":"**/*kernel*"}),
            &["src/kernel.rs"],
        );
        fs::remove_file(root.file(source)).unwrap();
    }
}

#[test]
fn preparation_unignored_wide_and_deep_controls_remain_bounded() {
    for wide in [true, false] {
        let root = TestDir::new();
        if wide {
            write(&root.0, "reference/first", "");
            for index in 0..ENTRY_LIMIT {
                fs::write(root.file(&format!("reference/{index}")), "").unwrap();
            }
        } else {
            let mut deep = root.0.clone();
            for _ in 0..=DEPTH_LIMIT {
                deep.push("d");
            }
            fs::create_dir_all(deep).unwrap();
        }
        let error = prepare_search(&root.0, "glob", json!({"pattern":"**/*kernel*"})).unwrap_err();
        assert!(error.to_string().contains("traversal limit"), "{error:#}");
    }
}

#[test]
fn preparation_nested_repository_resets_only_git_rules() {
    let root = TestDir::new();
    let outside = TestDir::new();
    write(&root.0, ".git/info/exclude", "git-excluded/\n");
    write(&root.0, ".gitignore", "git-ignored/\n");
    write(&root.0, ".ignore", "blocked/\n");
    write(&root.0, "nested/.git/info/exclude", "local-excluded/\n");
    write(&root.0, "nested/.gitignore", "local-ignored/\n");
    write(&root.0, "nested/.ignore", "local-blocked/\n");
    write(&outside.0, "info/exclude", "");
    for path in ["nested/git-excluded", "nested/git-ignored"] {
        write(
            &root.0,
            &format!("{path}/.git"),
            format!("gitdir: {}\n", outside.0.display()),
        );
        write(&root.0, &format!("{path}/kernel.rs"), "hit\n");
    }
    for path in [
        "blocked",
        "local-excluded",
        "local-ignored",
        "local-blocked",
    ] {
        write(&root.0, &format!("nested/{path}/.git"), "malformed\n");
    }
    for scope in [".", "nested"] {
        let input = json!({"path":scope, "pattern":"**/*kernel*"});
        let action = prepare_search(&root.0, "glob", input.clone()).unwrap();
        let Action::Repository { metadata, .. } = &action else {
            panic!("expected repository action")
        };
        assert_eq!(metadata.len(), 2);
        assert!(metadata.iter().all(|prepared| {
            prepared.requirements.len() == 1 && prepared.requirements[0].root == outside.0
        }));
        let report = execute_search(&root.0, "glob", input, action);
        assert!(!report.truncated, "{}", render(&report));
        assert_eq!(report.entries.len(), 2);
    }
}

#[test]
fn preparation_ignore_precedence_and_negation_match_execution() {
    let root = TestDir::new();
    write(&root.0, ".git/info/exclude", "reference/\n");
    write(&root.0, ".gitignore", "!reference/\n");
    write(&root.0, "reference/kernel.rs", "hit\n");
    assert_search(
        &root.0,
        "glob",
        json!({"pattern":"**/*kernel*"}),
        &["reference/kernel.rs"],
    );
    write(&root.0, ".ignore", "reference/\n");
    write(&root.0, "reference/.git", "malformed\n");
    assert_search(&root.0, "glob", json!({"pattern":"**/*kernel*"}), &[]);
}

#[test]
fn preparation_trusted_nested_launch_applies_ancestor_pruning() {
    let root = TestDir::new();
    write(&root.0, "repo/.git/info/exclude", "reference/\n");
    write(&root.0, "repo/.gitignore", "generated/\n");
    write(&root.0, "repo/.ignore", "blocked/\n");
    for path in ["reference", "generated", "blocked"] {
        write(&root.0, &format!("repo/deep/{path}/.git"), "malformed\n");
    }
    write(&root.0, "repo/deep/kernel.rs", "hit\n");
    assert_search(
        &root.file("repo/deep"),
        "glob",
        json!({"pattern":"**/*kernel*"}),
        &["kernel.rs"],
    );
}

#[test]
fn preparation_external_excludes_prune_without_losing_exact_approvals() {
    for common in [false, true] {
        let root = TestDir::new();
        let outside = TestDir::new();
        write(
            &root.0,
            ".git",
            format!("gitdir: {}\n", outside.file("git").display()),
        );
        let exclude = if common {
            write(&outside.0, "git/commondir", "../common\n");
            "common/info/exclude"
        } else {
            "git/info/exclude"
        };
        write(&outside.0, exclude, "reference/\n");
        write(&root.0, "reference/.git", "malformed\n");
        write(&root.0, "kernel.rs", "hit\n");
        let input = json!({"pattern":"**/*kernel*"});
        let action = prepare_search(&root.0, "glob", input.clone()).unwrap();
        let Action::Repository { metadata, .. } = &action else {
            panic!("expected repository action")
        };
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].requirements.len(), if common { 2 } else { 1 });
        assert_eq!(metadata[0].requirements[0].root, outside.file("git"));
        if common {
            assert_eq!(metadata[0].requirements[1].root, outside.file("common"));
        }
        let report = execute_search(&root.0, "glob", input, action);
        assert!(!report.truncated, "{}", render(&report));
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].path, "kernel.rs");
    }
}

#[test]
fn removed_ignore_rule_does_not_authorize_new_external_metadata() {
    let root = TestDir::new();
    let outside = TestDir::new();
    write(&root.0, ".ignore", "reference/\n");
    write(
        &root.0,
        "reference/.git",
        format!("gitdir: {}\n", outside.0.display()),
    );
    write(&outside.0, "info/exclude", [0xff, 0xfe]);
    let input = json!({"pattern":"**/*kernel*"});
    let action = prepare_search(&root.0, "glob", input.clone()).unwrap();
    fs::remove_file(root.file(".ignore")).unwrap();
    let report = execute_search(&root.0, "glob", input, action);
    assert!(report.truncated);
    assert_eq!(report.preparation_required.len(), 1);
    assert_eq!(report.preparation_required[0].root, outside.0);
    assert_eq!(
        report.bytes_read,
        fs::metadata(root.file("reference/.git")).unwrap().len() as usize
    );
    assert!(report.errors.iter().all(|error| !error.contains("utf-8")));
}

#[test]
fn preparation_ignore_file_reads_remain_bounded() {
    for source in [".gitignore", ".ignore"] {
        let root = TestDir::new();
        write(&root.0, source, "#".repeat(METADATA_FILE_LIMIT));
        assert!(prepare_search(&root.0, "glob", json!({"pattern":"**"})).is_ok());
        write(&root.0, source, "#".repeat(METADATA_FILE_LIMIT + 1));
        assert!(prepare_search(&root.0, "glob", json!({"pattern":"**"})).is_err());
    }
}

#[cfg(unix)]
#[test]
fn preparation_does_not_follow_ignore_or_child_symlinks() {
    use std::os::unix::fs::symlink;
    let outside = TestDir::new();
    write(&outside.0, "rules", "reference/\n");
    write(&outside.0, ".git", "malformed\n");
    for source in [".gitignore", ".ignore", ".git/info/exclude"] {
        let root = TestDir::new();
        fs::create_dir_all(root.file(".git/info")).unwrap();
        symlink(outside.file("rules"), root.file(source)).unwrap();
        assert!(prepare_search(&root.0, "glob", json!({"pattern":"**"})).is_err());
    }
    let root = TestDir::new();
    symlink(&outside.0, root.file("reference")).unwrap();
    write(&root.0, "kernel.rs", "hit\n");
    assert_search(
        &root.0,
        "glob",
        json!({"pattern":"**/*kernel*"}),
        &["kernel.rs"],
    );
}
