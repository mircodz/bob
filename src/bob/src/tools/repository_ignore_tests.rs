use super::*;
use serde_json::json;
use std::collections::BTreeSet;
use std::fs;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob-ignore-parity-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }

    fn write(&self, path: &str, text: impl AsRef<[u8]>) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn search(&self, path: &str) -> SearchReport {
        search(&self.0, path)
    }

    fn assert_oracle(&self, scope: &str) -> BTreeSet<String> {
        let report = self.search(scope);
        assert!(!report.truncated, "{}", render(&report));
        let actual = report
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect::<BTreeSet<_>>();
        let expected = ignore::WalkBuilder::new(&self.0)
            .hidden(false)
            .git_global(false)
            .parents(false)
            .build()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .filter(|entry| entry.path().starts_with(self.0.join(scope)))
            .filter(|entry| entry.path().extension() == Some(OsStr::new("txt")))
            .map(|entry| relative_name(&self.0, entry.path()).unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        actual
    }
}

fn search(root: &Path, scope: &str) -> SearchReport {
    let input = json!({"pattern":"**/*.txt", "path":scope});
    let Action::Repository {
        path, ancestors, ..
    } = prepare(root, "glob", input.clone()).unwrap()
    else {
        panic!("not a repository action");
    };
    let (_sender, cancel) = watch::channel(false);
    execute(root, "glob", input, &path, &ancestors, &cancel).unwrap()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn parent_ignore_gitignore_and_repo_exclude_precedence_match_ripgrep_walker() {
    let fixture = Fixture::new();
    fixture.write(".git/info/exclude", "*.txt\n");
    fixture.write(
        ".gitignore",
        "!git-kept.txt\nnested/*.txt\n!nested/parent-kept.txt\n",
    );
    fixture.write(".ignore", "!ignore-kept.txt\nnested/force.txt\n");
    fixture.write("nested/.gitignore", "!local.txt\n!force.txt\n");
    for name in [
        "git-kept.txt",
        "ignore-kept.txt",
        "excluded.txt",
        "nested/parent-kept.txt",
        "nested/local.txt",
        "nested/force.txt",
        "nested/excluded.txt",
    ] {
        fixture.write(name, "hit");
    }
    let actual = fixture.assert_oracle(".");
    assert_eq!(
        actual,
        BTreeSet::from([
            "git-kept.txt".into(),
            "ignore-kept.txt".into(),
            "nested/local.txt".into(),
            "nested/parent-kept.txt".into()
        ])
    );
    fixture.assert_oracle("nested");
    fixture.write("nested/.ignore", "!force.txt\n");
    assert!(fixture.assert_oracle("nested").contains("nested/force.txt"));
}

#[test]
fn nested_repository_resets_git_rules_but_inherits_ignore_rules() {
    let fixture = Fixture::new();
    fixture.write(".git/info/exclude", "outer-excluded.txt\n");
    fixture.write(".gitignore", "*.txt\n");
    fixture.write(".ignore", "blocked.txt\n");
    fixture.write("nested/.git/info/exclude", "inner-kept.txt\n");
    fixture.write("nested/.gitignore", "!inner-kept.txt\n");
    for name in [
        "ordinary.txt",
        "outer-excluded.txt",
        "inner-kept.txt",
        "blocked.txt",
    ] {
        fixture.write(&format!("nested/{name}"), "hit");
    }
    let actual = fixture.assert_oracle("nested");
    assert_eq!(actual.len(), 3);
    assert!(!actual.contains("nested/blocked.txt"));
}

#[test]
fn worktree_commondir_rules_are_rooted_at_worktree_not_metadata() {
    let fixture = Fixture::new();
    fixture.write(".git/worktrees/linked/commondir", "../..\n");
    fixture.write(".git/info/exclude", "/hidden.txt\n");
    fixture.write(
        "linked/.git",
        format!(
            "gitdir: {}\n",
            fixture.0.join(".git/worktrees/linked").display()
        ),
    );
    fixture.write("linked/hidden.txt", "hit");
    fixture.write("linked/visible.txt", "hit");
    fixture.write("linked/sub/hidden.txt", "hit");
    let actual = fixture.assert_oracle("linked");
    assert_eq!(
        actual,
        BTreeSet::from(["linked/visible.txt".into(), "linked/sub/hidden.txt".into()])
    );
    fixture.write("linked/.git", "gitdir: ../.git/worktrees/linked\n");
    let report = fixture.search("linked");
    assert!(!report.truncated, "{}", render(&report));
    assert_eq!(
        report
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<BTreeSet<_>>(),
        actual
    );
}

#[test]
fn external_git_metadata_is_reported_without_reading_or_widening_scope() {
    let outside = Fixture::new();
    outside.write("info/exclude", "visible.txt\n");
    for common in [false, true] {
        let fixture = Fixture::new();
        if common {
            fixture.write(".git", "gitdir: metadata\n");
            fixture.write("metadata/commondir", outside.0.to_str().unwrap());
        } else {
            fixture.write(".git", format!("gitdir: {}\n", outside.0.display()));
        }
        fixture.write("visible.txt", "hit");
        let report = fixture.search(".");
        assert!(report.truncated);
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("outside the prepared filesystem root")));
        assert!(report
            .entries
            .iter()
            .any(|entry| entry.path == "visible.txt"));
        assert_eq!(report.next_offset, None);
    }
}

#[test]
fn capability_root_does_not_import_ancestor_ignore_sources() {
    let fixture = Fixture::new();
    fixture.write(".ignore", "*.txt\n");
    fixture.write(".gitignore", "*.txt\n");
    fixture.write(".git/info/exclude", "*.txt\n");
    fixture.write("nested/visible.txt", "hit");
    let report = search(&fixture.0.join("nested"), ".");
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].path, "visible.txt");
    assert_eq!(report.bytes_read, 0);
}

#[test]
fn git_exclude_and_pointer_reads_are_bounded_and_list_reads_no_metadata() {
    for path in [".git/info/exclude", ".git", "metadata/commondir"] {
        let fixture = Fixture::new();
        if path == "metadata/commondir" {
            fixture.write(".git", "gitdir: metadata\n");
        }
        fixture.write(path, "x".repeat(16 * 1024 + 1));
        fixture.write("visible.txt", "hit");
        let report = fixture.search(".");
        assert!(report.truncated && !report.errors.is_empty());
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("16384-byte")));
        let input = json!({});
        let Action::Repository {
            path, ancestors, ..
        } = prepare(&fixture.0, "list_dir", input.clone()).unwrap()
        else {
            panic!()
        };
        let (_sender, cancel) = watch::channel(false);
        let report = execute(&fixture.0, "list_dir", input, &path, &ancestors, &cancel).unwrap();
        assert!(!report.truncated);
        assert_eq!(report.bytes_read, 0);
    }
}

fn search_with_context(root: &Path, context: &DiscoveryContext) -> SearchReport {
    let input = json!({"pattern":"**/*.txt"});
    let Action::Repository {
        path, ancestors, ..
    } = prepare(root, "glob", input.clone()).unwrap()
    else {
        panic!()
    };
    let (_sender, cancel) = watch::channel(false);
    execute_with_context(root, "glob", input, &path, &ancestors, &cancel, context).unwrap()
}

#[test]
fn trusted_nested_launch_imports_parent_rules_and_matches_production_walker() {
    let fixture = Fixture::new();
    fixture.write("repo/.git/info/exclude", "/a/deep/excluded.txt\n");
    fixture.write("repo/.gitignore", "*.txt\n!parent.txt\n!excluded.txt\n");
    fixture.write("repo/.ignore", "blocked.txt\n");
    fixture.write("repo/a/.gitignore", "!local.txt\n");
    fixture.write("repo/a/deep/.gitignore", "!blocked.txt\n");
    for name in [
        "parent.txt",
        "local.txt",
        "excluded.txt",
        "blocked.txt",
        "hidden.txt",
    ] {
        fixture.write(&format!("repo/a/deep/{name}"), "hit");
    }
    fixture.write("repo/sibling/never-returned.txt", "hit");
    fixture.write(".ignore", "local.txt\n");
    let launch = fixture.0.join("repo/a/deep");
    let context = DiscoveryContext::from_workspace(&launch, None).unwrap();
    let report = search_with_context(&launch, &context);
    assert!(!report.truncated, "{}", render(&report));
    let actual = report
        .entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    let expected = ignore::WalkBuilder::new(fixture.0.join("repo"))
        .hidden(false)
        .git_global(false)
        .parents(false)
        .build()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter(|entry| {
            entry.path().starts_with(&launch) && entry.path().extension() == Some(OsStr::new("txt"))
        })
        .map(|entry| relative_name(&launch, entry.path()).unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
    assert_eq!(
        actual,
        BTreeSet::from([
            "excluded.txt".into(),
            "parent.txt".into(),
            "local.txt".into()
        ])
    );
    assert!(prepare(&launch, "glob", json!({"pattern":"**", "path":".."})).is_err());
    assert!(prepare(
        &launch,
        "glob",
        json!({"pattern":"**", "path":fixture.0.join("repo/sibling")})
    )
    .is_err());
}

#[test]
fn nested_launch_reads_in_boundary_worktree_common_exclude() {
    let fixture = Fixture::new();
    fixture.write(
        ".git",
        format!(
            "gitdir: {}\n",
            fixture.0.join("metadata/worktrees/current").display()
        ),
    );
    fixture.write("metadata/worktrees/current/commondir", "../..\n");
    fixture.write("metadata/info/exclude", "/nested/hidden.txt\n");
    fixture.write("nested/hidden.txt", "hit");
    fixture.write("nested/visible.txt", "hit");
    let launch = fixture.0.join("nested");
    let context = DiscoveryContext::from_workspace(&launch, None).unwrap();
    let report = search_with_context(&launch, &context);
    assert!(!report.truncated, "{}", render(&report));
    assert_eq!(
        report
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["visible.txt"]
    );
    assert!(report.preparation_required.is_empty());
    let expected = ignore::WalkBuilder::new(&launch)
        .hidden(false)
        .git_global(false)
        .build()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| relative_name(&launch, entry.path()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(expected, ["visible.txt"]);
    fixture.write(".git", "gitdir: metadata/worktrees/current\n");
    let relative = search_with_context(&launch, &context);
    assert!(!relative.truncated, "{}", render(&relative));
    assert_eq!(
        relative
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn external_worktree_metadata_requests_exact_roots_without_reading_them() {
    let fixture = Fixture::new();
    let outside = Fixture::new();
    outside.write("info/exclude", "visible.txt\n");
    fixture.write(".git", format!("gitdir: {}\n", outside.0.display()));
    fixture.write("nested/visible.txt", "hit");
    let launch = fixture.0.join("nested");
    let context = DiscoveryContext::from_workspace(&launch, None).unwrap();
    let report = search_with_context(&launch, &context);
    assert!(report.truncated && !report.errors.is_empty());
    assert_eq!(
        report.preparation_required,
        [GitMetadataPreparation {
            source: fixture.0.join(".git"),
            root: outside.0.clone(),
            kind: GitMetadataKind::GitDir
        }]
    );
    assert_eq!(
        report.bytes_read,
        fs::metadata(fixture.0.join(".git")).unwrap().len() as usize
    );
    fixture.write(".git", "gitdir: metadata\n");
    fixture.write("metadata/commondir", format!("{}\n", outside.0.display()));
    let report = search_with_context(&launch, &context);
    assert_eq!(
        report.preparation_required,
        [GitMetadataPreparation {
            source: fixture.0.join("metadata/commondir"),
            root: outside.0.clone(),
            kind: GitMetadataKind::CommonDir
        }]
    );
    assert_eq!(
        report.bytes_read,
        (fs::metadata(fixture.0.join(".git")).unwrap().len()
            + fs::metadata(fixture.0.join("metadata/commondir"))
                .unwrap()
                .len()) as usize
    );
}

#[test]
fn metadata_path_cannot_leave_and_reenter_the_discovery_boundary() {
    let fixture = Fixture::new();
    fixture.write("metadata/info/exclude", "*.txt\n");
    fixture.write(
        ".git",
        format!(
            "gitdir: ../{}/metadata\n",
            fixture.0.file_name().unwrap().to_str().unwrap()
        ),
    );
    fixture.write("nested/visible.txt", "hit");
    let launch = fixture.0.join("nested");
    let context = DiscoveryContext::from_workspace(&launch, None).unwrap();
    let report = search_with_context(&launch, &context);
    assert!(report.truncated && !report.errors.is_empty());
    assert!(report.preparation_required.is_empty());
    assert_eq!(
        report.bytes_read,
        fs::metadata(fixture.0.join(".git")).unwrap().len() as usize
    );
}

#[test]
fn unrelated_effect_root_cannot_import_its_own_ancestors_through_launch_context() {
    let fixture = Fixture::new();
    fixture.write(".git/info/exclude", "*.txt\n");
    fixture.write(".ignore", "*.txt\n");
    fixture.write("nested/visible.txt", "hit");
    let launch = Fixture::new();
    let context = DiscoveryContext::from_workspace(&launch.0, None).unwrap();
    let report = search_with_context(&fixture.0.join("nested"), &context);
    assert!(!report.truncated);
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].path, "visible.txt");
    assert_eq!(report.bytes_read, 0);
}

#[cfg(unix)]
#[test]
fn nested_launch_does_not_follow_ancestor_ignore_or_metadata_symlinks() {
    use std::os::unix::fs::symlink;
    let outside = Fixture::new();
    outside.write("rules", "*.txt\n");
    outside.write("info/exclude", "*.txt\n");
    for source in [".ignore", ".gitignore", ".git/info", "metadata"] {
        let fixture = Fixture::new();
        fixture.write("nested/visible.txt", "hit");
        let target = if source == "metadata" {
            fixture.write(".git", "gitdir: metadata\n");
            outside.0.clone()
        } else {
            fs::create_dir(fixture.0.join(".git")).unwrap();
            if source == ".git/info" {
                outside.0.join("info")
            } else {
                outside.0.join("rules")
            }
        };
        symlink(target, fixture.0.join(source)).unwrap();
        let launch = fixture.0.join("nested");
        let context = DiscoveryContext::from_workspace(&launch, None).unwrap();
        let report = search_with_context(&launch, &context);
        assert!(report.truncated && !report.errors.is_empty(), "{source}");
        assert!(
            report.preparation_required.is_empty(),
            "symlinks must fail, not request broader approval"
        );
    }
}

#[cfg(unix)]
#[test]
fn git_metadata_symlinks_and_parent_traversal_never_escape() {
    use std::os::unix::fs::symlink;
    let outside = Fixture::new();
    outside.write("info/exclude", "visible.txt\n");
    outside.write("commondir", outside.0.to_str().unwrap());
    for link in [
        ".git",
        ".git/info",
        ".git/info/exclude",
        "metadata",
        "metadata/commondir",
    ] {
        let fixture = Fixture::new();
        fixture.write("visible.txt", "hit");
        let target = match link {
            ".git/info/exclude" => outside.0.join("info/exclude"),
            ".git/info" => outside.0.join("info"),
            "metadata/commondir" => outside.0.join("commondir"),
            _ => outside.0.clone(),
        };
        if link.starts_with("metadata") {
            fixture.write(".git", "gitdir: metadata\n");
        }
        fs::create_dir_all(fixture.0.join(link).parent().unwrap()).unwrap();
        symlink(target, fixture.0.join(link)).unwrap();
        let report = fixture.search(".");
        assert!(report.truncated && !report.errors.is_empty(), "{link}");
        assert!(report
            .entries
            .iter()
            .any(|entry| entry.path == "visible.txt"));
    }
    let fixture = Fixture::new();
    fixture.write(".git", "gitdir: ../outside\n");
    fixture.write("visible.txt", "hit");
    let report = fixture.search(".");
    assert!(report
        .errors
        .iter()
        .any(|error| error.contains("outside the prepared filesystem root")));
}
