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
                .git, and target/ are skipped automatically."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression to search for." },
                    "path": { "type": "string", "description": "Directory to search (defaults to cwd)." },
                    "glob": { "type": "string", "description": "Only search files matching this glob." },
                    "ignore_case": { "type": "boolean" },
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
        let cap = input["max_results"].as_u64().unwrap_or(200) as usize;

        let root = resolve_path(&ctx.cwd, path);
        let re = match RegexBuilder::new(pattern).case_insensitive(ignore_case).build() {
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
