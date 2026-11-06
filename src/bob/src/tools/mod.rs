mod create;
mod file;
mod mutation;
mod presentation;
use mutation::*;
mod writes;
pub(crate) use writes::PreparedWrites;
mod read;
mod repository;
mod shell;
mod shell_policy;
use crate::native_fs as filesystem;
use crate::native_worker::{self, ShellRequest};
use crate::workspace::*;
use anyhow::{bail, ensure, Context, Result};
use bob_core::core::shell::{ShellKind, ShellSelection};
#[cfg(test)]
use bob_kernel::ThreadId;
use bob_kernel::{EffectClass, Invocation, PreparedTool, ToolDescriptor, ToolOutcome};
use bob_types::ToolSpec;
use file::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use shell::*;
#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::Duration;
use tokio::sync::{watch, Mutex};
#[cfg(test)]
const STREAM_LIMIT: usize = 256 * 1024;
const COMMAND_LIMIT: usize = 16 * 1024;
const SHELL_LIMITATION: &str = native_worker::CONTAINMENT;
#[derive(Default)]
pub struct ToolService {
    files: Mutex<FileState>,
    configured_shell: Option<ShellSelection>,
    discovery_home: Option<PathBuf>,
}
fn descriptors() -> Vec<ToolDescriptor> {
    [
        ("read_file", "Read a regular UTF-8 file up to 16 MiB with display-only 1-based line numbers. Output is capped at 2000 lines, 256 KiB, and 2000 Unicode characters per line. Page with offset/limit and the reported column_offset cursor. Only returned bytes establish thread-local read evidence. Applicable scoped project conventions are attached, subordinate to human requests and permissions.", EffectClass::Read,
            json!({"path": {"type":"string", "minLength":1}, "offset":{"type":"integer","minimum":1}, "limit":{"type":"integer","minimum":1}, "column_offset":{"type":"integer","minimum":1}}), vec!["path"]),
        ("edit_file", "Replace an exact string in a previously read unchanged UTF-8 file. Match uniquely or set replace_all. Preserve whitespace; never include read_file display line numbers. Prefer multi_edit for several edits. Identity-bound staged publication retains displaced originals for recovery.", EffectClass::Write,
            json!({"path": {"type":"string", "minLength":1}, "old_string":{"type":"string", "minLength":1}, "new_string":{"type":"string"}, "replace_all":{"type":"boolean"}}), vec!["path", "old_string", "new_string"]),
        ("multi_edit", "Apply sequential exact-string edits to ONE previously read file in a single atomic publication. Each edit sees the preceding result; if any edit fails nothing is written. Each old_string must match uniquely or set replace_all.", EffectClass::Write,
            json!({"path":{"type":"string","minLength":1},"edits":{"type":"array","minItems":1,"items":{"type":"object","properties":{"old_string":{"type":"string","minLength":1},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["old_string","new_string"],"additionalProperties":false}}}), vec!["path","edits"]),
        ("write_file", "Create a UTF-8 file or deliberately replace its entire contents, up to 16 MiB. Existing files require every byte to have been shown through read_file, including all pages and clipped lines, and must be unchanged. Prefer edit_file or multi_edit. Missing parents are created only during approved execution using anchored directory handles; never follows symlinks or overwrites a raced creation.", EffectClass::Write,
            json!({"path":{"type":"string","minLength":1},"content":{"type":"string"}}), vec!["path","content"]),
        ("create_file", "Create a new UTF-8 file, at most 16 MiB, in an existing parent directory. Preparation has no filesystem effects; publication never overwrites an existing file or symlink. Use project_instructions on the parent directory first. Creation does not establish edit evidence; read_file is required before a later edit.", EffectClass::Write,
            json!({"path":{"type":"string","minLength":1}, "content":{"type":"string"}}), vec!["path", "content"]),
        ("list_dir", "List immediate directory entries, including hidden and ignored names, without following symlinks. path defaults to the workspace. Defaults to 2000 results; use offset/max_results to page. Anchored traversal is bounded to 10,000 entries, 256 KiB output and 5 seconds; explicit truncation and errors. Grants no edit evidence.", EffectClass::Read,
            json!({"path":{"type":"string","minLength":1}, "max_results":{"type":"integer","minimum":1,"maximum":2000},"offset":{"type":"integer","minimum":1}}), vec![]),
        ("glob", "Find files by a relative glob pattern, newest modification time first. path defaults to the workspace; ** recurses. Anchored nofollow traversal honors .gitignore/.ignore and excludes .git/node_modules/target. Defaults to 500 results; use offset/max_results to page. Bounds: 10,000 entries, depth 32, 256 KiB output, 5 seconds. Narrow path/pattern when traversal limits are reached. Grants no edit evidence.", EffectClass::Read,
            json!({"path":{"type":"string","minLength":1}, "pattern":{"type":"string","minLength":1,"maxLength":4096}, "max_results":{"type":"integer","minimum":1,"maximum":2000},"offset":{"type":"integer","minimum":1}}), vec!["pattern"]),
        ("grep", "Search UTF-8 regular files using regex or literal:true. Supports ignore_case, multiline (dot spans newlines), context, and glob filters. output_mode is content (default), files_with_matches, or count. path is a directory or exact file, default workspace. Anchored nofollow traversal honors ignores, skips hidden/generated trees. Files up to 16 MiB; aggregate reads 16 MiB; 10,000 entries, depth 32, 256 KiB output, 16 KiB/line, 5 seconds. max_results defaults to 200; offset pages results. Narrow path/glob if a traversal limit is reached. Search NEVER establishes read/edit evidence; use read_file before editing.", EffectClass::Read,
            json!({"path":{"type":"string","minLength":1}, "pattern":{"type":"string","minLength":1,"maxLength":4096}, "literal":{"type":"boolean"}, "ignore_case":{"type":"boolean"}, "glob":{"type":"string"}, "max_results":{"type":"integer","minimum":1,"maximum":2000},"offset":{"type":"integer","minimum":1},"context":{"type":"integer","minimum":0,"maximum":2000},"multiline":{"type":"boolean"},"output_mode":{"type":"string","enum":["content","files_with_matches","count"]}}), vec!["pattern"]),
        ("project_instructions", "Read quoted conventions applicable to an existing directory path (use '.' for workspace). Stops at the prepared root. Select the first nonempty AGENTS.md, CLAUDE.md, or .bob/AGENTS.md per directory. Deeper scope overrides ancestors; human requests/permissions always take precedence. Refreshes at each execution, no hidden provider context. Bounds: depth 32, 64 KiB/file, 256 KiB total, 2 seconds; failures/truncation explicit. Grants no edit evidence. Successful read_file also attaches these scoped conventions.", EffectClass::Read,
            json!({"path":{"type":"string","minLength":1}}), vec!["path"]),
        ("bash", "Run an exact non-interactive command using the configured native shell from the workspace root. timeout is an optional positive duration in seconds; when omitted, execution is controlled by cancellation and parent liveness. This is not a filesystem sandbox. Foreground output retains at most 256 KiB per stream. New human input can move a running foreground command into a kernel-owned background job without restarting it or resetting its timeout. Collect the returned job ID with job_output; do not repeat the command. Background scheduling is owned by the kernel, not this execution adapter.", EffectClass::Write,
            json!({"command":{"type":"string", "minLength":1, "maxLength":COMMAND_LIMIT}, "timeout":{"type":"number", "exclusiveMinimum":0}, "run_in_background":{"type":"boolean"}}), vec!["command"]),
    ].into_iter().map(|(name, description, effect, properties, required)| ToolDescriptor {
        spec: ToolSpec {
            name: name.into(), description: description.into(),
            input_schema: json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false}),
        }, effect,
    }).collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathInput {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    path: String,
    #[serde(default, deserialize_with = "read::optional_integer")]
    offset: Option<usize>,
    #[serde(default, deserialize_with = "read::optional_integer")]
    limit: Option<usize>,
    #[serde(default, deserialize_with = "read::optional_integer")]
    column_offset: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditStep {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiEditInput {
    path: String,
    edits: Vec<EditStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellInput {
    command: String,
    #[serde(rename = "timeout")]
    timeout_seconds: Option<f64>,
    #[serde(default)]
    run_in_background: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Create {
        path: PathBuf,
        ancestor_parent: PathBuf,
        ancestors: Value,
        staged: PathBuf,
        content_sha256: String,
    },
    Repository {
        path: PathBuf,
        ancestors: Value,
        metadata: Vec<repository::PreparedGitMetadata>,
    },
    Instructions {
        path: PathBuf,
        ancestors: Value,
    },
    Read {
        path: PathBuf,
        snapshot: Snapshot,
    },
    Edit {
        path: PathBuf,
        snapshot: Snapshot,
        result_sha256: String,
        staged: PathBuf,
        backup: PathBuf,
    },
    Shell {
        executable: PathBuf,
        snapshot: Snapshot,
        dialect: String,
        helper: Option<BoundExecutable>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundExecutable {
    path: PathBuf,
    snapshot: Snapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bound {
    version: u32,
    invocation_sha256: String,
    workspace_root: PathBuf,
    workspace_identity: Value,
    root: PathBuf,
    root_identity: Value,
    action: Action,
}

fn effect_root(invocation: &Invocation, workspace: &Path) -> Result<PathBuf> {
    if invocation.name == "bash" {
        return Ok(workspace.to_path_buf());
    }
    let requested = invocation
        .input
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let requested = requested_path(workspace, requested)?;
    let path = requested.as_path();
    if path.starts_with(workspace) {
        return Ok(workspace.to_path_buf());
    }
    if invocation.name == "write_file" {
        return Ok(create::nearest_parent(path)?.path().to_path_buf());
    }
    let directory = if matches!(
        invocation.name.as_str(),
        "list_dir" | "glob" | "grep" | "project_instructions"
    ) {
        if invocation.name == "grep"
            && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        {
            path.parent().context("missing external file parent")?
        } else {
            path
        }
    } else {
        path.parent().context("missing external file parent")?
    };
    let pinned = filesystem::Directory::open(directory)?;
    pinned.verify()?;
    Ok(pinned.path().to_path_buf())
}

fn verify_root(invocation: &Invocation, bound: &Bound) -> Result<()> {
    ensure!(
        bound.version == 3,
        "unsupported prepared specification version"
    );
    ensure!(
        bound.invocation_sha256 == digest(&serde_json::to_vec(invocation)?),
        "prepared invocation was changed"
    );
    ensure!(
        root_path(&invocation.workspace)? == bound.workspace_root,
        "workspace root changed since preparation"
    );
    ensure!(
        path_identity(&bound.workspace_root)? == bound.workspace_identity,
        "workspace directory changed since preparation"
    );
    let expected_root = match &bound.action {
        Action::Create {
            ancestor_parent,
            path,
            ..
        } if !path.starts_with(&bound.workspace_root) => ancestor_parent.clone(),
        _ => effect_root(invocation, &bound.workspace_root)?,
    };
    ensure!(
        expected_root == bound.root,
        "prepared path capability changed"
    );
    ensure!(
        path_identity(&bound.root)? == bound.root_identity,
        "workspace directory changed since preparation"
    );
    Ok(())
}
fn preview(invocation: &Invocation, bound: &Bound) -> Result<String> {
    Ok(match &bound.action {
        Action::Create {
            path,
            content_sha256,
            ..
        } => {
            let input: create::CreateInput = serde_json::from_value(invocation.input.clone())?;
            format!(
                "Create {}\nExact content:\n{}\nsha256 {}",
                path.display(),
                input.content,
                content_sha256
            )
        }
        Action::Repository { path, metadata, .. } => {
            let mut text = format!("{} in {}", invocation.name, path.display());
            if !metadata.is_empty() {
                text.push_str(&format!(
                    "\nExact Git metadata manifest sha256: {}",
                    digest(&serde_json::to_vec(metadata)?)
                ));
                for requirement in metadata.iter().flat_map(|metadata| &metadata.requirements) {
                    text.push_str(&format!(
                        "\nRead-only Git metadata root: {} (pointer source: {})",
                        requirement.root.display(),
                        requirement.source.display()
                    ));
                }
            }
            text
        }
        Action::Instructions { path, .. } => {
            format!("Project conventions for directory {}", path.display())
        }
        Action::Read { path, snapshot } => format!(
            "Read {} ({} bytes, sha256 {})",
            path.display(),
            snapshot.len,
            snapshot.sha256
        ),
        Action::Edit {
            path,
            snapshot,
            result_sha256,
            staged,
            backup,
        } => {
            let recovery = if cfg!(unix) { staged } else { backup };
            format!("{} {}\nBefore sha256: {}\nAfter sha256: {}\nDisplaced original retained at: {}\n{}", invocation.name, path.display(), snapshot.sha256, result_sha256, recovery.display(), mutation_preview(invocation)?)
        }
        Action::Shell {
            executable,
            dialect,
            helper,
            ..
        } => {
            let input: ShellInput = serde_json::from_value(invocation.input.clone())?;
            let worker = helper
                .as_ref()
                .map(|helper| {
                    format!(
                        "\nWorker: {} (sha256 {})",
                        helper.path.display(),
                        helper.snapshot.sha256
                    )
                })
                .unwrap_or_default();
            let probe = if dialect == "PowerShell 7" {
                "\nAfter approval: a contained PowerShell 7 version probe (up to 10 seconds) runs before the exact user command."
            } else {
                ""
            };
            format!("Shell: {}\nExecutable: {}{}{}\nArguments: {:?}\nWorkspace: {}\nTimeout: {}\nExact command:\n{}\n\n{}", dialect, executable.display(), worker, probe, shell_kind(dialect)?.command_args(), bound.root.display(), timeout_description(&input), input.command, SHELL_LIMITATION)
        }
    })
}

impl ToolService {
    pub(crate) fn display_text(
        &self,
        tool: &PreparedTool,
        outcome: &ToolOutcome,
    ) -> Result<Option<String>> {
        match tool.invocation.name.as_str() {
            "bash" => native_worker::display_output(&outcome.text).map(Some),
            "grep" | "glob" | "list_dir" => {
                presentation::repository_display(&outcome.text).map(Some)
            }
            _ => Ok(None),
        }
    }

    pub fn new(configured_shell: Option<ShellSelection>, discovery_home: Option<PathBuf>) -> Self {
        Self {
            files: Mutex::new(FileState::default()),
            configured_shell,
            discovery_home,
        }
    }

    pub(crate) fn capabilities(&self, name: &str) -> bob_kernel::ToolCapabilities {
        bob_kernel::ToolCapabilities {
            mutates_workspace: matches!(
                name,
                "edit_file" | "multi_edit" | "write_file" | "create_file" | "bash"
            ),
            executes_process: name == "bash",
            plan_safe: !matches!(
                name,
                "edit_file" | "multi_edit" | "write_file" | "create_file" | "bash"
            ),
            ..Default::default()
        }
    }

    pub(crate) fn prepared_capabilities(
        &self,
        tool: &PreparedTool,
    ) -> Result<bob_kernel::ToolCapabilities> {
        let bound: Bound = serde_json::from_value(tool.specification.clone())?;
        verify_root(&tool.invocation, &bound)?;
        ensure!(
            tool.preview == preview(&tool.invocation, &bound)?
                && tool.presentation == Some(presentation::prepared(&tool.invocation, &bound)?),
            "prepared presentation changed"
        );
        let mut capabilities = self.capabilities(&tool.invocation.name);
        capabilities.external_access = bound.root != bound.workspace_root
            || matches!(&bound.action, Action::Repository { metadata, .. } if !metadata.is_empty());
        capabilities.requires_approval = capabilities.external_access;
        if matches!(&bound.action, Action::Shell { .. }) {
            capabilities.requires_approval |= shell_policy::mandatory(&tool.invocation, &bound)?;
        }
        Ok(capabilities)
    }

    pub(crate) fn prepared_authorization_target(
        &self,
        tool: &PreparedTool,
    ) -> Result<Option<PathBuf>> {
        self.prepared_capabilities(tool)?;
        let bound: Bound = serde_json::from_value(tool.specification.clone())?;
        let path = match &bound.action {
            Action::Create { path, .. }
            | Action::Repository { path, .. }
            | Action::Instructions { path, .. }
            | Action::Read { path, .. }
            | Action::Edit { path, .. } => path,
            Action::Shell { .. } => return Ok(None),
        };
        let requested = tool
            .invocation
            .input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".");
        ensure!(
            workspace_target(&bound.root, requested)? == *path,
            "prepared primary authorization target differs from the invocation"
        );
        Ok(Some(path.clone()))
    }

    pub(crate) fn prepared_shell_command(
        &self,
        tool: &PreparedTool,
    ) -> Result<Option<bob_kernel::ShellCommandGrant>> {
        if tool.invocation.name != "bash" {
            return Ok(None);
        }
        let capabilities = self.prepared_capabilities(tool)?;
        if capabilities.external_access || capabilities.requires_approval {
            return Ok(None);
        }
        let bound: Bound = serde_json::from_value(tool.specification.clone())?;
        let Action::Shell { dialect, .. } = &bound.action else {
            bail!("expected prepared shell action");
        };
        if shell_kind(dialect)? != ShellKind::Bash {
            return Ok(None);
        }
        let input: ShellInput = serde_json::from_value(tool.invocation.input.clone())?;
        Ok(
            bob_core::core::permissions::simple_bash_program(&input.command).map(|program| {
                bob_kernel::ShellCommandGrant {
                    dialect: "bash".into(),
                    program,
                }
            }),
        )
    }

    pub(crate) fn prepared_policy(
        &self,
        tool: &PreparedTool,
        permissions: &bob_kernel::PermissionPolicy,
    ) -> Result<Option<bob_kernel::RuleDecision>> {
        if tool.invocation.name != "bash" {
            return Ok(None);
        }
        self.prepared_capabilities(tool)?;
        let bound: Bound = serde_json::from_value(tool.specification.clone())?;
        Ok(
            shell_policy::allowed(&tool.invocation, &bound, &permissions.allow_bash)?
                .then_some(bob_kernel::RuleDecision::Allow),
        )
    }

    pub(crate) fn tools(&self) -> Vec<ToolDescriptor> {
        descriptors()
    }
    pub async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
        let workspace = root_path(&invocation.workspace)?;
        let root = effect_root(&invocation, &workspace)?;
        let root_identity = path_identity(&root)?;
        let action = match invocation.name.as_str() {
            "create_file" => create::prepare(
                &root,
                &serde_json::from_value(invocation.input.clone())?,
                false,
            )?,
            "list_dir" | "glob" | "grep" => {
                let context = crate::project_instructions::DiscoveryContext::from_workspace(
                    &workspace,
                    self.discovery_home.as_deref(),
                )?;
                repository::prepare_with_context(
                    &root,
                    &invocation.name,
                    invocation.input.clone(),
                    &context,
                )?
            }
            "project_instructions" => {
                let input: PathInput = serde_json::from_value(invocation.input.clone())?;
                let directory = workspace_directory(&root, &input.path)?;
                Action::Instructions {
                    path: directory.path().to_path_buf(),
                    ancestors: directory.identities()?,
                }
            }
            "read_file" => {
                let input: ReadInput = serde_json::from_value(invocation.input.clone())?;
                let path = workspace_file(&root, &input.path)?;
                let (text, snapshot) = read_snapshot(&path)?;
                read::page(&text, &input)?;
                Action::Read { path, snapshot }
            }
            "edit_file" | "multi_edit" | "write_file" => {
                let requested = mutation_path(&invocation)?;
                let target = workspace_target(&root, &requested)?;
                let absent = matches!(fs::symlink_metadata(&target), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
                if invocation.name == "write_file" && absent {
                    let input = serde_json::from_value(invocation.input.clone())?;
                    let action = create::prepare(&root, &input, true)?;
                    return self.finish_prepare(invocation, root, root_identity, action);
                }
                let path = workspace_file(&root, &requested)?;
                let (_, snapshot) = read_snapshot(&path)?;
                let state = self.files.lock().await;
                let text = require_evidence(&state, &invocation, &root, &path, &snapshot)?;
                let result = mutation_result(text, &invocation)?;
                let parent = path.parent().context("missing file parent")?;
                let token = uuid::Uuid::new_v4();
                let staged = parent.join(format!(".bob-{token}.staged"));
                let backup = parent.join(format!(".bob-{token}.backup"));
                Action::Edit {
                    path,
                    snapshot,
                    result_sha256: digest(result.as_bytes()),
                    staged,
                    backup,
                }
            }
            "bash" => shell::prepare(&root, &invocation, self.configured_shell.as_ref())?,
            name => bail!("unknown tool {name:?}"),
        };
        self.finish_prepare(invocation, root, root_identity, action)
    }

    fn finish_prepare(
        &self,
        invocation: Invocation,
        root: PathBuf,
        root_identity: Value,
        action: Action,
    ) -> Result<PreparedTool> {
        let workspace_root = root_path(&invocation.workspace)?;
        let workspace_identity = path_identity(&workspace_root)?;
        let bound = Bound {
            version: 3,
            workspace_root,
            workspace_identity,
            invocation_sha256: digest(&serde_json::to_vec(&invocation)?),
            root,
            root_identity,
            action,
        };
        verify_root(&invocation, &bound)?;
        let preview = preview(&invocation, &bound)?;
        let presentation = Some(presentation::prepared(&invocation, &bound)?);
        Ok(PreparedTool {
            presentation,
            invocation,
            specification: serde_json::to_value(bound)?,
            preview,
        })
    }

    pub async fn execute(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        self.execute_observed(tool, cancel, None).await
    }

    pub(crate) async fn execute_observed(
        &self,
        tool: PreparedTool,
        mut cancel: watch::Receiver<bool>,
        progress: Option<bob_kernel::ToolProgress>,
    ) -> Result<ToolOutcome> {
        if cancellation_requested(&cancel) {
            return Ok(cancelled_before_effect("cancelled before execution"));
        }
        let result = self.execute_bound(tool, &mut cancel, progress).await;
        Ok(match result {
            Ok(outcome) => outcome,
            Err(error) => ToolOutcome {
                text: format!("{error:#}"),
                is_error: true,
                interrupted: false,
                presentation: None,
            },
        })
    }
}
fn cancellation_requested(cancel: &watch::Receiver<bool>) -> bool {
    *cancel.borrow() || cancel.has_changed().is_err()
}

fn cancelled_before_effect(text: &str) -> ToolOutcome {
    ToolOutcome {
        text: text.into(),
        is_error: true,
        interrupted: false,
        presentation: None,
    }
}

fn unknown_effect(text: String) -> ToolOutcome {
    ToolOutcome {
        text,
        is_error: true,
        interrupted: true,
        presentation: None,
    }
}

impl ToolService {
    async fn execute_bound(
        &self,
        tool: PreparedTool,
        cancel: &mut watch::Receiver<bool>,
        progress: Option<bob_kernel::ToolProgress>,
    ) -> Result<ToolOutcome> {
        let bound: Bound = serde_json::from_value(tool.specification)?;
        let invocation = tool.invocation;
        verify_root(&invocation, &bound)?;
        ensure!(
            tool.preview == preview(&invocation, &bound)?,
            "prepared preview was changed"
        );
        ensure!(
            tool.presentation == Some(presentation::prepared(&invocation, &bound)?),
            "prepared action presentation was changed"
        );
        match &bound.action {
            Action::Repository {
                path,
                ancestors,
                metadata,
            } => {
                let metadata = metadata.clone();
                let root = bound.root.clone();
                let path = path.clone();
                let ancestors = ancestors.clone();
                let cancel = cancel.clone();
                let workspace = bound.workspace_root.clone();
                let home = self.discovery_home.clone();
                let report = tokio::task::spawn_blocking(move || {
                    let context = crate::project_instructions::DiscoveryContext::from_workspace(
                        &workspace,
                        home.as_deref(),
                    )?;
                    repository::execute_prepared_with_context(
                        &root,
                        &invocation.name,
                        invocation.input,
                        repository::PreparedScope {
                            path: &path,
                            ancestors: &ancestors,
                        },
                        &cancel,
                        &context,
                        &metadata,
                    )
                })
                .await
                .context("repository worker failed")??;
                return Ok(ToolOutcome {
                    text: serde_json::to_string(&report)?,
                    is_error: report.cancelled || !report.errors.is_empty(),
                    interrupted: false,
                    presentation: Some(bob_kernel::ToolResultPresentation {
                        summary: Some(format!(
                            "{} results{}",
                            report.entries.len(),
                            if report.truncated {
                                " (incomplete)"
                            } else {
                                ""
                            }
                        )),
                        body_preview: presentation::repository_result(&report),
                        exit_code: None,
                    }),
                });
            }
            Action::Create { path, .. } => {
                let outcome = create::execute(&invocation, &bound, cancel)?;
                if invocation.name == "write_file" {
                    let mut state = self.files.lock().await;
                    state.reads.retain(|(_, _, target), _| target != path);
                    if !outcome.is_error && !outcome.interrupted {
                        let input: create::CreateInput =
                            serde_json::from_value(invocation.input.clone())?;
                        if let Ok(evidence_root) = effect_root(&invocation, &bound.workspace_root) {
                            record_known_write(
                                &mut state,
                                &invocation,
                                &evidence_root,
                                path,
                                &input.content,
                                true,
                            );
                        }
                    }
                }
                return Ok(outcome);
            }
            Action::Instructions { path, ancestors } => {
                ensure!(
                    invocation.name == "project_instructions",
                    "prepared tool kind mismatch"
                );
                let input: PathInput = serde_json::from_value(invocation.input)?;
                let directory = workspace_directory(&bound.root, &input.path)?;
                ensure!(
                    directory.path() == path && directory.identities()? == *ancestors,
                    "instruction scope changed since preparation"
                );
                let context = crate::project_instructions::DiscoveryContext::from_workspace(
                    &bound.workspace_root,
                    self.discovery_home.as_deref(),
                )?;
                let report = crate::project_instructions::load_with_context(
                    &bound.root,
                    &directory,
                    &context,
                    || cancellation_requested(cancel),
                )?;
                return Ok(ToolOutcome {
                    text: crate::project_instructions::render(&report),
                    is_error: report.cancelled || !report.failures.is_empty(),
                    interrupted: false,
                    presentation: Some(bob_kernel::ToolResultPresentation {
                        summary: Some(format!(
                            "{} convention sources{}",
                            report.sources.len(),
                            if report.truncated || !report.failures.is_empty() {
                                " (incomplete)"
                            } else {
                                ""
                            }
                        )),
                        body_preview: None,
                        exit_code: None,
                    }),
                });
            }
            _ => {}
        }
        if matches!(bound.action, Action::Shell { .. }) {
            return shell::execute(&invocation, &bound, cancel, progress).await;
        }
        file::execute(
            &self.files,
            invocation,
            bound,
            cancel,
            self.discovery_home.as_deref(),
        )
        .await
    }
}
#[cfg(test)]
mod capability_tests;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod repository_display_tests;
#[cfg(test)]
mod runtime_tests;
#[cfg(test)]
mod tests;
#[cfg(all(test, windows))]
mod windows_path_tests;
