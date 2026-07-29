//! Search tools: glob (find files by pattern) and grep (search file contents).

use crate::core::types::ToolSpec;
use crate::tools::builtin::resolve_path;
use crate::tools::registry::{Tool, ToolContext};
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
                '**/Cargo.toml'). Returns matching paths relative to the search root, sorted. Use \
                this to locate files when you know part of the name or extension — prefer it over \
                `find` via bash. To search file *contents*, use grep."
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let pattern = input["pattern"].as_str().unwrap_or("");
        let path = input["path"].as_str().unwrap_or(".");
        let root = resolve_path(&ctx.cwd, path);
        let joined = root.join(pattern);
        let full_pattern = joined.to_string_lossy();

        let mut out: Vec<String> = Vec::new();
        match glob::glob(&full_pattern) {
            Ok(paths) => {
                for entry in paths.flatten() {
                    if entry.is_file() {
                        let rel = entry
                            .strip_prefix(&root)
                            .unwrap_or(&entry)
                            .to_string_lossy()
                            .to_string();
                        out.push(rel);
                    }
                }
            }
            Err(e) => return format!("error: invalid glob: {}", e),
        }
        if out.is_empty() {
            return "(no matches)".to_string();
        }
        out.sort();
        out.truncate(500);
        out.join("\n")
    }
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".to_string(),
            description: "Search file contents with a regular expression. Returns matching lines \
                as path:line:text. Optionally restrict to a subdirectory (`path`) or to files \
                matching a `glob`. This is the right way to find where something is defined or \
                used across the codebase — prefer it over `grep`/`rg` via bash. Node_modules, \
                .git, and target/ are skipped automatically. Set `literal: true` to search for the \
                pattern verbatim (no regex) — useful for strings with regex metacharacters like \
                `#[derive` or `Vec<T>`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression (or literal string if `literal` is true)." },
                    "path": { "type": "string", "description": "Directory to search (defaults to cwd)." },
                    "glob": { "type": "string", "description": "Only search files matching this glob." },
                    "ignore_case": { "type": "boolean" },
                    "literal": { "type": "boolean", "description": "Treat `pattern` as a literal string, not a regex." },
                    "max_results": { "type": "number", "description": "Cap on matching lines (default 200)." }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let pattern = input["pattern"].as_str().unwrap_or("");
        let path = input["path"].as_str().unwrap_or(".");
        let file_glob = input["glob"].as_str().unwrap_or("**/*");
        let ignore_case = input["ignore_case"].as_bool().unwrap_or(false);
        let literal = input["literal"].as_bool().unwrap_or(false);
        let cap = input["max_results"].as_u64().unwrap_or(200) as usize;

        let root = resolve_path(&ctx.cwd, path);
        // Build the matcher. If `literal` is set, escape the pattern up front. If a
        // regex fails to compile (e.g. the model searched for `#[derive`, an
        // unclosed character class), fall back to a literal search of the same
        // text instead of hard-erroring — that's almost always what was intended.
        let re = match build_matcher(pattern, ignore_case, literal) {
            Ok(r) => r,
            Err(e) => return format!("error: invalid regex: {}", e),
        };

        let joined = root.join(file_glob);
        let full_pattern = joined.to_string_lossy();
        let cwd = Path::new(&ctx.cwd);
        let mut results: Vec<String> = Vec::new();

        let paths = match glob::glob(&full_pattern) {
            Ok(p) => p,
            Err(e) => return format!("error: invalid glob: {}", e),
        };
        'outer: for entry in paths.flatten() {
            let s = entry.to_string_lossy();
            if s.contains("node_modules/") || s.contains(".git/") || s.contains("/target/") {
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
            "(no matches)".to_string()
        } else {
            results.join("\n")
        }
    }
}

/// Build the grep matcher. A `literal` pattern is escaped up front; otherwise we
/// try it as a regex and, if it doesn't compile (e.g. `#[derive` — an unclosed
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
        // `#[derive` is an invalid regex (unclosed class) but a common literal
        // search; it must not error — it should match the literal text.
        let re = build_matcher("#[derive", false, false).unwrap();
        assert!(re.is_match("#[derive(Clone)]"));
        assert!(!re.is_match("derive"));
    }

    #[test]
    fn valid_regex_stays_a_regex() {
        let re = build_matcher(r"fn \w+\(", false, false).unwrap();
        assert!(re.is_match("fn main("));
    }

    #[test]
    fn literal_flag_escapes_metachars() {
        let re = build_matcher("Vec<T>", false, true).unwrap();
        assert!(re.is_match("let v: Vec<T> = ..."));
    }

    #[test]
    fn ignore_case_applies() {
        let re = build_matcher("todo", true, false).unwrap();
        assert!(re.is_match("// TODO: fix"));
    }
}
