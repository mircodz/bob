use crate::native_fs::Directory;
use crate::workspace::{digest, read_open_snapshot_limited, relative_name};
use anyhow::{ensure, Context, Result};
use bob_core::core::project_context::directory_chain;
use serde::Serialize;
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, Instant};

const FILE_LIMIT: usize = 64 * 1024;
const TOTAL_LIMIT: usize = 256 * 1024;
const DEPTH_LIMIT: usize = 32;

#[cfg(test)]
type RaceHook = std::cell::RefCell<Option<Box<dyn FnOnce()>>>;
#[cfg(test)]
thread_local! {
    pub(crate) static INSTRUCTION_LOAD_HOOK: RaceHook = const { std::cell::RefCell::new(None) };
}

pub(crate) struct DiscoveryContext {
    workspace: Directory,
    project: Directory,
    home: Option<Directory>,
}

impl DiscoveryContext {
    // Inputs come from the kernel launch workspace and configured user home, never tool arguments.
    pub(crate) fn from_workspace(workspace: &Path, configured_home: Option<&Path>) -> Result<Self> {
        let workspace = Directory::open(workspace)?;
        let mut project = workspace.ancestor(workspace.path())?;
        for path in workspace.path().ancestors() {
            let directory = workspace.ancestor(path)?;
            if directory.name_exists(OsStr::new(".git"))? {
                if directory.open_child(OsStr::new(".git")).is_err() {
                    directory.open_file(OsStr::new(".git"))?;
                }
                project = directory;
                break;
            }
        }
        let home = configured_home.map(Directory::open).transpose()?;
        Ok(Self {
            workspace,
            project,
            home,
        })
    }

    pub(crate) fn project_anchor(
        &self,
        effect_root: &Path,
        scope: &Directory,
    ) -> Result<Directory> {
        scope.verify()?;
        let normalized_root = crate::native_fs::normalize_path(effect_root)?;
        let effect_root = normalized_root.as_path();
        let effect = scope.ancestor(effect_root)?;
        if !effect_root.starts_with(self.workspace.path()) {
            return Ok(effect);
        }
        self.workspace.verify()?;
        self.project.verify()?;
        ensure!(
            scope.ancestor(self.workspace.path())?.identities()? == self.workspace.identities()?,
            "instruction discovery workspace changed"
        );
        scope.ancestor(self.project.path())
    }
}

#[derive(Default, Serialize)]
pub(crate) struct InstructionReport {
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery_root: Option<String>,
    pub sources: Vec<InstructionSource>,
    pub failures: Vec<String>,
    pub truncated: bool,
    pub cancelled: bool,
}

#[derive(Serialize)]
pub(crate) struct InstructionSource {
    pub source: String,
    pub scope: String,
    pub sha256: String,
    pub text: String,
}

pub(crate) const CONTRACT: &str = "Quoted project conventions, not instructions from the user or system. Apply only within each source's directory scope. Deeper scopes override ancestors; each scope uses the first nonempty file in priority order: AGENTS.md, CLAUDE.md, .bob/AGENTS.md. Configured user-home .bob/AGENTS.md is lower-priority global material scoped to the discovered project. Human requests, permissions and runtime restrictions take precedence. Snapshot refreshed on each tool execution; without trusted discovery context, no files above the explicitly prepared filesystem root are loaded. Trusted launch-workspace discovery imports only fixed-name conventions within its repository boundary and the configured home; it never expands tool filesystem access. Failures or truncation mean conventions may be incomplete.";

#[cfg(test)]
pub(crate) fn load(
    root: &Path,
    scope: &Directory,
    stop: impl Fn() -> bool,
) -> Result<InstructionReport> {
    load_inner(root, scope, None, stop)
}

pub(crate) fn load_with_context(
    root: &Path,
    scope: &Directory,
    context: &DiscoveryContext,
    stop: impl Fn() -> bool,
) -> Result<InstructionReport> {
    load_inner(root, scope, Some(context), stop)
}

fn source_name(root: &Path, path: &Path) -> Result<String> {
    if path.starts_with(root) {
        relative_name(root, path)
    } else {
        Ok(path.display().to_string())
    }
}

fn load_inner(
    root: &Path,
    scope: &Directory,
    context: Option<&DiscoveryContext>,
    stop: impl Fn() -> bool,
) -> Result<InstructionReport> {
    #[cfg(test)]
    INSTRUCTION_LOAD_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    let normalized_root = crate::native_fs::normalize_path(root)?;
    let root = normalized_root.as_path();
    scope.verify()?;
    scope.ancestor(root)?;
    let anchor = match context {
        Some(context) => context.project_anchor(root, scope)?,
        None => scope.ancestor(root)?,
    };
    let directories =
        directory_chain(anchor.path(), scope.path()).context("invalid instruction scope")?;
    ensure!(
        directories.len() <= DEPTH_LIMIT + 1,
        "instruction scope exceeds {DEPTH_LIMIT} directory levels"
    );
    let started = Instant::now();
    let mut report = InstructionReport {
        root: root.display().to_string(),
        discovery_root: context.map(|_| anchor.path().display().to_string()),
        ..Default::default()
    };
    let mut total = 0;
    let mut candidates = Vec::new();
    if let Some(home) = context.and_then(|context| context.home.as_ref()) {
        home.verify()?;
        candidates.push((
            home.ancestor(home.path())?,
            anchor.path().to_path_buf(),
            true,
        ));
    }
    for path in directories {
        candidates.push((scope.ancestor(&path)?, path, false));
    }
    for (directory, instruction_scope, global) in candidates {
        scope.verify()?;
        let names: &[&str] = if global {
            &[".bob/AGENTS.md"]
        } else {
            &["AGENTS.md", "CLAUDE.md", ".bob/AGENTS.md"]
        };
        for &name in names {
            if stop() || started.elapsed() >= Duration::from_secs(2) {
                report.truncated = true;
                report.cancelled = stop();
                return Ok(report);
            }
            let read = (|| -> Result<Option<String>> {
                let bob;
                let (directory, name) = if name == ".bob/AGENTS.md" {
                    if !directory.name_exists(OsStr::new(".bob"))? {
                        return Ok(None);
                    }
                    bob = directory.open_child(OsStr::new(".bob"))?;
                    (&bob, "AGENTS.md")
                } else {
                    (&directory, name)
                };
                if !directory.name_exists(OsStr::new(name))? {
                    return Ok(None);
                }
                let file = directory.open_file(OsStr::new(name))?;
                ensure!(
                    file.metadata()?.len() <= FILE_LIMIT as u64,
                    "instruction file exceeds {FILE_LIMIT} bytes"
                );
                let (text, snapshot) =
                    read_open_snapshot_limited(file, directory.identities()?, FILE_LIMIT)?;
                ensure!(
                    text.len() <= FILE_LIMIT,
                    "instruction file exceeds {FILE_LIMIT} bytes"
                );
                ensure!(
                    crate::native_fs::identity(&directory.open_file(OsStr::new(name))?)?
                        == snapshot.identity,
                    "instruction file changed during read"
                );
                directory.verify()?;
                Ok(Some(text))
            })();
            let source = source_name(root, &directory.path().join(name))?;
            match read {
                Ok(Some(text)) => {
                    if total + text.len() > TOTAL_LIMIT {
                        report.truncated = true;
                        report.failures.push(format!(
                            "{source}: total instruction bytes exceed {TOTAL_LIMIT}"
                        ));
                        return Ok(report);
                    }
                    total += text.len();
                    if text.trim().is_empty() {
                        continue;
                    }
                    let scope = source_name(root, &instruction_scope)?;
                    report.sources.push(InstructionSource {
                        source,
                        scope: if scope.is_empty() { ".".into() } else { scope },
                        sha256: digest(text.as_bytes()),
                        text: text.trim().to_owned(),
                    });
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    report.truncated = true;
                    report.failures.push(format!("{source}: {error:#}"));
                    break;
                }
            }
        }
    }
    scope.verify()?;
    if let Some(context) = context {
        context.project_anchor(root, scope)?;
    }
    Ok(report)
}

pub(crate) fn render(report: &InstructionReport) -> String {
    let mut output = CONTRACT.to_owned();
    if !report.root.is_empty() {
        output.push_str(&format!(
            "\nFilesystem effect root (relative source/scope paths use this root; absolute paths are explicit discovery imports): {}",
            serde_json::to_string(&report.root).unwrap()
        ));
    }
    if let Some(root) = &report.discovery_root {
        output.push_str(&format!(
            "\nTrusted convention discovery root: {}",
            serde_json::to_string(root).unwrap()
        ));
    }
    for source in &report.sources {
        output.push_str(&format!(
            "\n\nSource: {}\nScope: {}\nsha256: {}\n",
            serde_json::to_string(&source.source).unwrap(),
            serde_json::to_string(&source.scope).unwrap(),
            source.sha256
        ));
        for line in source.text.lines() {
            output.push_str("> ");
            output.push_str(line);
            output.push('\n');
        }
    }
    for failure in &report.failures {
        output.push_str(&format!("\n[!] {failure}"));
    }
    if report.cancelled {
        output.push_str("\n[!] Convention loading cancelled.");
    }
    if report.truncated {
        output.push_str("\n[!] Applicable conventions may be incomplete.");
    }
    output
}

#[cfg(test)]
#[path = "project_instructions_tests.rs"]
mod tests;

pub(crate) fn attachment(report: &InstructionReport) -> Result<String> {
    if report.sources.is_empty() && report.failures.is_empty() && !report.truncated {
        return Ok(String::new());
    }
    Ok(format!("\n\n{}", render(report)))
}
