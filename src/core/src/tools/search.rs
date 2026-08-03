//! Search tools: glob (find files by pattern) and grep (search file contents).

use crate::core::types::ToolSpec;
use crate::tools::builtin::resolve_path;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use regex::RegexBuilder;
use serde_json::{json, Value};
use std::path::Path;

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
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
        let pattern = input["pattern"].as_str().unwrap_or("");
        let path = input["path"].as_str().unwrap_or(".");
        let root = resolve_path(&ctx.cwd, path);
        let joined = root.join(pattern);
        let full_pattern = joined.to_string_lossy();

        // Collect (mtime, relative-path) so we can sort most-recently-modified
        // first — the ordering Claude Code uses, since the freshest files are
        // usually the relevant ones.
        let mut out: Vec<(std::time::SystemTime, String)> = Vec::new();
        match glob::glob(&full_pattern) {
            Ok(paths) => {
                for entry in paths.flatten() {
                    let s = entry.to_string_lossy();
                    if s.contains("node_modules/") || s.contains("/.git/") || s.contains("/target/")
                    {
                        continue;
                    }
                    if !entry.is_file() {
                        continue;
                    }
                    let mtime = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    let rel = entry
                        .strip_prefix(&root)
                        .unwrap_or(&entry)
                        .to_string_lossy()
                        .to_string();
                    out.push((mtime, rel));
                }
            }
            Err(e) => return Err(ToolError::invalid_input(format!("invalid glob: {}", e))),
        }
        if out.is_empty() {
            return Ok("(no matches)".to_string());
        }
        // Most recent first; ties broken by path for a stable order.
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        out.truncate(500);
        Ok(out
            .into_iter()
            .map(|(_, p)| p)
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
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
                    "context": { "type": "number", "description": "Lines of context to show around each match (like rg -C)." },
                    "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content (default), files_with_matches, or count." },
                    "max_results": { "type": "number", "description": "Cap on output lines (default 200)." }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let pattern = input["pattern"].as_str().unwrap_or("");
        if pattern.is_empty() {
            return Err(ToolError::invalid_input("pattern is required"));
        }
        let path = input["path"].as_str().unwrap_or(".");
        let root = resolve_path(&ctx.cwd, path);
        let cap = input["max_results"].as_u64().unwrap_or(200) as usize;
        let output_mode = input["output_mode"].as_str().unwrap_or("content");

        // Prefer ripgrep: it respects .gitignore, handles context/multiline/output
        // modes, and is far faster. Fall back to a pure-Rust scan if rg is missing.
        if let Some(out) = run_ripgrep(&input, pattern, &root, output_mode, cap).await {
            return out;
        }
        fallback_grep(&input, pattern, &root, &ctx.cwd, cap)
    }
}

/// Run ripgrep if it's available. Returns `None` if the `rg` binary can't be
/// spawned (not installed), so the caller can fall back. Otherwise returns the
/// tool result (Ok with matches / "(no matches)", or Err on a real rg error).
async fn run_ripgrep(
    input: &Value,
    pattern: &str,
    root: &Path,
    output_mode: &str,
    cap: usize,
) -> Option<ToolResult> {
    let mut cmd = tokio::process::Command::new("rg");
    // Stable, line-based output with paths relative to the search root.
    cmd.arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never");
    match output_mode {
        "files_with_matches" => {
            cmd.arg("--files-with-matches");
        }
        "count" => {
            cmd.arg("--count");
        }
        _ => {
            cmd.arg("--with-filename");
            if let Some(ctx_lines) = input["context"].as_u64() {
                cmd.arg("--context").arg(ctx_lines.to_string());
            }
        }
    }
    if input["ignore_case"].as_bool().unwrap_or(false) {
        cmd.arg("--ignore-case");
    }
    if input["literal"].as_bool().unwrap_or(false) {
        cmd.arg("--fixed-strings");
    }
    if input["multiline"].as_bool().unwrap_or(false) {
        cmd.arg("--multiline").arg("--multiline-dotall");
    }
    if let Some(g) = input["glob"].as_str() {
        if !g.is_empty() {
            cmd.arg("--glob").arg(g);
        }
    }
    // rg honors .gitignore by default; also skip the VCS/build dirs explicitly.
    for excl in ["!.git", "!node_modules", "!target"] {
        cmd.arg("--glob").arg(excl);
    }
    cmd.arg("--").arg(pattern).arg(root);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let out = cmd.output().await.ok()?; // None → rg not spawnable → fall back.
                                        // rg exit code 1 = no matches (not an error); 2 = real error.
    if out.status.code() == Some(2) {
        let err = String::from_utf8_lossy(&out.stderr);
        return Some(Err(ToolError::invalid_input(format!(
            "ripgrep: {}",
            err.trim()
        ))));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Make paths relative to the search root for compact, readable output.
    let root_prefix = format!("{}/", root.to_string_lossy());
    let mut lines: Vec<String> = stdout
        .lines()
        .map(|l| l.strip_prefix(&root_prefix).unwrap_or(l).to_string())
        .collect();
    if lines.is_empty() {
        return Some(Ok("(no matches)".to_string()));
    }
    let total = lines.len();
    lines.truncate(cap);
    let mut body = lines.join("\n");
    if total > cap {
        body.push_str(&format!("\n... [{} more; narrow the search]", total - cap));
    }
    Some(Ok(body))
}

/// Pure-Rust grep used when ripgrep isn't installed. Honors path/glob/literal/
/// ignore_case + the content output mode; ignores context/multiline (rg-only).
fn fallback_grep(input: &Value, pattern: &str, root: &Path, cwd: &str, cap: usize) -> ToolResult {
    let file_glob = input["glob"].as_str().unwrap_or("**/*");
    let ignore_case = input["ignore_case"].as_bool().unwrap_or(false);
    let literal = input["literal"].as_bool().unwrap_or(false);
    let re = match build_matcher(pattern, ignore_case, literal) {
        Ok(r) => r,
        Err(e) => return Err(ToolError::invalid_input(format!("invalid regex: {}", e))),
    };

    // If `glob` is a bare pattern like "*.rs", search it recursively.
    let effective_glob = if file_glob.contains('/') {
        file_glob.to_string()
    } else {
        format!("**/{}", file_glob)
    };
    let joined = root.join(&effective_glob);
    let full_pattern = joined.to_string_lossy();
    let cwd = Path::new(cwd);
    let mut results: Vec<String> = Vec::new();

    let paths = match glob::glob(&full_pattern) {
        Ok(p) => p,
        Err(e) => return Err(ToolError::invalid_input(format!("invalid glob: {}", e))),
    };
    'outer: for entry in paths.flatten() {
        let s = entry.to_string_lossy();
        if s.contains("node_modules/") || s.contains("/.git/") || s.contains("/target/") {
            continue;
        }
        if !entry.is_file() {
            continue;
        }
        let text = match std::fs::read_to_string(&entry) {
            Ok(t) => t,
            Err(_) => continue, // binary/unreadable
        };
        for (i, line) in text.split('\n').enumerate() {
            if re.is_match(line) {
                let rel = entry.strip_prefix(cwd).unwrap_or(&entry).to_string_lossy();
                let trimmed: String = line.trim().chars().take(200).collect();
                results.push(format!("{}:{}:{}", rel, i + 1, trimmed));
                if results.len() >= cap {
                    break 'outer;
                }
            }
        }
    }

    if results.is_empty() {
        Ok("(no matches)".to_string())
    } else {
        Ok(results.join("\n"))
    }
}

/// Build the grep matcher. A `literal` pattern is escaped up front; otherwise we
/// try it as a regex and, if it doesn't compile (e.g. `foo[bar` — an unclosed
/// character class), retry as a literal search of the same text. That way a
/// pattern full of regex metacharacters never hard-fails.
fn build_matcher(
    pattern: &str,
    ignore_case: bool,
    literal: bool,
) -> Result<regex::Regex, regex::Error> {
    let build = |pat: &str| RegexBuilder::new(pat).case_insensitive(ignore_case).build();
    if literal {
        build(&regex::escape(pattern))
    } else {
        build(pattern).or_else(|_| build(&regex::escape(pattern)))
    }
}

#[cfg(test)]
mod tests {
    use super::build_matcher;

    #[test]
    fn regex_metachars_fall_back_to_literal() {
        // `foo[bar` is an invalid regex (unclosed class) but a plausible literal
        // search; it must not error — it should match the literal text.
        let re = build_matcher("foo[bar", false, false).unwrap();
        assert!(re.is_match("call foo[bar] here"));
        assert!(!re.is_match("foobar"));
    }

    #[test]
    fn valid_regex_stays_a_regex() {
        let re = build_matcher(r"fn \w+\(", false, false).unwrap();
        assert!(re.is_match("fn main("));
    }

    #[test]
    fn literal_flag_escapes_metachars() {
        let re = build_matcher("a[i]", false, true).unwrap();
        assert!(re.is_match("return a[i] + 1"));
    }

    #[test]
    fn ignore_case_applies() {
        let re = build_matcher("todo", true, false).unwrap();
        assert!(re.is_match("// TODO: fix"));
    }
}
