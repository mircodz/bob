use super::*;
use bob_core::core::project_context::ProjectContext;
use std::fs;
use std::path::PathBuf;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("bob-instruction-parity-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }

    fn write(&self, path: &str, text: impl AsRef<[u8]>) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn load(&self, path: &str) -> InstructionReport {
        load(
            &self.0,
            &Directory::open(&self.0.join(path)).unwrap(),
            || false,
        )
        .unwrap()
    }

    fn assert_production(&self, path: &str) {
        let report = self.load(path);
        assert!(!report.truncated && report.failures.is_empty());
        let context = ProjectContext {
            root: self.0.clone(),
            cwd: self.0.join(path),
        };
        let production = context.instructions_for(&context.cwd).unwrap();
        assert_eq!(report.sources.len(), production.len());
        for (actual, expected) in report.sources.iter().zip(production) {
            assert_eq!(
                actual.source,
                relative_name(&self.0, &expected.path).unwrap()
            );
            assert_eq!(self.0.join(&actual.scope), expected.scope);
            assert_eq!(actual.text, expected.content);
            assert_eq!(actual.sha256, digest(&fs::read(expected.path).unwrap()));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn first_nonempty_convention_matches_production_at_each_scope() {
    let fixture = Fixture::new();
    fixture.write("AGENTS.md", "  agents root \n");
    fixture.write("CLAUDE.md", "not selected");
    fixture.write(".bob/AGENTS.md", "not selected either");
    fixture.write("child/AGENTS.md", " \n\t");
    fixture.write("child/CLAUDE.md", "  child claude \n");
    fixture.write("child/.bob/AGENTS.md", "not selected child");
    fixture.write("child/deep/AGENTS.md", "\n");
    fixture.write("child/deep/CLAUDE.md", "\t");
    fixture.write("child/deep/.bob/AGENTS.md", "  bob deep \n");
    fixture.write("sibling/AGENTS.md", "sibling only");
    fixture.assert_production("child/deep");
    let report = fixture.load("child/deep");
    assert_eq!(
        report
            .sources
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>(),
        ["agents root", "child claude", "bob deep"]
    );
    assert_eq!(report.sources[2].scope, "child/deep");
}

#[test]
fn instruction_file_and_total_byte_boundaries_match_production() {
    let fixture = Fixture::new();
    for path in [
        "AGENTS.md",
        "a/AGENTS.md",
        "a/b/AGENTS.md",
        "a/b/c/AGENTS.md",
    ] {
        fixture.write(path, "é".repeat(FILE_LIMIT / 2));
    }
    fixture.assert_production("a/b/c");
    fixture.write("a/b/c/d/AGENTS.md", "x");
    let report = fixture.load("a/b/c/d");
    assert!(report.truncated && report.failures[0].contains("262144"));
    assert_eq!(report.sources.len(), 4);
    fixture.write("AGENTS.md", "x".repeat(FILE_LIMIT + 1));
    fixture.write(
        "CLAUDE.md",
        "must not silently replace failed higher priority file",
    );
    let report = fixture.load(".");
    assert!(report.truncated && report.failures[0].contains("65536"));
    assert!(report.sources.is_empty());
}

#[test]
fn empty_candidates_still_count_toward_aggregate_read_budget() {
    let fixture = Fixture::new();
    for path in [
        "AGENTS.md",
        "CLAUDE.md",
        ".bob/AGENTS.md",
        "child/AGENTS.md",
    ] {
        fixture.write(path, " ".repeat(FILE_LIMIT));
    }
    fixture.write("child/CLAUDE.md", "x");
    let report = fixture.load("child");
    assert!(report.sources.is_empty());
    assert!(report.truncated && report.failures[0].contains("262144"));
}

#[test]
fn nested_capability_root_never_imports_parent_or_sibling_material() {
    let fixture = Fixture::new();
    fixture.write("AGENTS.md", "outside capability");
    fixture.write(".bob/AGENTS.md", "outside fallback");
    fixture.write("nested/child/AGENTS.md", "inside");
    fixture.write("nested/sibling/AGENTS.md", "sibling");
    let root = fixture.0.join("nested");
    let report = load(
        &root,
        &Directory::open(&root.join("child")).unwrap(),
        || false,
    )
    .unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].source, "child/AGENTS.md");
    assert_eq!(report.sources[0].text, "inside");
    assert!(CONTRACT.contains("no files above the explicitly prepared filesystem root"));
}

#[test]
fn trusted_nested_launch_matches_production_sources_without_expanding_effect_root() {
    let fixture = Fixture::new();
    fixture.write("AGENTS.md", "outside repository");
    fixture.write("repo/.git/config", "");
    fixture.write("repo/AGENTS.md", "root conventions");
    fixture.write("repo/a/AGENTS.md", " \n");
    fixture.write("repo/a/CLAUDE.md", "parent conventions");
    fixture.write("repo/a/deep/.bob/AGENTS.md", "launch conventions");
    fixture.write("repo/sibling/AGENTS.md", "sibling conventions");
    let launch = fixture.0.join("repo/a/deep");
    let discovery = DiscoveryContext::from_workspace(&launch, None).unwrap();
    let report = load_with_context(
        &launch,
        &Directory::open(&launch).unwrap(),
        &discovery,
        || false,
    )
    .unwrap();
    let production = ProjectContext::discover(&launch)
        .unwrap()
        .instructions_for(&launch)
        .unwrap();
    assert!(!report.truncated && report.failures.is_empty());
    assert_eq!(report.root, launch.display().to_string());
    assert_eq!(
        report.discovery_root,
        Some(fixture.0.join("repo").display().to_string())
    );
    assert_eq!(report.sources.len(), production.len());
    for (actual, expected) in report.sources.iter().zip(production) {
        assert_eq!(launch.join(&actual.source), expected.path);
        assert_eq!(launch.join(&actual.scope), expected.scope);
        assert_eq!(actual.text, expected.content);
    }
    assert!(crate::workspace::workspace_directory(&launch, "..").is_err());
    assert!(load_with_context(
        &launch,
        &Directory::open(&fixture.0.join("repo")).unwrap(),
        &discovery,
        || false
    )
    .is_err());
}

#[test]
fn configured_home_only_loads_global_bob_with_project_scope_and_lower_priority() {
    let fixture = Fixture::new();
    let home = Fixture::new();
    home.write("AGENTS.md", "not a global convention");
    home.write("CLAUDE.md", "not global either");
    home.write(".bob/AGENTS.md", "global conventions");
    fixture.write(".git/config", "");
    fixture.write(
        "AGENTS.md",
        format!(
            "project rules; import {}",
            home.0.join("CLAUDE.md").display()
        ),
    );
    fixture.write("nested/CLAUDE.md", "nested conventions");
    let launch = fixture.0.join("nested");
    let discovery = DiscoveryContext::from_workspace(&launch, Some(&home.0)).unwrap();
    let report = load_with_context(
        &launch,
        &Directory::open(&launch).unwrap(),
        &discovery,
        || false,
    )
    .unwrap();
    assert!(!report.truncated);
    assert_eq!(report.sources.len(), 3);
    assert_eq!(
        report.sources[0].source,
        home.0.join(".bob/AGENTS.md").display().to_string()
    );
    assert_eq!(report.sources[0].scope, fixture.0.display().to_string());
    assert_eq!(report.sources[0].text, "global conventions");
    assert_eq!(report.sources[2].text, "nested conventions");
    assert!(!report
        .sources
        .iter()
        .any(|source| source.text == "not global either"));
    let without_home = DiscoveryContext::from_workspace(&launch, None).unwrap();
    assert_eq!(
        load_with_context(
            &launch,
            &Directory::open(&launch).unwrap(),
            &without_home,
            || false
        )
        .unwrap()
        .sources
        .len(),
        2
    );
}

#[test]
fn arbitrary_effect_root_cannot_become_a_discovery_workspace() {
    let launch = Fixture::new();
    let outside = Fixture::new();
    outside.write(".git/config", "");
    outside.write("AGENTS.md", "unapproved ancestor");
    outside.write("nested/AGENTS.md", "explicit effect root");
    let root = outside.0.join("nested");
    let discovery = DiscoveryContext::from_workspace(&launch.0, None).unwrap();
    let report = load_with_context(&root, &Directory::open(&root).unwrap(), &discovery, || {
        false
    })
    .unwrap();
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].text, "explicit effect root");
}

#[test]
fn worktree_boundary_is_discovered_without_reading_external_gitdir() {
    let fixture = Fixture::new();
    fixture.write("AGENTS.md", "outer repository");
    fixture.write(".git/config", "");
    fixture.write("linked/.git", "gitdir: /unapproved/not-opened\n");
    fixture.write("linked/AGENTS.md", "worktree");
    fixture.write("linked/deep/CLAUDE.md", "launch");
    let launch = fixture.0.join("linked/deep");
    let discovery = DiscoveryContext::from_workspace(&launch, None).unwrap();
    let report = load_with_context(
        &launch,
        &Directory::open(&launch).unwrap(),
        &discovery,
        || false,
    )
    .unwrap();
    assert_eq!(
        report
            .sources
            .iter()
            .map(|source| source.text.as_str())
            .collect::<Vec<_>>(),
        ["worktree", "launch"]
    );
    assert_eq!(
        report.discovery_root,
        Some(
            ProjectContext::discover(&launch)
                .unwrap()
                .root
                .display()
                .to_string()
        )
    );
}

#[cfg(unix)]
#[test]
fn ancestor_and_global_symlinks_and_replaced_workspace_are_rejected() {
    use std::os::unix::fs::symlink;
    let outside = Fixture::new();
    outside.write("AGENTS.md", "outside");
    let fixture = Fixture::new();
    fixture.write(".git/config", "");
    fixture.write("nested/file", "");
    symlink(&outside.0, fixture.0.join(".bob")).unwrap();
    let home = Fixture::new();
    symlink(&outside.0, home.0.join(".bob")).unwrap();
    let launch = fixture.0.join("nested");
    let discovery = DiscoveryContext::from_workspace(&launch, Some(&home.0)).unwrap();
    let report = load_with_context(
        &launch,
        &Directory::open(&launch).unwrap(),
        &discovery,
        || false,
    )
    .unwrap();
    assert!(report.sources.is_empty());
    assert_eq!(report.failures.len(), 2);
    fs::rename(&launch, fixture.0.join("old-nested")).unwrap();
    fs::create_dir(&launch).unwrap();
    assert!(load_with_context(
        &launch,
        &Directory::open(&launch).unwrap(),
        &discovery,
        || false
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn bob_directory_and_instruction_symlinks_are_not_followed() {
    use std::os::unix::fs::symlink;
    let outside = Fixture::new();
    outside.write("AGENTS.md", "outside");
    for directory_link in [false, true] {
        let fixture = Fixture::new();
        if directory_link {
            symlink(&outside.0, fixture.0.join(".bob")).unwrap();
        } else {
            fs::create_dir(fixture.0.join(".bob")).unwrap();
            symlink(
                outside.0.join("AGENTS.md"),
                fixture.0.join(".bob/AGENTS.md"),
            )
            .unwrap();
        }
        let report = fixture.load(".");
        assert!(report.sources.is_empty());
        assert!(report.truncated && !report.failures.is_empty());
        assert!(!render(&report).contains("> outside"));
    }
}
