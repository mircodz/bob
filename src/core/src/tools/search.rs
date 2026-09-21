use crate::core::types::ToolSpec;
use crate::tools::filesystem::{self, MAX_OUTPUT_BYTES};
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use tokio::io::AsyncReadExt;

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".to_string(),
            description: "Find files by name using a glob pattern (e.g. 'src/**/*.rs', \
                '**/Cargo.toml'). Returns matching paths sorted by modification time, most \
                recently changed first, so the files you're likely working on surface at the top. \
                Use this to locate files when you know part of the name or extension — prefer it \
                over `find` via bash. To search file *contents*, use grep."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern, e.g. **/*.rs" },
                    "path": { "type": "string", "description": "Directory to search from (defaults to cwd)." }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let pattern = filesystem::required_string(&input, "pattern")?;
        if pattern.is_empty()
            || Path::new(pattern).is_absolute()
            || Path::new(pattern)
                .components()
                .any(|part| part == std::path::Component::ParentDir)
        {
            return Err(ToolError::invalid_input(
                "glob pattern must be nonempty and relative to path, without '..'",
            ));
        }
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| ToolError::invalid_input(format!("invalid glob: {e}")))?
            .compile_matcher();
        let root = search_root(&input, ctx)?;
        if !root.is_dir() {
            return Err(ToolError::invalid_input("glob path must be a directory"));
        }
        let mut cmd = rg_command(&root);
        cmd.arg("--files").arg("--hidden").arg("--null").arg(".");
        let collected = collect_process(cmd, 10000, 0).await?;
        let mut out = Vec::new();
        for bytes in collected
            .stdout
            .split(|byte| *byte == 0)
            .filter(|bytes| !bytes.is_empty())
        {
            let name = std::str::from_utf8(bytes).map_err(|_| {
                ToolError::invalid_input("search encountered a non-UTF-8 path; narrow the search")
            })?;
            let relative = name.strip_prefix("./").unwrap_or(name);
            if !matcher.is_match(relative) {
                continue;
            }
            let path = root.join(relative);
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_file() || !std::fs::canonicalize(&path)?.starts_with(&root) {
                continue;
            }
            out.push((
                metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                relative.to_string(),
            ));
        }
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let truncated = collected.truncated || out.len() > 500;
        out.truncate(500);
        let mut output = if out.is_empty() {
            "(no matches)".to_string()
        } else {
            out.into_iter()
                .map(|(_, path)| path)
                .collect::<Vec<_>>()
                .join("\n")
        };
        if truncated {
            output.push_str("\n... [glob results or file enumeration truncated; narrow path/pattern; ordering covers collected files only]");
        }
        Ok(filesystem::bounded_output(output).into())
    }
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".to_string(),
            description: "Search file contents with a regular expression, powered by ripgrep. \
                Returns matching lines as path:line:text. This is the right way to find where \
                something is defined or used across the codebase — prefer it over `grep`/`rg` via \
                bash. Files ignored by .gitignore, plus .git/, node_modules/, and target/, are \
                skipped automatically. \
                \n\nOptions: restrict to a subdirectory (`path`) or to files matching a `glob` \
                (e.g. '*.py', '*.{ts,tsx}'); `ignore_case`; `literal: true` to search verbatim (no \
                regex, useful for strings with metacharacters like `a[i]` or `x => y`); \
                `multiline: true` to let `.` span lines; and `context` (N lines shown around each \
                match, like rg -C). \
                Set `output_mode` to \"content\" (default, matching lines), \"files_with_matches\" \
                (just the file paths), or \"count\" (per-file match counts)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression (or literal string if `literal` is true)." },
                    "path": { "type": "string", "description": "Directory to search (defaults to cwd)." },
                    "glob": { "type": "string", "description": "Only search files matching this glob (e.g. '*.rs')." },
                    "ignore_case": { "type": "boolean" },
                    "literal": { "type": "boolean", "description": "Treat `pattern` as a literal string, not a regex." },
                    "multiline": { "type": "boolean", "description": "Allow matches to span lines (`.` matches newlines)." },
                    "context": { "type": "integer", "minimum": 0, "description": "Lines of context to show around each match (like rg -C)." },
                    "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content (default), files_with_matches, or count." },
                    "max_results": { "type": "integer", "minimum": 1, "description": "Cap on output lines (default 200)." }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let pattern = filesystem::required_string(&input, "pattern")?;
        if pattern.is_empty() {
            return Err(ToolError::invalid_input("pattern must not be empty"));
        }
        let root = search_root(&input, ctx)?;
        let cap = filesystem::optional_usize(&input, "max_results", 200)?.min(2000);
        if cap == 0 {
            return Err(ToolError::invalid_input("max_results must be positive"));
        }
        let mode = match input.get("output_mode") {
            None => "content",
            Some(v) => v
                .as_str()
                .filter(|mode| matches!(*mode, "content" | "files_with_matches" | "count"))
                .ok_or_else(|| ToolError::invalid_input("invalid output_mode"))?,
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_ripgrep(&input, pattern, &root, mode, cap),
        )
        .await
        .map_err(|_| ToolError::timeout("search exceeded 30 seconds; narrow the search"))?
    }
}

fn search_root(input: &Value, ctx: &ToolContext) -> Result<std::path::PathBuf, ToolError> {
    let path = match input.get("path") {
        None => ".",
        Some(_) => filesystem::required_path(input)?,
    };
    let root = filesystem::workspace_path(&ctx.cwd, Path::new(path))?;
    let metadata = std::fs::metadata(&root)?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(ToolError::invalid_input(
            "search path must be a directory or regular file",
        ));
    }
    Ok(root)
}

fn rg_command(root: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("rg");
    cmd.arg("--no-config")
        .arg("--no-ignore-global")
        .arg("--no-follow")
        .arg("--path-separator=/")
        .arg("--color=never");
    for exclude in ["!.git", "!node_modules", "!target"] {
        cmd.arg("--glob").arg(exclude);
    }
    cmd.current_dir(if root.is_dir() {
        root
    } else {
        root.parent().unwrap_or(root)
    });
    cmd
}

fn grep_command(
    input: &Value,
    pattern: &str,
    root: &Path,
    output_mode: &str,
) -> Result<tokio::process::Command, ToolError> {
    let mut cmd = rg_command(root);
    cmd.arg("--line-number").arg("--no-heading");
    let context = filesystem::optional_usize(input, "context", 0)?;
    if context > 2000 {
        return Err(ToolError::invalid_input(
            "context must not exceed 2000 lines",
        ));
    }
    match output_mode {
        "files_with_matches" => {
            cmd.arg("--files-with-matches");
        }
        "count" => {
            cmd.arg("--count").arg("--with-filename");
        }
        _ => {
            cmd.arg("--with-filename")
                .arg("--context")
                .arg(context.to_string());
        }
    }
    if filesystem::optional_bool(input, "ignore_case")? {
        cmd.arg("--ignore-case");
    }
    if filesystem::optional_bool(input, "literal")? {
        cmd.arg("--fixed-strings");
    }
    if filesystem::optional_bool(input, "multiline")? {
        cmd.arg("--multiline").arg("--multiline-dotall");
    }
    cmd.arg("--").arg(pattern);
    Ok(cmd)
}

async fn run_ripgrep(
    input: &Value,
    pattern: &str,
    root: &Path,
    output_mode: &str,
    cap: usize,
) -> ToolResult {
    let mut cmd = grep_command(input, pattern, root, output_mode)?;
    let collected = if let Some(value) = input.get("glob") {
        let glob = value
            .as_str()
            .ok_or_else(|| ToolError::invalid_input("glob must be a string"))?;
        if glob.is_empty() {
            cmd.arg(if root.is_dir() { Path::new(".") } else { root });
            collect_process(cmd, cap, b'\n').await?
        } else {
            let (negated, glob) = glob
                .strip_prefix('!')
                .map_or((false, glob), |glob| (true, glob));
            let glob_pattern = if glob.contains('/') {
                glob.to_string()
            } else {
                format!("**/{glob}")
            };
            let matcher = globset::GlobBuilder::new(&glob_pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| ToolError::invalid_input(format!("invalid glob: {e}")))?
                .compile_matcher();
            let (names, enumeration_truncated) = if root.is_dir() {
                let mut files = rg_command(root);
                files.arg("--files").arg("--null").arg(".");
                let enumerated = collect_process(files, 10000, 0).await?;
                let names = enumerated
                    .stdout
                    .split(|byte| *byte == 0)
                    .filter(|name| !name.is_empty())
                    .map(|name| {
                        std::str::from_utf8(name).map(str::to_string).map_err(|_| {
                            ToolError::invalid_input(
                                "search encountered a non-UTF-8 path; narrow the search",
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (names, enumerated.truncated)
            } else {
                (
                    vec![root.file_name().unwrap().to_string_lossy().into_owned()],
                    false,
                )
            };
            let base = if root.is_dir() {
                root
            } else {
                root.parent().unwrap()
            };
            let mut selected = Vec::new();
            for name in names {
                let name = name.strip_prefix("./").unwrap_or(&name);
                if matcher.is_match(name) == negated {
                    continue;
                }
                let path = base.join(name);
                if !std::fs::symlink_metadata(&path)?.is_file() {
                    continue;
                }
                if !std::fs::canonicalize(&path)?.starts_with(base) {
                    continue;
                }
                selected.push(path);
            }
            let mut result = Collected {
                stdout: Vec::new(),
                truncated: enumeration_truncated,
            };
            for chunk in selected.chunks(32) {
                let lines = result.stdout.iter().filter(|byte| **byte == b'\n').count();
                if lines >= cap || result.stdout.len() >= MAX_OUTPUT_BYTES {
                    result.truncated = true;
                    break;
                }
                let mut command = grep_command(input, pattern, root, output_mode)?;
                command.args(chunk.iter().map(|path| {
                    path.strip_prefix(base)
                        .expect("selected path is within search root")
                }));
                let next = collect_process(command, cap - lines, b'\n').await?;
                let remaining = MAX_OUTPUT_BYTES - result.stdout.len();
                result
                    .stdout
                    .extend_from_slice(&next.stdout[..next.stdout.len().min(remaining)]);
                if next.truncated || next.stdout.len() > remaining {
                    result.truncated = true;
                    break;
                }
            }
            result
        }
    } else {
        cmd.arg(if root.is_dir() { Path::new(".") } else { root });
        collect_process(cmd, cap, b'\n').await?
    };
    let text = String::from_utf8_lossy(&collected.stdout);
    let prefix = format!("{}/", root.display());
    #[cfg(windows)]
    let prefix = prefix
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .to_string();
    let mut output = text
        .lines()
        .map(|line| {
            line.strip_prefix("./")
                .or_else(|| line.strip_prefix(&prefix))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if output.is_empty() {
        output.push_str("(no matches)");
    }
    if collected.truncated {
        output.push_str("\n... [search output truncated; narrow the search or reduce context]");
    }
    Ok(output.into())
}

struct Collected {
    stdout: Vec<u8>,
    truncated: bool,
}

async fn collect_process(
    mut cmd: tokio::process::Command,
    cap: usize,
    separator: u8,
) -> Result<Collected, ToolError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ToolError::unavailable("ripgrep (rg) is required for search; install ripgrep and retry")
        } else {
            ToolError::from(error)
        }
    })?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];
    let mut out_done = false;
    let mut err_done = false;
    let mut status: Option<ExitStatus> = None;
    let mut records = 0;
    let mut truncated = false;
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(30));
    tokio::pin!(deadline);
    loop {
        if out_done && err_done && status.is_some() {
            break;
        }
        tokio::select! {
            result = stdout.read(&mut out_buf), if !out_done => {
                let length = result?;
                if length == 0 { out_done = true; }
                for byte in &out_buf[..length] {
                    if output.len() == MAX_OUTPUT_BYTES || records == cap { truncated = true; break; }
                    output.push(*byte);
                    if *byte == separator { records += 1; }
                }
                if truncated { break; }
            }
            result = stderr.read(&mut err_buf), if !err_done => {
                let length = result?;
                if length == 0 { err_done = true; }
                let remaining = (16 * 1024usize).saturating_sub(errors.len());
                errors.extend_from_slice(&err_buf[..length.min(remaining)]);
                if length > remaining {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Err(ToolError::failed(format!("ripgrep: {} [stderr truncated]", String::from_utf8_lossy(&errors))));
                }
            }
            result = child.wait(), if status.is_none() => { status = Some(result?); }
            _ = &mut deadline => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(ToolError::timeout("ripgrep exceeded 30 seconds; narrow the search"));
            }
        }
    }
    if truncated {
        let _ = child.kill().await;
        let _ = child.wait().await;
        // File enumeration must not pass a truncated path to metadata/read calls.
        if separator == 0 {
            if let Some(end) = output.iter().rposition(|byte| *byte == 0) {
                output.truncate(end + 1);
            } else {
                output.clear();
            }
        }
    } else if !matches!(status.and_then(|status| status.code()), Some(0 | 1)) {
        return Err(ToolError::failed(format!(
            "ripgrep failed: {}",
            String::from_utf8_lossy(&errors).trim()
        )));
    }
    if !errors.is_empty() {
        return Err(ToolError::failed(format!(
            "ripgrep: {}",
            String::from_utf8_lossy(&errors).trim()
        )));
    }
    Ok(Collected {
        stdout: output,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files::FileFixture as Fixture;
    use crate::tools::registry::ToolErrorKind;
    use crate::tools::schema::CompiledSchema;

    fn fixture() -> Fixture {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.dir.join(".git")).unwrap();
        fixture.write(".gitignore", "ignored.rs\n");
        fixture.write("src/main.rs", "before\nTODO a[i]\nafter\nsecond TODO\n");
        fixture.write("other.ts", "TODO typescript\n");
        fixture.write("ignored.rs", "TODO ignored\n");
        fixture.write("node_modules/hidden.rs", "TODO dependency\n");
        fixture.write("target/hidden.rs", "TODO output\n");
        fixture
    }

    #[tokio::test]
    async fn grep_honors_ignores_globs_and_output_modes() {
        let fixture = fixture();
        let output = GrepTool
            .execute(json!({"pattern":"TODO","glob":"*.{rs,ts}"}), &fixture.ctx)
            .await
            .unwrap();
        assert!(output
            .as_text()
            .unwrap()
            .contains("src/main.rs:2:TODO a[i]"));
        assert!(output
            .as_text()
            .unwrap()
            .contains("other.ts:1:TODO typescript"));
        assert!(!output.as_text().unwrap().contains("ignored"));
        assert!(!output.as_text().unwrap().contains("dependency"));
        assert!(!output.as_text().unwrap().contains("hidden.rs"));
        let files = GrepTool
            .execute(
                json!({"pattern":"TODO","glob":"*.rs","output_mode":"files_with_matches"}),
                &fixture.ctx,
            )
            .await
            .unwrap();
        assert_eq!(files.as_text().unwrap(), "src/main.rs");
        let count = GrepTool
            .execute(
                json!({"pattern":"TODO","glob":"src/*.rs","output_mode":"count"}),
                &fixture.ctx,
            )
            .await
            .unwrap();
        assert_eq!(count.as_text().unwrap(), "src/main.rs:2");
    }

    #[tokio::test]
    async fn grep_honors_literal_case_context_and_multiline() {
        let fixture = fixture();
        let literal = GrepTool
            .execute(
                json!({"pattern":"todo a[i]","literal":true,"ignore_case":true,"context":1}),
                &fixture.ctx,
            )
            .await
            .unwrap();
        assert!(literal.as_text().unwrap().contains("TODO a[i]"));
        assert!(literal.as_text().unwrap().contains("before"));
        assert!(literal.as_text().unwrap().contains("after"));
        let multiline = GrepTool
            .execute(
                json!({"pattern":"before.*after","multiline":true}),
                &fixture.ctx,
            )
            .await
            .unwrap();
        assert!(multiline.as_text().unwrap().contains("TODO a[i]"));
        assert!(GrepTool
            .execute(json!({"pattern":"foo[bar"}), &fixture.ctx)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn search_caps_are_visible_and_long_lines_are_bounded() {
        let fixture = fixture();
        let output = GrepTool
            .execute(json!({"pattern":"TODO","max_results":1}), &fixture.ctx)
            .await
            .unwrap();
        assert!(output.as_text().unwrap().contains("truncated"));
        fixture.write("long", &"x".repeat(MAX_OUTPUT_BYTES * 2));
        let output = GrepTool
            .execute(json!({"pattern":"x","path":"long"}), &fixture.ctx)
            .await
            .unwrap();
        assert!(output.as_text().unwrap().contains("truncated"));
        assert!(output.as_text().unwrap().len() < MAX_OUTPUT_BYTES + 200);
    }

    #[tokio::test]
    async fn glob_honors_ignores_braces_and_result_cap() {
        let fixture = fixture();
        let output = GlobTool
            .execute(json!({"pattern":"**/*.{rs,ts}"}), &fixture.ctx)
            .await
            .unwrap();
        assert!(output.as_text().unwrap().contains("src/main.rs"));
        assert!(output.as_text().unwrap().contains("other.ts"));
        assert!(!output.as_text().unwrap().contains("ignored"));
        assert!(!output.as_text().unwrap().contains("hidden"));
        for index in 0..501 {
            fixture.write(&format!("many/{index}.txt"), "");
        }
        let output = GlobTool
            .execute(json!({"pattern":"many/*.txt"}), &fixture.ctx)
            .await
            .unwrap();
        assert!(output.as_text().unwrap().contains("truncated"));
        assert_eq!(
            output
                .as_text()
                .unwrap()
                .lines()
                .filter(|line| line.ends_with(".txt"))
                .count(),
            500
        );
    }

    #[tokio::test]
    async fn invalid_options_and_missing_rg_are_explicit_errors() {
        let fixture = fixture();
        for input in [
            json!({}),
            json!({"pattern":1}),
            json!({"pattern":"x","output_mode":"other"}),
            json!({"pattern":"x","literal":"true"}),
            json!({"pattern":"x","max_results":0}),
        ] {
            assert_eq!(
                GrepTool
                    .execute(input, &fixture.ctx)
                    .await
                    .unwrap_err()
                    .kind,
                ToolErrorKind::InvalidInput
            );
        }
        let command = tokio::process::Command::new(fixture.dir.join("missing-rg"));
        let error = collect_process(command, 10, b'\n').await.err().unwrap();
        assert_eq!(error.kind, ToolErrorKind::Unavailable);
        assert!(error.message.contains("install ripgrep"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn search_does_not_follow_child_symlinks_or_block_on_fifos() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let fixture = fixture();
        let other = Fixture::new();
        other.write("secret.rs", "TODO other workspace\n");
        symlink(&other.dir, fixture.dir.join("linked")).unwrap();
        let fifo = fixture.dir.join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let output = GrepTool
            .execute(json!({"pattern":"TODO"}), &fixture.ctx)
            .await
            .unwrap();
        assert!(!output.as_text().unwrap().contains("other workspace"));
        assert!(GrepTool
            .execute(json!({"pattern":"TODO","path":"fifo"}), &fixture.ctx)
            .await
            .is_err());
        let output = GlobTool
            .execute(json!({"pattern":"**/*.rs"}), &fixture.ctx)
            .await
            .unwrap();
        assert!(!output.as_text().unwrap().contains("secret"));
    }

    #[tokio::test]
    async fn glob_enumerates_hidden_paths_without_bypassing_ignores() {
        let fixture = fixture();
        fixture.write(".github/workflows/check.yml", "name: check\n");
        fixture.write(".github/workflows/ignored.yml", "name: ignored\n");
        fixture.write(".hidden/nested.yml", "hidden\n");
        fixture.write(".git/private.yml", "private\n");
        fixture.write("node_modules/.hidden/dependency.yml", "dependency\n");
        fixture.write("target/.hidden/output.yml", "output\n");
        fixture.write(".github/node_modules/dependency.yml", "nested dependency\n");
        fixture.write(".github/target/output.yml", "nested output\n");
        fixture.write(".nested/.git/private.yml", "nested private\n");
        fixture.write(".gitignore", "ignored.rs\n.github/workflows/ignored.yml\n");
        fixture.write(".ignore", ".hidden/ignored.yml\n");
        fixture.write(".hidden/ignored.yml", "ignored\n");
        let output = GlobTool
            .execute(json!({"pattern":".github/workflows/*.yml"}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(output.as_text().unwrap(), ".github/workflows/check.yml");
        let output = GlobTool
            .execute(json!({"pattern":"**/*.yml"}), &fixture.ctx)
            .await
            .unwrap();
        let mut paths = output.as_text().unwrap().lines().collect::<Vec<_>>();
        paths.sort();
        assert_eq!(paths, [".github/workflows/check.yml", ".hidden/nested.yml"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hidden_glob_enumeration_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let fixture = fixture();
        let other = Fixture::new();
        let file = other.write("secret.yml", "secret\n");
        std::fs::create_dir_all(fixture.dir.join(".github/workflows")).unwrap();
        symlink(&file, fixture.dir.join(".github/workflows/linked.yml")).unwrap();
        symlink(&other.dir, fixture.dir.join(".linked")).unwrap();
        let output = GlobTool
            .execute(json!({"pattern":"**/*.yml"}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(output.as_text().unwrap(), "(no matches)");
    }

    #[test]
    fn grep_command_defaults_keep_patterns_separate_from_options() {
        let fixture = Fixture::new();
        let pattern = "--glob=*.rs\n-é[🙂]";
        let command = grep_command(&json!({}), pattern, &fixture.dir, "content").unwrap();
        let command = command.as_std();
        assert_eq!(command.get_program(), "rg");
        assert_eq!(command.get_current_dir(), Some(fixture.dir.as_ref()));
        let args = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "--no-config",
                "--no-ignore-global",
                "--no-follow",
                "--path-separator=/",
                "--color=never",
                "--glob",
                "!.git",
                "--glob",
                "!node_modules",
                "--glob",
                "!target",
                "--line-number",
                "--no-heading",
                "--with-filename",
                "--context",
                "0",
                "--",
                pattern,
            ]
        );
    }

    #[test]
    fn grep_command_parses_modes_and_boolean_flags_without_spawning() {
        let fixture = Fixture::new();
        for (mode, expected_mode) in [
            ("content", vec!["--with-filename", "--context", "2000"]),
            ("count", vec!["--count", "--with-filename"]),
            ("files_with_matches", vec!["--files-with-matches"]),
        ] {
            let command = grep_command(
                &json!({"context":2000.0,"literal":true,"ignore_case":true,"multiline":true}),
                "a[i]",
                &fixture.dir,
                mode,
            )
            .unwrap();
            let args = command
                .as_std()
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect::<Vec<_>>();
            let start = args.iter().position(|arg| *arg == "--no-heading").unwrap() + 1;
            let mut expected = expected_mode;
            expected.extend([
                "--ignore-case",
                "--fixed-strings",
                "--multiline",
                "--multiline-dotall",
                "--",
                "a[i]",
            ]);
            assert_eq!(&args[start..], expected, "{mode}");
        }
        let command = grep_command(
            &json!({"literal":false,"ignore_case":false,"multiline":false}),
            "pattern",
            &fixture.dir,
            "content",
        )
        .unwrap();
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        for flag in [
            "--fixed-strings",
            "--ignore-case",
            "--multiline",
            "--multiline-dotall",
        ] {
            assert!(!args.contains(&flag), "{flag}");
        }
        for (option, flags) in [
            ("literal", vec!["--fixed-strings"]),
            ("ignore_case", vec!["--ignore-case"]),
            ("multiline", vec!["--multiline", "--multiline-dotall"]),
        ] {
            let mut input = json!({"literal":false,"ignore_case":false,"multiline":false});
            input[option] = json!(true);
            let command =
                grep_command(&input, "pattern", &fixture.dir, "files_with_matches").unwrap();
            let args = command
                .as_std()
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect::<Vec<_>>();
            let start = args
                .iter()
                .position(|arg| *arg == "--files-with-matches")
                .unwrap()
                + 1;
            let mut expected = flags;
            expected.extend(["--", "pattern"]);
            assert_eq!(&args[start..], expected, "{option}");
        }
    }

    #[test]
    fn grep_command_rejects_malformed_options_before_spawning() {
        let fixture = Fixture::new();
        for name in ["literal", "ignore_case", "multiline"] {
            for value in [json!(null), json!(1), json!("true"), json!([])] {
                let mut input = json!({});
                input[name] = value;
                let error = grep_command(&input, "pattern", &fixture.dir, "content").unwrap_err();
                assert_eq!(error.kind, ToolErrorKind::InvalidInput);
                assert_eq!(error.message, format!("{name} must be a boolean"));
            }
        }
        for value in [json!(-1), json!(0.5), json!("1"), json!(null), json!(1e100)] {
            let error = grep_command(&json!({"context":value}), "pattern", &fixture.dir, "count")
                .unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::InvalidInput);
            assert_eq!(error.message, "context must be a nonnegative integer");
        }
        let error =
            grep_command(&json!({"context":2001}), "pattern", &fixture.dir, "content").unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::InvalidInput);
        assert_eq!(error.message, "context must not exceed 2000 lines");
    }

    #[tokio::test]
    async fn glob_rejects_invalid_patterns_before_enumerating_files() {
        let fixture = Fixture::new();
        for pattern in ["", "../*.rs", "src/../*.rs"] {
            let error = GlobTool
                .execute(json!({"pattern":pattern}), &fixture.ctx)
                .await
                .unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::InvalidInput);
            assert_eq!(
                error.message,
                "glob pattern must be nonempty and relative to path, without '..'"
            );
        }
        let absolute = fixture.dir.join("*.rs");
        let error = GlobTool
            .execute(json!({"pattern":absolute}), &fixture.ctx)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::InvalidInput);
        let error = GlobTool
            .execute(json!({"pattern":"["}), &fixture.ctx)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::InvalidInput);
        assert!(error.message.starts_with("invalid glob:"));
    }

    #[tokio::test]
    async fn grep_numeric_schema_and_execution_boundaries() {
        let fixture = fixture();
        let schema = CompiledSchema::compile(&GrepTool.spec().input_schema).unwrap();
        let output = GrepTool
            .execute(
                json!({"pattern":"TODO a","context":1.0,"max_results":3.0}),
                &fixture.ctx,
            )
            .await
            .unwrap();
        assert!(output.as_text().unwrap().contains("before"));
        assert!(output.as_text().unwrap().contains("TODO a[i]"));
        assert!(output.as_text().unwrap().contains("after"));
        let output = GrepTool
            .execute(
                json!({"pattern":"TODO a","context":0.0,"max_results":1.0}),
                &fixture.ctx,
            )
            .await
            .unwrap();
        assert!(output.as_text().unwrap().contains("TODO a[i]"));
        assert!(!output.as_text().unwrap().contains("before"));
        for name in ["context", "max_results"] {
            for (case, value, schema_accepts, execution_accepts) in [
                ("minimum", json!(1), true, true),
                ("integral float", json!(1.0), true, true),
                ("zero", json!(0.0), name == "context", name == "context"),
                ("negative", json!(-1.0), false, false),
                ("fractional", json!(1.5), false, false),
                ("overflow", json!(1e100), true, false),
                ("runtime cap", json!(2001), true, name == "max_results"),
                ("string", json!("1"), false, false),
                ("null", json!(null), false, false),
            ] {
                let mut input = json!({"pattern":"TODO"});
                input[name] = value;
                assert_eq!(
                    schema.validate(&input).is_ok(),
                    schema_accepts,
                    "{name}: {case}"
                );
                let result = GrepTool.execute(input, &fixture.ctx).await;
                assert_eq!(
                    result.is_ok(),
                    execution_accepts,
                    "{name}: {case}: {result:?}"
                );
                if let Err(error) = result {
                    assert_eq!(error.kind, ToolErrorKind::InvalidInput, "{name}: {case}");
                }
            }
        }
    }
}
