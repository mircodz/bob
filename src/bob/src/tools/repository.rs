use super::{cancellation_requested, Action};
use crate::native_fs::{Directory, EntryKind};
use crate::project_instructions::DiscoveryContext;
use crate::workspace::{
    modification, relative_name, workspace_directory, workspace_target, READ_LIMIT,
};
use anyhow::{bail, ensure, Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::watch;

const ENTRY_LIMIT: usize = 10_000;
const BYTE_LIMIT: usize = 16 * 1024 * 1024;
const OUTPUT_LIMIT: usize = 256 * 1024;
const REPORT_RESERVE: usize = 32 * 1024;
const DEPTH_LIMIT: usize = 32;
const TIME_LIMIT: Duration = Duration::from_secs(5);

#[cfg(test)]
type RaceHook = std::cell::RefCell<Option<Box<dyn FnOnce()>>>;
#[cfg(test)]
thread_local! {
    pub(super) static SEARCH_READ_HOOK: RaceHook = const { std::cell::RefCell::new(None) };
}

fn default_path() -> String {
    ".".into()
}
fn default_results() -> usize {
    200
}
fn default_list_results() -> usize {
    2000
}
fn default_glob_results() -> usize {
    500
}
fn default_offset() -> usize {
    1
}

fn integer<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<usize, D::Error> {
    let value = Value::deserialize(deserializer)?;
    let number = value
        .as_u64()
        .and_then(|number| usize::try_from(number).ok())
        .or_else(|| {
            value
                .as_f64()
                .filter(|number| {
                    number.is_finite()
                        && *number >= 0.0
                        && number.fract() == 0.0
                        && *number < usize::MAX as f64
                })
                .map(|number| number as usize)
        });
    number.ok_or_else(|| {
        serde::de::Error::custom("expected a nonnegative integer within platform range")
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListInput {
    #[serde(default = "default_path")]
    path: String,
    #[serde(default = "default_list_results", deserialize_with = "integer")]
    max_results: usize,
    #[serde(default = "default_offset", deserialize_with = "integer")]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobInput {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default = "default_glob_results", deserialize_with = "integer")]
    max_results: usize,
    #[serde(default = "default_offset", deserialize_with = "integer")]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepInput {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default = "default_results", deserialize_with = "integer")]
    max_results: usize,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    ignore_case: bool,
    glob: Option<String>,
    #[serde(default, deserialize_with = "integer")]
    context: usize,
    #[serde(default)]
    multiline: bool,
    #[serde(default)]
    output_mode: OutputMode,
    #[serde(default = "default_offset", deserialize_with = "integer")]
    offset: usize,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    Content,
    FilesWithMatches,
    Count,
}

struct FileGlob {
    matcher: GlobMatcher,
    negated: bool,
}

impl FileGlob {
    fn matches(&self, path: &Path) -> bool {
        self.matcher.is_match(path) != self.negated
    }
}

fn file_glob(pattern: &str) -> Result<Option<FileGlob>> {
    if pattern.is_empty() {
        return Ok(None);
    }
    let (negated, pattern) = pattern
        .strip_prefix('!')
        .map_or((false, pattern), |pattern| (true, pattern));
    let pattern = if pattern.contains('/') {
        pattern.to_owned()
    } else {
        format!("**/{pattern}")
    };
    Ok(Some(FileGlob {
        matcher: glob(&pattern)?,
        negated,
    }))
}

enum Query {
    List,
    Glob(GlobMatcher),
    Grep {
        regex: Regex,
        glob: Option<FileGlob>,
        context: usize,
        multiline: bool,
        output_mode: OutputMode,
    },
}

struct Search {
    path: String,
    max_results: usize,
    offset: usize,
    query: Query,
}

fn glob(pattern: &str) -> Result<GlobMatcher> {
    ensure!(
        !pattern.is_empty() && pattern.len() <= 4096,
        "glob pattern must contain 1..4096 bytes"
    );
    ensure!(
        !Path::new(pattern).is_absolute()
            && !Path::new(pattern)
                .components()
                .any(|part| part == Component::ParentDir),
        "glob pattern must be relative to path, without '..'"
    );
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

fn parse(name: &str, input: Value) -> Result<Search> {
    let search = match name {
        "list_dir" => {
            let input: ListInput = serde_json::from_value(input)?;
            Search {
                path: input.path,
                max_results: input.max_results,
                offset: input.offset,
                query: Query::List,
            }
        }
        "glob" => {
            let input: GlobInput = serde_json::from_value(input)?;
            Search {
                path: input.path,
                max_results: input.max_results,
                offset: input.offset,
                query: Query::Glob(glob(&input.pattern)?),
            }
        }
        "grep" => {
            let input: GrepInput = serde_json::from_value(input)?;
            ensure!(
                !input.pattern.is_empty() && input.pattern.len() <= 4096,
                "grep pattern must contain 1..4096 bytes"
            );
            ensure!(input.context <= 2000, "context must not exceed 2000 lines");
            let pattern = if input.literal {
                regex::escape(&input.pattern)
            } else {
                input.pattern
            };
            let regex = RegexBuilder::new(&pattern)
                .case_insensitive(input.ignore_case)
                .multi_line(input.multiline)
                .dot_matches_new_line(input.multiline)
                .size_limit(1024 * 1024)
                .dfa_size_limit(1024 * 1024)
                .build()?;
            Search {
                path: input.path,
                max_results: input.max_results,
                offset: input.offset,
                query: Query::Grep {
                    regex,
                    glob: input.glob.as_deref().map(file_glob).transpose()?.flatten(),
                    context: input.context,
                    multiline: input.multiline,
                    output_mode: input.output_mode,
                },
            }
        }
        _ => bail!("invalid repository tool"),
    };
    ensure!(
        (1..=2000).contains(&search.max_results),
        "max_results must be from 1 to 2000"
    );
    ensure!(search.offset > 0, "offset must be a positive result index");
    Ok(search)
}

fn search_scope(root: &Path, search: &Search) -> Result<(Directory, PathBuf)> {
    match workspace_directory(root, &search.path) {
        Ok(directory) => {
            let path = directory.path().to_path_buf();
            Ok((directory, path))
        }
        Err(error) if matches!(search.query, Query::Grep { .. }) => {
            let requested = workspace_target(root, &search.path)?;
            let (directory, _) = crate::native_fs::open_regular(&requested).with_context(|| {
                format!("search path is neither a directory nor a regular file: {error}")
            })?;
            let path = directory
                .path()
                .join(requested.file_name().context("missing filename")?);
            Ok((directory, path))
        }
        Err(error) => Err(error),
    }
}

pub(super) fn prepare(root: &Path, name: &str, input: Value) -> Result<Action> {
    let search = parse(name, input)?;
    let (directory, path) = search_scope(root, &search)?;
    ensure!(
        directory.path().strip_prefix(root)?.components().count() <= DEPTH_LIMIT,
        "search scope exceeds directory depth limit"
    );
    Ok(Action::Repository {
        path,
        ancestors: directory.identities()?,
        metadata: Vec::new(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum GitMetadataKind {
    GitDir,
    CommonDir,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitMetadataPreparation {
    pub source: PathBuf,
    pub root: PathBuf,
    pub kind: GitMetadataKind,
}

impl std::fmt::Display for GitMetadataPreparation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Git metadata is outside the prepared filesystem root and trusted discovery boundary; fresh approval required for {:?} from {:?}", self.root, self.source)
    }
}

impl std::error::Error for GitMetadataPreparation {}

const METADATA_FILE_LIMIT: usize = 16 * 1024;
const METADATA_TOTAL_LIMIT: usize = 256 * 1024;
const METADATA_CHAIN_LIMIT: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MetadataNode {
    Directory {
        path: PathBuf,
        ancestors: Value,
    },
    File {
        path: PathBuf,
        snapshot: crate::workspace::Snapshot,
    },
    Absent {
        path: PathBuf,
        ancestors: Value,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreparedGitMetadata {
    scope: PathBuf,
    nodes: Vec<MetadataNode>,
    pub requirements: Vec<GitMetadataPreparation>,
    exclude: Option<PathBuf>,
}

impl PreparedGitMetadata {
    fn directory(&mut self, directory: &Directory) -> Result<()> {
        directory.verify()?;
        self.nodes.push(MetadataNode::Directory {
            path: directory.path().to_path_buf(),
            ancestors: directory.identities()?,
        });
        Ok(())
    }

    fn exists(&mut self, directory: &Directory, name: &str) -> Result<bool> {
        if directory.name_exists(OsStr::new(name))? {
            return Ok(true);
        }
        self.nodes.push(MetadataNode::Absent {
            path: directory.path().join(name),
            ancestors: directory.identities()?,
        });
        directory.verify()?;
        Ok(false)
    }

    fn read(&mut self, directory: &Directory, name: &str, bytes: &mut usize) -> Result<String> {
        let (text, snapshot) = metadata_snapshot(directory, OsStr::new(name), None)?;
        *bytes += text.len();
        ensure!(
            *bytes <= METADATA_TOTAL_LIMIT,
            "Git metadata preparation exceeds byte limit; narrow search scope"
        );
        self.nodes.push(MetadataNode::File {
            path: directory.path().join(name),
            snapshot,
        });
        Ok(text)
    }

    fn verify(&self) -> Result<Option<String>> {
        let mut exclude = None;
        for node in &self.nodes {
            match node {
                MetadataNode::Directory { path, ancestors } => {
                    let directory = Directory::open(path)?;
                    ensure!(
                        directory.identities()? == *ancestors,
                        "Git metadata directory changed; prepare a new revision for fresh approval"
                    );
                }
                MetadataNode::Absent { path, ancestors } => {
                    let directory =
                        Directory::open(path.parent().context("missing metadata parent")?)?;
                    ensure!(
                        directory.identities()? == *ancestors,
                        "Git metadata parent changed; prepare a new revision for fresh approval"
                    );
                    ensure!(
                        !directory
                            .name_exists(path.file_name().context("missing metadata name")?)?,
                        "new Git metadata appeared; prepare a new revision for fresh approval"
                    );
                    directory.verify()?;
                }
                MetadataNode::File { path, snapshot } => {
                    let directory =
                        Directory::open(path.parent().context("missing metadata parent")?)?;
                    let (text, _) = metadata_snapshot(
                        &directory,
                        path.file_name().context("missing metadata name")?,
                        Some(snapshot),
                    )?;
                    if self.exclude.as_ref() == Some(path) {
                        exclude = Some(text);
                    }
                }
            }
        }
        Ok(exclude)
    }
}

fn metadata_snapshot(
    directory: &Directory,
    name: &OsStr,
    expected: Option<&crate::workspace::Snapshot>,
) -> Result<(String, crate::workspace::Snapshot)> {
    let file = directory.open_file(name)?;
    let ancestors = directory.identities()?;
    if let Some(expected) = expected {
        ensure!(
            ancestors == expected.ancestors
                && crate::native_fs::identity(&file)? == expected.identity,
            "Git metadata identity changed; prepare a new revision for fresh approval"
        );
        let metadata = file.metadata()?;
        ensure!(
            metadata.len() == expected.len && modification(&metadata) == expected.modified,
            "Git metadata changed; prepare a new revision for fresh approval"
        );
    }
    let (text, snapshot) =
        crate::workspace::read_open_snapshot_limited(file, ancestors, METADATA_FILE_LIMIT)?;
    ensure!(
        expected.is_none_or(|expected| expected == &snapshot),
        "Git metadata snapshot changed; prepare a new revision for fresh approval"
    );
    ensure!(
        crate::native_fs::identity(&directory.open_file(name)?)? == snapshot.identity,
        "Git metadata changed during snapshot read; prepare a new revision for fresh approval"
    );
    directory.verify()?;
    Ok((text, snapshot))
}

fn pointer_directory(base: &Directory, text: &str) -> Result<Directory> {
    ensure!(
        !text.is_empty() && text.len() <= 4096,
        "invalid Git metadata pointer length"
    );
    let requested = base.path().join(text);
    ensure!(
        requested.is_absolute() && requested.components().count() <= DEPTH_LIMIT,
        "Git metadata pointer exceeds directory depth limit"
    );
    let mut directory = base.ancestor(base.path())?;
    if Path::new(text).is_absolute() {
        let root = requested
            .ancestors()
            .last()
            .context("missing metadata filesystem root")?;
        directory = Directory::open(root)?;
    }
    for part in Path::new(text).components() {
        directory = match part {
            Component::Normal(name) => directory.open_child(name)?,
            Component::ParentDir => directory.ancestor(
                directory
                    .path()
                    .parent()
                    .context("Git pointer escapes filesystem root")?,
            )?,
            Component::CurDir | Component::RootDir | Component::Prefix(_) => directory,
        };
    }
    directory.verify()?;
    Ok(directory)
}

fn pointer_line(text: &str) -> Result<&str> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    ensure!(
        !text.is_empty() && !text.contains(['\n', '\r', '\0']),
        "invalid Git metadata pointer"
    );
    Ok(text)
}

fn prepare_git_metadata(
    directory: &Directory,
    boundary: &Path,
    bytes: &mut usize,
) -> Result<Option<PreparedGitMetadata>> {
    if !directory.name_exists(OsStr::new(".git"))? {
        return Ok(None);
    }
    let mut prepared = PreparedGitMetadata {
        scope: directory.path().to_path_buf(),
        nodes: Vec::new(),
        requirements: Vec::new(),
        exclude: None,
    };
    prepared.directory(directory)?;
    let git = match directory.open_child(OsStr::new(".git")) {
        Ok(git) => git,
        Err(_) => {
            let text = prepared.read(directory, ".git", bytes)?;
            let pointer = pointer_line(&text)?
                .strip_prefix("gitdir: ")
                .context("invalid Git worktree pointer")?;
            let git = pointer_directory(directory, pointer)?;
            if !git.path().starts_with(boundary) {
                prepared.requirements.push(GitMetadataPreparation {
                    source: directory.path().join(".git"),
                    root: git.path().to_path_buf(),
                    kind: GitMetadataKind::GitDir,
                });
            }
            git
        }
    };
    prepared.directory(&git)?;
    let common = if prepared.exists(&git, "commondir")? {
        let text = prepared.read(&git, "commondir", bytes)?;
        let common = pointer_directory(&git, pointer_line(&text)?)?;
        ensure!(common.path() != git.path(), "cyclic Git commondir pointer");
        if !common.path().starts_with(boundary) {
            prepared.requirements.push(GitMetadataPreparation {
                source: git.path().join("commondir"),
                root: common.path().to_path_buf(),
                kind: GitMetadataKind::CommonDir,
            });
        }
        prepared.directory(&common)?;
        ensure!(
            !prepared.exists(&common, "commondir")?,
            "chained Git commondir pointers are unsupported; metadata was not ignored"
        );
        common
    } else {
        git
    };
    if prepared.exists(&common, "info")? {
        let info = common.open_child(OsStr::new("info"))?;
        prepared.directory(&info)?;
        if prepared.exists(&info, "exclude")? {
            prepared.read(&info, "exclude", bytes)?;
            prepared.exclude = Some(info.path().join("exclude"));
        }
    }
    prepared.verify()?;
    Ok(Some(prepared))
}

pub(super) fn prepare_with_context(
    root: &Path,
    name: &str,
    input: Value,
    context: &DiscoveryContext,
) -> Result<Action> {
    let mut action = prepare(root, name, input.clone())?;
    let search = parse(name, input)?;
    let (directory, selected) = search_scope(root, &search)?;
    if matches!(search.query, Query::List) || selected != directory.path() {
        return Ok(action);
    }
    let anchor = context.project_anchor(root, &directory)?;
    ensure!(
        directory
            .path()
            .strip_prefix(anchor.path())?
            .components()
            .count()
            <= DEPTH_LIMIT,
        "Git metadata discovery exceeds depth limit"
    );
    let Action::Repository { metadata, .. } = &mut action else {
        unreachable!()
    };
    let started = Instant::now();
    let mut bytes = 0;
    let mut ignore_bytes = 0;
    let mut inspect = |directory: &Directory, ignores: &mut IgnoreRules| -> Result<()> {
        ensure!(
            started.elapsed() < TIME_LIMIT,
            "Git metadata preparation exceeded time limit; narrow search scope"
        );
        let prepared = prepare_git_metadata(directory, anchor.path(), &mut bytes)?;
        ignores.load(prepared.is_some(), |name| {
            ensure!(
                started.elapsed() < TIME_LIMIT,
                "Git metadata preparation exceeded time limit; narrow search scope"
            );
            if name == ".git" {
                let prepared = prepared.as_ref().expect("repository boundary was detected");
                return prepared
                    .verify()?
                    .map(|text| compile_ignore(directory.path(), prepared.exclude.clone(), &text))
                    .transpose();
            }
            if !directory.name_exists(OsStr::new(name))? {
                return Ok(None);
            }
            let (text, _) = metadata_snapshot(directory, OsStr::new(name), None)?;
            ignore_bytes += text.len();
            ensure!(
                ignore_bytes + bytes <= BYTE_LIMIT,
                "ignore preparation exceeds byte limit; narrow search scope"
            );
            compile_ignore(directory.path(), Some(directory.path().join(name)), &text).map(Some)
        })?;
        if let Some(prepared) = prepared.filter(|prepared| !prepared.requirements.is_empty()) {
            ensure!(
                metadata.len() < METADATA_CHAIN_LIMIT,
                "Git metadata preparation exceeds chain limit; narrow search scope"
            );
            metadata.push(prepared);
        }
        Ok(())
    };
    let mut parent = directory.ancestor(anchor.path())?;
    let mut ignores = IgnoreRules::default();
    for component in directory.path().strip_prefix(anchor.path())?.components() {
        inspect(&parent, &mut ignores)?;
        let child = parent.path().join(component);
        if fixed_exclusion(component.as_os_str()) || ignores.excludes(&child, true) {
            for prepared in metadata.iter() {
                prepared.verify()?;
            }
            context.project_anchor(root, &parent)?;
            return Ok(action);
        }
        parent = directory.ancestor(&child)?;
    }
    let mut pending = vec![(directory, ignores, 0)];
    let mut visited = 0;
    while let Some((directory, mut ignores, depth)) = pending.pop() {
        inspect(&directory, &mut ignores)?;
        let entries =
            directory.entries(ENTRY_LIMIT - visited, || started.elapsed() >= TIME_LIMIT)?;
        ensure!(
            !entries.truncated,
            "Git metadata discovery exceeds traversal limit; narrow search scope"
        );
        visited += entries.entries.len();
        for entry in entries.entries {
            if entry.kind != EntryKind::Directory
                || entry.name.to_str().is_none()
                || fixed_exclusion(&entry.name)
                || ignores.excludes(&directory.path().join(&entry.name), true)
                || (matches!(search.query, Query::Grep { .. })
                    && entry
                        .name
                        .to_str()
                        .is_some_and(|name| name.starts_with('.')))
            {
                continue;
            }
            ensure!(
                depth < DEPTH_LIMIT && visited < ENTRY_LIMIT,
                "Git metadata discovery exceeds traversal limit; narrow search scope"
            );
            pending.push((
                directory.open_child(&entry.name)?,
                ignores.clone(),
                depth + 1,
            ));
        }
        directory.verify()?;
    }
    for prepared in metadata.iter() {
        prepared.verify()?;
    }
    context.project_anchor(root, &parent)?;
    Ok(action)
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct SearchReport<S = &'static str> {
    pub tool: S,
    pub scope: String,
    pub entries: Vec<SearchEntry<S>>,
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preparation_required: Vec<GitMetadataPreparation>,
    pub truncated: bool,
    pub cancelled: bool,
    pub limits_reached: Vec<S>,
    pub visited: usize,
    pub bytes_read: usize,
    pub skipped_nonregular: usize,
    pub skipped_ignored: usize,
    pub offset: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct SearchEntry<S = &'static str> {
    pub path: String,
    pub kind: S,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Clone, Default)]
struct IgnoreRules {
    ignore: Vec<Gitignore>,
    gitignore: Vec<Gitignore>,
    git_exclude: Vec<Gitignore>,
}

fn compile_ignore(scope: &Path, source: Option<PathBuf>, text: &str) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(scope);
    for line in text.lines() {
        builder.add_line(source.clone(), line)?;
    }
    Ok(builder.build()?)
}

impl IgnoreRules {
    fn load(
        &mut self,
        repository: bool,
        mut load: impl FnMut(&str) -> Result<Option<Gitignore>>,
    ) -> Result<()> {
        if repository {
            // A repository boundary cuts off ancestor Git rules, but not .ignore rules.
            self.gitignore.clear();
            self.git_exclude.clear();
            if let Some(ignore) = load(".git")? {
                self.git_exclude.push(ignore);
            }
        }
        if let Some(ignore) = load(".gitignore")? {
            self.gitignore.push(ignore);
        }
        if let Some(ignore) = load(".ignore")? {
            self.ignore.push(ignore);
        }
        Ok(())
    }

    fn excludes(&self, path: &Path, is_dir: bool) -> bool {
        self.ignore
            .iter()
            .rev()
            .chain(self.gitignore.iter().rev())
            .chain(self.git_exclude.iter().rev())
            .find_map(|ignore| match ignore.matched(path, is_dir) {
                ignore::Match::None => None,
                matched => Some(matched.is_ignore()),
            })
            .unwrap_or(false)
    }
}

fn fixed_exclusion(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        #[cfg(windows)]
        let normalized = name.to_ascii_lowercase();
        #[cfg(windows)]
        let name = normalized.as_str();
        matches!(name, ".git" | "node_modules" | "target") || name.starts_with(".bob-")
    })
}

struct Walker<'a> {
    root: &'a Path,
    metadata_root: &'a Path,
    metadata: &'a [PreparedGitMetadata],
    scope: &'a Path,
    search: &'a Search,
    cancel: &'a watch::Receiver<bool>,
    started: Instant,
    output_bytes: usize,
    seen: usize,
    glob_entries: Vec<(SystemTime, SearchEntry)>,
    report: SearchReport,
}

impl Walker<'_> {
    fn limit(&mut self, reason: &'static str) {
        self.report.truncated = true;
        if !self.report.limits_reached.contains(&reason) {
            self.report.limits_reached.push(reason);
        }
    }
    fn stop(&mut self) -> bool {
        if cancellation_requested(self.cancel) {
            self.report.cancelled = true;
            self.limit("cancelled");
            return true;
        }
        if self.started.elapsed() >= TIME_LIMIT {
            self.limit("time");
            return true;
        }
        if self.report.limits_reached.contains(&"results") {
            return true;
        }
        if self.report.limits_reached.contains(&"bytes_read") {
            return true;
        }
        if self.output_bytes >= OUTPUT_LIMIT - REPORT_RESERVE {
            self.limit("output_bytes");
            return true;
        }
        false
    }
    fn error(&mut self, path: &Path, error: impl std::fmt::Display) {
        self.report.truncated = true;
        if self.report.errors.len() >= 64 {
            self.limit("errors");
            return;
        }
        let error = format!(
            "{}: {error}",
            path.strip_prefix(self.root).unwrap_or(path).display()
        );
        let error = clip(&error, 1024).to_owned();
        let bytes = serde_json::to_vec(&error).map_or(4096, |value| value.len());
        if self.output_bytes + bytes >= OUTPUT_LIMIT - REPORT_RESERVE {
            self.output_bytes = OUTPUT_LIMIT;
            self.limit("errors");
            self.limit("output_bytes");
            return;
        }
        self.output_bytes += bytes;
        self.report.errors.push(error);
    }
    fn record(&mut self, entry: SearchEntry) -> Result<()> {
        self.seen += 1;
        if self.seen < self.search.offset {
            return Ok(());
        }
        if self.report.entries.len() >= self.search.max_results {
            self.limit("results");
            return Ok(());
        }
        let bytes = serde_json::to_vec(&entry)?.len() + 1;
        if self.output_bytes + bytes >= OUTPUT_LIMIT - REPORT_RESERVE {
            self.output_bytes = OUTPUT_LIMIT;
            self.limit("output_bytes");
            return Ok(());
        }
        self.output_bytes += bytes;
        self.report.entries.push(entry);
        Ok(())
    }
    fn read(&mut self, directory: &Directory, name: &OsStr, limit: usize) -> Result<String> {
        ensure!(!self.stop(), "search budget exhausted");
        #[cfg(test)]
        SEARCH_READ_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        let mut file = directory.open_file(name)?;
        let before = file.metadata()?;
        let identity = crate::native_fs::identity(&file)?;
        ensure!(
            before.len() <= limit as u64,
            "file exceeds {limit}-byte search read limit"
        );
        let remaining = BYTE_LIMIT - self.report.bytes_read;
        if before.len() > remaining as u64 {
            self.limit("bytes_read");
            bail!("aggregate read budget exhausted; narrow path or glob");
        }
        let mut bytes = Vec::new();
        let result = Read::by_ref(&mut file)
            .take((limit + 1).min(remaining) as u64)
            .read_to_end(&mut bytes);
        self.report.bytes_read += bytes.len();
        result?;
        let after = file.metadata()?;
        ensure!(
            before.len() == after.len()
                && after.len() == bytes.len() as u64
                && modification(&before) == modification(&after)
                && crate::native_fs::identity(&file)? == identity,
            "file changed during search read"
        );
        ensure!(
            bytes.len() <= limit,
            "file exceeds {limit}-byte search read limit"
        );
        ensure!(
            crate::native_fs::identity(&directory.open_file(name)?)? == identity,
            "file changed during search read"
        );
        directory.verify()?;
        Ok(String::from_utf8(bytes)?)
    }
    fn grep(
        &mut self,
        path: &str,
        text: &str,
        regex: &Regex,
        context: usize,
        multiline: bool,
        mode: OutputMode,
    ) -> Result<()> {
        if mode != OutputMode::Content {
            let mut count = 0;
            if multiline {
                for _ in regex.find_iter(text) {
                    if self.stop() {
                        return Ok(());
                    }
                    count += 1;
                    if mode == OutputMode::FilesWithMatches {
                        break;
                    }
                }
            } else {
                for line in text.lines() {
                    if self.stop() {
                        return Ok(());
                    }
                    if regex.is_match(line) {
                        count += 1;
                        if mode == OutputMode::FilesWithMatches {
                            break;
                        }
                    }
                }
            }
            if count > 0 {
                self.record(SearchEntry {
                    path: path.to_owned(),
                    kind: if mode == OutputMode::Count {
                        "count"
                    } else {
                        "file"
                    },
                    line: None,
                    text: (mode == OutputMode::Count).then(|| count.to_string()),
                })?;
            }
            return Ok(());
        }
        let mut matches = regex.find_iter(text).peekable();
        let mut before = VecDeque::new();
        let mut after = 0;
        let mut start = 0;
        for (index, raw) in text.split_inclusive('\n').enumerate() {
            if self.stop() {
                break;
            }
            let end = start + raw.len();
            let line = raw.strip_suffix('\n').unwrap_or(raw);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let matched = if multiline {
                while matches
                    .peek()
                    .is_some_and(|m| m.end() <= start && m.start() < start)
                {
                    matches.next();
                    if self.stop() {
                        return Ok(());
                    }
                }
                matches.peek().is_some_and(|m| {
                    m.start() < end
                        || (end == text.len() && !raw.ends_with('\n') && m.start() == end)
                })
            } else {
                regex.is_match(line)
            };
            if matched {
                while let Some((number, text)) = before.pop_front() {
                    if self.stop() {
                        return Ok(());
                    }
                    self.grep_line(path, number, text, false)?;
                }
                if self.stop() {
                    break;
                }
                self.grep_line(path, index + 1, line, true)?;
                after = context;
            } else if after > 0 {
                self.grep_line(path, index + 1, line, false)?;
                after -= 1;
            } else if context > 0 {
                before.push_back((index + 1, line));
                if before.len() > context {
                    before.pop_front();
                }
            }
            start = end;
        }
        Ok(())
    }
    fn grep_line(&mut self, path: &str, line: usize, text: &str, matched: bool) -> Result<()> {
        if self.seen + 1 >= self.search.offset && text.len() > 16 * 1024 {
            self.limit("line_bytes");
        }
        self.record(SearchEntry {
            path: path.to_owned(),
            kind: if matched { "match" } else { "context" },
            line: Some(line),
            text: Some(clip(text, 16 * 1024).to_owned()),
        })
    }
    fn ignore_file(
        &mut self,
        directory: &Directory,
        name: &str,
        scope: &Path,
    ) -> Result<Option<Gitignore>> {
        if !directory.name_exists(OsStr::new(name))? {
            return Ok(None);
        }
        let text = self.read(directory, OsStr::new(name), 16 * 1024)?;
        compile_ignore(scope, Some(directory.path().join(name)), &text).map(Some)
    }
    fn metadata_directory(
        &mut self,
        base: &Directory,
        path: &str,
        source: PathBuf,
        kind: GitMetadataKind,
    ) -> Result<Directory> {
        ensure!(!path.is_empty(), "empty Git metadata path");
        let path = Path::new(path);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.path().join(path)
        };
        ensure!(
            absolute.is_absolute(),
            "Git metadata path must resolve to an absolute path"
        );
        let mut requested = PathBuf::new();
        for part in absolute.components() {
            match part {
                Component::ParentDir => {
                    requested.pop();
                }
                Component::CurDir => {}
                _ => requested.push(part.as_os_str()),
            }
        }
        let requirement = GitMetadataPreparation {
            source,
            root: requested,
            kind,
        };
        if !requirement.root.starts_with(self.metadata_root) {
            return Err(requirement.into());
        }
        let (mut directory, relative) = if path.is_absolute() {
            (
                base.ancestor(self.metadata_root)?,
                path.strip_prefix(self.metadata_root).context(
                    "Git metadata path traverses outside the trusted discovery boundary",
                )?,
            )
        } else {
            (base.ancestor(base.path())?, path)
        };
        ensure!(
            relative.components().count() <= DEPTH_LIMIT,
            "Git metadata path exceeds directory depth limit"
        );
        for part in relative.components() {
            ensure!(!self.stop(), "search budget exhausted");
            directory = match part {
                Component::CurDir => directory,
                Component::Normal(name) => directory.open_child(name)?,
                Component::ParentDir => {
                    ensure!(
                        directory.path() != self.metadata_root,
                        "Git metadata path traverses outside the trusted discovery boundary"
                    );
                    directory.ancestor(
                        directory
                            .path()
                            .parent()
                            .context("missing Git metadata parent")?,
                    )?
                }
                _ => bail!("invalid Git metadata path"),
            };
        }
        Ok(directory)
    }
    fn git_exclude(&mut self, directory: &Directory) -> Result<Option<Gitignore>> {
        if let Some(prepared) = self
            .metadata
            .iter()
            .find(|metadata| metadata.scope == directory.path())
        {
            let Some(text) = prepared.verify()? else {
                return Ok(None);
            };
            self.report.bytes_read += text.len();
            ensure!(
                self.report.bytes_read <= BYTE_LIMIT,
                "Git metadata exceeds search read budget"
            );
            return compile_ignore(directory.path(), prepared.exclude.clone(), &text).map(Some);
        }
        let git = match directory.open_child(OsStr::new(".git")) {
            Ok(git) => git,
            Err(_) => {
                let text = self.read(directory, OsStr::new(".git"), 16 * 1024)?;
                let path = text
                    .lines()
                    .next()
                    .and_then(|line| line.strip_prefix("gitdir: "))
                    .context("invalid Git worktree metadata")?;
                self.metadata_directory(
                    directory,
                    path,
                    directory.path().join(".git"),
                    GitMetadataKind::GitDir,
                )?
            }
        };
        let git = if git.name_exists(OsStr::new("commondir"))? {
            let text = self.read(&git, OsStr::new("commondir"), 16 * 1024)?;
            self.metadata_directory(
                &git,
                text.lines().next().context("empty Git commondir")?,
                git.path().join("commondir"),
                GitMetadataKind::CommonDir,
            )?
        } else {
            git
        };
        if !git.name_exists(OsStr::new("info"))? {
            return Ok(None);
        }
        let info = git.open_child(OsStr::new("info"))?;
        self.ignore_file(&info, "exclude", directory.path())
    }
    fn ignores(&mut self, directory: &Directory, ignores: &mut IgnoreRules) {
        if self.stop() {
            return;
        }
        let repository = match directory.name_exists(OsStr::new(".git")) {
            Ok(repository) => repository,
            Err(error) => {
                self.error(&directory.path().join(".git"), error);
                false
            }
        };
        let result = ignores.load(repository, |name| {
            if self.stop() {
                return Ok(None);
            }
            let result = if name == ".git" {
                self.git_exclude(directory)
            } else {
                self.ignore_file(directory, name, directory.path())
            };
            match result {
                Ok(ignore) => Ok(ignore),
                Err(error) => {
                    if let Some(requirement) = error.downcast_ref::<GitMetadataPreparation>() {
                        if !self.report.preparation_required.contains(requirement) {
                            let bytes = serde_json::to_vec(requirement)
                                .map_or(OUTPUT_LIMIT, |value| value.len());
                            if self.output_bytes + bytes < OUTPUT_LIMIT - REPORT_RESERVE
                                && self.report.preparation_required.len() < 64
                            {
                                self.output_bytes += bytes;
                                self.report.preparation_required.push(requirement.clone());
                            } else {
                                self.limit("output_bytes");
                            }
                        }
                    }
                    self.error(&directory.path().join(name), error);
                    Ok(None)
                }
            }
        });
        if let Err(error) = result {
            self.error(directory.path(), error);
        }
    }
    fn selected_file(&mut self, directory: &Directory, path: &Path) -> Result<()> {
        if self.stop() {
            return Ok(());
        }
        let name = path.file_name().context("missing filename")?;
        self.report.visited += 1;
        let search = self.search;
        let Query::Grep {
            regex,
            glob,
            context,
            multiline,
            output_mode,
        } = &search.query
        else {
            bail!("only grep supports a file scope");
        };
        if glob
            .as_ref()
            .is_none_or(|glob| glob.matches(Path::new(name)))
        {
            let text = self.read(directory, name, READ_LIMIT)?;
            ensure!(!text.contains('\0'), "binary file skipped");
            self.grep(
                &relative_name(self.root, path)?,
                &text,
                regex,
                *context,
                *multiline,
                *output_mode,
            )?;
        }
        directory.verify()?;
        Ok(())
    }
    fn walk(&mut self, directory: Directory, mut ignores: IgnoreRules, depth: usize) -> Result<()> {
        if self.stop() {
            return Ok(());
        }
        if self.report.visited >= ENTRY_LIMIT {
            self.limit("entries");
            return Ok(());
        }
        if !matches!(self.search.query, Query::List) {
            self.ignores(&directory, &mut ignores);
        }
        let entries = directory.entries(ENTRY_LIMIT - self.report.visited, || {
            cancellation_requested(self.cancel) || self.started.elapsed() >= TIME_LIMIT
        })?;
        if entries.truncated {
            self.limit("directory_entries");
        }
        self.report.visited += entries.entries.len();
        let mut sorted = entries.entries;
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in sorted {
            if self.stop() {
                break;
            }
            let path = directory.path().join(&entry.name);
            if entry.name.to_str().is_none() {
                self.error(&path, "non-Unicode filename cannot be represented");
                continue;
            }
            if matches!(self.search.query, Query::List) {
                self.record(SearchEntry {
                    path: relative_name(self.root, &path)?,
                    kind: match entry.kind {
                        EntryKind::Directory => "directory",
                        EntryKind::File => "file",
                        _ => "other",
                    },
                    line: None,
                    text: None,
                })?;
                continue;
            }
            if matches!(self.search.query, Query::Grep { .. })
                && entry
                    .name
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'))
            {
                self.report.skipped_ignored += 1;
                continue;
            }
            if fixed_exclusion(&entry.name) {
                self.report.skipped_ignored += 1;
                continue;
            }
            if !matches!(entry.kind, EntryKind::File | EntryKind::Directory) {
                self.report.skipped_nonregular += 1;
                if entry.kind == EntryKind::Unknown {
                    self.error(&path, "filesystem did not report a regular entry type");
                }
                continue;
            }
            let is_dir = entry.kind == EntryKind::Directory;
            if ignores.excludes(&path, is_dir) {
                self.report.skipped_ignored += 1;
                continue;
            }
            let relative = relative_name(self.root, &path)?;
            let matched_path = path.strip_prefix(self.scope)?;
            let search = self.search;
            match &search.query {
                Query::Glob(glob) if !is_dir && glob.is_match(matched_path) => {
                    let result = (|| -> Result<SystemTime> {
                        let file = directory.open_file(&entry.name)?;
                        let modified = file
                            .metadata()?
                            .modified()
                            .unwrap_or(SystemTime::UNIX_EPOCH);
                        ensure!(
                            crate::native_fs::identity(&file)?
                                == crate::native_fs::identity(&directory.open_file(&entry.name)?)?,
                            "file changed during glob metadata read"
                        );
                        directory.verify()?;
                        Ok(modified)
                    })();
                    match result {
                        Ok(modified) => self.glob_entries.push((
                            modified,
                            SearchEntry {
                                path: relative,
                                kind: "file",
                                line: None,
                                text: None,
                            },
                        )),
                        Err(error) => self.error(&path, error),
                    }
                }
                Query::Grep {
                    regex,
                    glob,
                    context,
                    multiline,
                    output_mode,
                } if !is_dir && glob.as_ref().is_none_or(|glob| glob.matches(matched_path)) => {
                    match self.read(&directory, &entry.name, READ_LIMIT) {
                        Ok(text) if text.contains('\0') => self.error(&path, "binary file skipped"),
                        Ok(text) => {
                            self.grep(&relative, &text, regex, *context, *multiline, *output_mode)?
                        }
                        Err(error) => self.error(&path, error),
                    }
                }
                _ => {}
            }
            if is_dir && !matches!(self.search.query, Query::List) {
                if depth >= DEPTH_LIMIT {
                    self.limit("depth");
                    continue;
                }
                match directory.open_child(&entry.name) {
                    Ok(child) => {
                        if let Err(error) = self.walk(child, ignores.clone(), depth + 1) {
                            self.error(&path, error);
                        }
                    }
                    Err(error) => self.error(&path, error),
                }
            }
        }
        directory.verify()?;
        Ok(())
    }
}

pub(super) fn render<S: AsRef<str>>(report: &SearchReport<S>) -> String {
    let mut lines = Vec::new();
    let has_context = report
        .entries
        .iter()
        .any(|entry| entry.kind.as_ref() == "context");
    let mut previous: Option<(&str, usize)> = None;
    let prefix = if report.scope.is_empty() {
        String::new()
    } else {
        format!("{}/", report.scope)
    };
    for entry in &report.entries {
        let path = entry.path.strip_prefix(&prefix).unwrap_or(&entry.path);
        if report.tool.as_ref() == "list_dir" {
            lines.push(path.rsplit('/').next().unwrap_or(path).to_owned());
            continue;
        }
        if has_context {
            if let (Some((path, line)), Some(next)) = (previous, entry.line) {
                if path != entry.path || next != line + 1 {
                    lines.push("--".to_owned());
                }
            }
        }
        lines.push(
            match (entry.kind.as_ref(), entry.line, entry.text.as_deref()) {
                ("match", Some(line), Some(text)) => format!("{path}:{line}:{text}"),
                ("context", Some(line), Some(text)) => format!("{path}-{line}-{text}"),
                ("count", _, Some(count)) => format!("{path}:{count}"),
                ("directory", _, _) => format!("{path}/"),
                _ => path.to_owned(),
            },
        );
        previous = entry.line.map(|line| (entry.path.as_str(), line));
    }
    if lines.is_empty() {
        lines.push(
            if report.truncated {
                "(no results returned; search incomplete)"
            } else if report.offset > 1 {
                "(no results at this offset)"
            } else if report.tool.as_ref() == "list_dir" {
                "(empty)"
            } else {
                "(no matches)"
            }
            .to_owned(),
        );
    }
    for error in &report.errors {
        lines.push(format!("[!] {error}"));
    }
    if report.truncated {
        let reasons = report
            .limits_reached
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(", ");
        let reasons = if reasons.is_empty() {
            "read or traversal errors"
        } else {
            &reasons
        };
        let paging = report.next_offset.map_or_else(String::new, |offset| {
            format!(" use offset={offset} with the same query to continue if files are unchanged;")
        });
        lines.push(format!("... [search incomplete ({reasons});{paging} narrow path/pattern/glob or reduce context; glob ordering covers collected files only]"));
        if report
            .limits_reached
            .iter()
            .any(|reason| reason.as_ref() == "line_bytes")
        {
            lines.push("[!] Matching lines were clipped at 16384 bytes; use read_file with the reported path and line to inspect the full line.".to_owned());
        }
    }
    lines.join("\n")
}

fn clip(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(super) struct PreparedScope<'a> {
    pub path: &'a Path,
    pub ancestors: &'a Value,
}

#[cfg(test)]
pub(super) fn execute(
    root: &Path,
    name: &str,
    input: Value,
    path: &Path,
    ancestors: &Value,
    cancel: &watch::Receiver<bool>,
) -> Result<SearchReport> {
    execute_inner(
        root,
        name,
        input,
        PreparedScope { path, ancestors },
        cancel,
        None,
        &[],
    )
}

#[cfg(test)]
pub(super) fn execute_with_context(
    root: &Path,
    name: &str,
    input: Value,
    path: &Path,
    ancestors: &Value,
    cancel: &watch::Receiver<bool>,
    context: &DiscoveryContext,
) -> Result<SearchReport> {
    execute_inner(
        root,
        name,
        input,
        PreparedScope { path, ancestors },
        cancel,
        Some(context),
        &[],
    )
}

pub(super) fn execute_prepared_with_context(
    root: &Path,
    name: &str,
    input: Value,
    scope: PreparedScope<'_>,
    cancel: &watch::Receiver<bool>,
    context: &DiscoveryContext,
    metadata: &[PreparedGitMetadata],
) -> Result<SearchReport> {
    for prepared in metadata {
        ensure!(
            !cancellation_requested(cancel),
            "cancelled before Git metadata verification"
        );
        prepared.verify()?;
    }
    let report = execute_inner(root, name, input, scope, cancel, Some(context), metadata)?;
    for prepared in metadata {
        prepared.verify()?;
    }
    Ok(report)
}

fn execute_inner(
    root: &Path,
    name: &str,
    input: Value,
    scope: PreparedScope<'_>,
    cancel: &watch::Receiver<bool>,
    context: Option<&DiscoveryContext>,
    metadata: &[PreparedGitMetadata],
) -> Result<SearchReport> {
    let PreparedScope { path, ancestors } = scope;
    let search = parse(name, input)?;
    let (directory, selected_path) = search_scope(root, &search)?;
    ensure!(
        selected_path == path && directory.identities()? == *ancestors,
        "search scope changed since preparation"
    );
    let scope = directory.path().to_path_buf();
    let scope_anchor = directory.ancestor(&scope)?;
    let anchor = match context {
        Some(context) => context.project_anchor(root, &directory)?,
        None => directory.ancestor(root)?,
    };
    ensure!(
        scope.strip_prefix(anchor.path())?.components().count() <= DEPTH_LIMIT,
        "ignore discovery exceeds directory depth limit"
    );
    let mut walker = Walker {
        root,
        metadata_root: anchor.path(),
        metadata,
        scope: &scope,
        search: &search,
        cancel,
        started: Instant::now(),
        output_bytes: 0,
        seen: 0,
        glob_entries: Vec::new(),
        report: SearchReport {
            tool: match name {
                "grep" => "grep",
                "glob" => "glob",
                _ => "list_dir",
            },
            scope: relative_name(root, &scope)?,
            offset: search.offset,
            ..SearchReport::default()
        },
    };
    let mut parent = directory.ancestor(anchor.path())?;
    let mut ignores = IgnoreRules::default();
    for part in scope.strip_prefix(anchor.path())?.components() {
        if matches!(search.query, Query::List) || selected_path != scope {
            break;
        }
        walker.ignores(&parent, &mut ignores);
        if walker.stop() || !walker.report.errors.is_empty() {
            return Ok(walker.report);
        }
        let child = parent.path().join(part);
        if fixed_exclusion(part.as_os_str()) || ignores.excludes(&child, true) {
            walker.report.skipped_ignored += 1;
            walker.error(
                &child,
                "selected search scope is excluded; scope was not searched",
            );
            return Ok(walker.report);
        }
        parent = directory.ancestor(&child)?;
    }
    let result = if selected_path == scope {
        walker.walk(directory, ignores, 0)
    } else {
        walker.selected_file(&directory, &selected_path)
    };
    if let Err(error) = result {
        walker.error(path, error);
    }
    if matches!(search.query, Query::Glob(_)) {
        walker
            .glob_entries
            .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
        for (_, entry) in std::mem::take(&mut walker.glob_entries) {
            walker.record(entry)?;
            if walker.report.limits_reached.contains(&"results")
                || walker.output_bytes >= OUTPUT_LIMIT - REPORT_RESERVE
            {
                break;
            }
        }
    }
    if !walker.report.entries.is_empty()
        && walker.report.errors.is_empty()
        && walker
            .report
            .limits_reached
            .iter()
            .all(|reason| matches!(*reason, "results" | "output_bytes" | "line_bytes"))
        && walker
            .report
            .limits_reached
            .iter()
            .any(|reason| matches!(*reason, "results" | "output_bytes"))
    {
        walker.report.next_offset = search.offset.checked_add(walker.report.entries.len());
    }
    if let Some(context) = context {
        if let Err(error) = context.project_anchor(root, &scope_anchor) {
            walker.error(&scope, error);
            walker.report.next_offset = None;
        }
    }
    Ok(walker.report)
}

#[cfg(test)]
#[path = "repository_preparation_tests.rs"]
mod preparation_tests;

#[cfg(test)]
#[path = "repository_ignore_tests.rs"]
mod ignore_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("bob-search-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }

        fn write(&self, path: &str, text: impl AsRef<[u8]>) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn search(&self, name: &str, input: Value) -> SearchReport {
            let Action::Repository {
                path, ancestors, ..
            } = prepare(&self.0, name, input.clone()).unwrap()
            else {
                panic!("not a repository action")
            };
            let (_sender, cancel) = watch::channel(false);
            execute(&self.0, name, input, &path, &ancestors, &cancel).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn grep_modes_context_and_recursive_globs() {
        let fixture = Fixture::new();
        fixture.write(
            "nested/file.rs",
            "before\nA[i] A[i]\nbetween\na[i]\nafter\n",
        );
        fixture.write("other.txt", "A[i]\n");
        let input = json!({"pattern":"a[i]", "literal":true, "ignore_case":true, "glob":"*.rs", "context":1});
        let report = fixture.search("grep", input.clone());
        assert!(!report.truncated, "{}", render(&report));
        assert_eq!(report.entries.len(), 5);
        assert_eq!(
            report.entries.iter().map(|e| e.kind).collect::<Vec<_>>(),
            ["context", "match", "context", "match", "context"]
        );
        assert_eq!(render(&report), "nested/file.rs-1-before\nnested/file.rs:2:A[i] A[i]\nnested/file.rs-3-between\nnested/file.rs:4:a[i]\nnested/file.rs-5-after");
        let mut files = input.clone();
        files["output_mode"] = json!("files_with_matches");
        assert_eq!(render(&fixture.search("grep", files)), "nested/file.rs");
        let mut counts = input;
        counts["output_mode"] = json!("count");
        assert_eq!(render(&fixture.search("grep", counts)), "nested/file.rs:2");
        assert_eq!(
            render(&fixture.search(
                "grep",
                json!({"pattern":"A", "glob":"!*.rs", "output_mode":"files_with_matches"})
            )),
            "other.txt"
        );
        assert_eq!(
            fixture
                .search("grep", json!({"pattern":"A", "glob":""}))
                .entries
                .len(),
            2
        );
    }

    #[test]
    fn multiline_matches_render_each_line_and_count_matches() {
        let fixture = Fixture::new();
        fixture.write("file", "before\nSTART\nend\nafter\nSTART\nend\n");
        let input = json!({"pattern":"^START.*?end$", "multiline":true});
        let report = fixture.search("grep", input.clone());
        assert_eq!(
            report
                .entries
                .iter()
                .map(|e| e.line.unwrap())
                .collect::<Vec<_>>(),
            [2, 3, 5, 6]
        );
        assert!(!report.truncated);
        let mut counts = input;
        counts["output_mode"] = json!("count");
        assert_eq!(render(&fixture.search("grep", counts)), "file:2");
        assert!(fixture
            .search("grep", json!({"pattern":"START.*end"}))
            .entries
            .is_empty());
        assert_eq!(
            fixture
                .search(
                    "grep",
                    json!({"pattern":"START\nend", "multiline":true, "literal":true})
                )
                .entries
                .len(),
            4
        );
    }

    #[test]
    fn result_pages_do_not_duplicate_context_or_claim_exact_pages_truncated() {
        let fixture = Fixture::new();
        fixture.write("file", "before\nhit\nafter\nhit\nlast\n");
        let mut input = json!({"pattern":"hit", "context":1, "max_results":2});
        let first = fixture.search("grep", input.clone());
        assert_eq!(first.next_offset, Some(3));
        assert!(render(&first).contains("offset=3"));
        input["offset"] = json!(3);
        let second = fixture.search("grep", input.clone());
        assert_eq!(second.entries[0].line, Some(3));
        assert_eq!(second.next_offset, Some(5));
        input["offset"] = json!(5);
        let last = fixture.search("grep", input);
        assert_eq!(last.entries[0].line, Some(5));
        assert!(!last.truncated);
        let exact = fixture.search("grep", json!({"pattern":"hit", "max_results":2}));
        assert_eq!(exact.entries.len(), 2);
        assert!(!exact.truncated);
        assert_eq!(exact.next_offset, None);
    }

    #[test]
    fn glob_orders_all_collected_files_before_paging() {
        let fixture = Fixture::new();
        for (path, seconds) in [("a.rs", 1), ("nested/b.rs", 3), ("c.rs", 2)] {
            fixture.write(path, "hit");
            fs::File::options()
                .write(true)
                .open(fixture.0.join(path))
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                )
                .unwrap();
        }
        fs::create_dir(fixture.0.join("directory.rs")).unwrap();
        let first = fixture.search("glob", json!({"pattern":"**/*.rs", "max_results":2}));
        assert_eq!(
            first
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["nested/b.rs", "c.rs"]
        );
        assert_eq!(first.next_offset, Some(3));
        let last = fixture.search(
            "glob",
            json!({"pattern":"**/*.rs", "max_results":2, "offset":3}),
        );
        assert_eq!(render(&last), "a.rs");
        assert!(!last.truncated);
    }

    #[test]
    fn search_charges_actual_bytes_even_for_failed_utf8_reads() {
        let fixture = Fixture::new();
        fixture.write(".gitignore", "*.skip\n");
        fixture.write("a", "hit\n");
        fixture.write("b", "hit\n");
        let report = fixture.search("grep", json!({"pattern":"hit", "glob":"[ab]"}));
        assert_eq!(report.entries.len(), 2);
        assert_eq!(report.bytes_read, 15);
        assert!(!report.truncated, "{}", render(&report));
        fixture.write("a", [0xff, 0xfe]);
        let report = fixture.search("grep", json!({"pattern":"hit", "glob":"[ab]"}));
        assert_eq!(report.bytes_read, 13);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.next_offset, None);
    }

    #[test]
    fn read_budget_does_not_prevent_searching_the_last_read_file() {
        let fixture = Fixture::new();
        fixture.write("file", "hit\n");
        let search = parse("grep", json!({"pattern":"hit"})).unwrap();
        let (_sender, cancel) = watch::channel(false);
        let mut walker = Walker {
            root: &fixture.0,
            metadata_root: &fixture.0,
            metadata: &[],
            scope: &fixture.0,
            search: &search,
            cancel: &cancel,
            started: Instant::now(),
            output_bytes: 0,
            seen: 0,
            glob_entries: Vec::new(),
            report: SearchReport {
                bytes_read: BYTE_LIMIT - 4,
                ..SearchReport::default()
            },
        };
        let directory = Directory::open(&fixture.0).unwrap();
        let text = walker
            .read(&directory, OsStr::new("file"), READ_LIMIT)
            .unwrap();
        assert_eq!(walker.report.bytes_read, BYTE_LIMIT);
        walker
            .grep(
                "file",
                &text,
                &Regex::new("hit").unwrap(),
                0,
                false,
                OutputMode::Content,
            )
            .unwrap();
        assert_eq!(walker.report.entries.len(), 1);
        assert!(walker
            .read(&directory, OsStr::new("file"), READ_LIMIT)
            .is_err());
        assert_eq!(walker.report.bytes_read, BYTE_LIMIT);
    }

    #[test]
    fn file_scope_is_exact_and_rendering_uses_scope_relative_paths() {
        let fixture = Fixture::new();
        fixture.write("nested/selected.rs", "hit\n");
        fixture.write("nested/other.rs", "hit\n");
        let report = fixture.search(
            "grep",
            json!({"path":"nested/selected.rs", "pattern":"hit"}),
        );
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].path, "nested/selected.rs");
        assert_eq!(report.visited, 1);
        assert_eq!(render(&report), "selected.rs:1:hit");
        let report = fixture.search("glob", json!({"path":"nested", "pattern":"*.rs"}));
        assert!(render(&report).lines().all(|line| !line.contains('/')));
        assert!(fixture
            .search(
                "grep",
                json!({"path":"nested/selected.rs", "pattern":"hit", "glob":"*.txt"})
            )
            .entries
            .is_empty());
    }

    #[test]
    fn grep_skips_hidden_but_glob_and_list_include_hidden_and_list_includes_ignored() {
        let fixture = Fixture::new();
        fixture.write(".gitignore", "*.skip\n");
        fixture.write(".hidden", "hit\n");
        fixture.write(".hidden-dir/file", "hit\n");
        fixture.write("file.skip", "hit\n");
        fixture.write("visible", "hit\n");
        fixture.write("target/generated", "hit\n");
        let grep = fixture.search("grep", json!({"pattern":"hit"}));
        assert_eq!(render(&grep), "visible:1:hit");
        assert_eq!(
            render(&fixture.search("grep", json!({"path":".hidden", "pattern":"hit"}))),
            ".hidden:1:hit"
        );
        assert_eq!(
            render(&fixture.search("grep", json!({"path":"file.skip", "pattern":"hit"}))),
            "file.skip:1:hit"
        );
        let glob = fixture.search("glob", json!({"pattern":"**"}));
        assert!(glob
            .entries
            .iter()
            .any(|entry| entry.path == ".hidden-dir/file"));
        assert!(glob
            .entries
            .iter()
            .all(|entry| !entry.path.ends_with(".skip") && !entry.path.starts_with("target/")));
        let list = fixture.search("list_dir", json!({}));
        assert_eq!(list.bytes_read, 0);
        assert_eq!(
            render(&list),
            ".gitignore\n.hidden\n.hidden-dir\nfile.skip\ntarget\nvisible"
        );
        assert_eq!(
            render(&fixture.search("list_dir", json!({"path":"target"}))),
            "generated"
        );
        fs::create_dir(fixture.0.join("empty")).unwrap();
        assert_eq!(
            render(&fixture.search("list_dir", json!({"path":"empty"}))),
            "(empty)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_file_scopes_reject_symlinks_and_retargeting_and_list_never_follows() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = Fixture::new();
        outside.write("secret", "outside secret\n");
        symlink(outside.0.join("secret"), fixture.0.join("link")).unwrap();
        assert!(prepare(
            &fixture.0,
            "grep",
            json!({"path":"link", "pattern":"secret"})
        )
        .is_err());
        let list = fixture.search("list_dir", json!({}));
        assert_eq!(render(&list), "link");
        assert_eq!(list.bytes_read, 0);
        fixture.write("file", "inside\n");
        let target = fixture.0.join("file");
        let replacement = outside.0.join("secret");
        SEARCH_READ_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::remove_file(&target).unwrap();
                symlink(&replacement, &target).unwrap();
            }));
        });
        let report = fixture.search("grep", json!({"path":"file", "pattern":"secret"}));
        assert!(report.entries.is_empty());
        assert!(report.truncated);
        assert!(!report.errors.is_empty());
    }

    #[test]
    fn malformed_options_are_rejected_before_traversal() {
        assert!(parse(
            "grep",
            json!({"pattern":"x", "offset":1.0, "context":0.0, "max_results":2.0})
        )
        .is_ok());
        for input in [
            json!({"pattern":"x", "context":1.5}),
            json!({"pattern":"x", "offset":1e100}),
            json!({"pattern":"x", "output_mode":"other"}),
            json!({"pattern":"x", "context":2001}),
            json!({"pattern":"x", "offset":0}),
            json!({"pattern":"x", "max_results":2001}),
            json!({"pattern":"x", "multiline":"true"}),
            json!({"pattern":"x", "context":-1}),
        ] {
            assert!(parse("grep", input).is_err());
        }
        for pattern in ["../*.rs", "src/../*.rs", "["] {
            assert!(parse("glob", json!({"pattern":pattern})).is_err());
        }
    }
}

#[cfg(test)]
#[path = "repository_metadata_tests.rs"]
mod metadata_tests;
