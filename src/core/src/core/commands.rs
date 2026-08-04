//! User-defined slash commands loaded from markdown files. A command is a
//! markdown file whose body is a prompt template; invoking `/name args` expands
//! the template (substituting `$ARGUMENTS`, `$1`, `$2`, …) and sends it to the
//! agent as the user's turn. This mirrors Claude Code's `.claude/commands` and
//! opencode's command files.
//!
//! Search paths (a later one overrides an earlier same-named command):
//!   1. `~/.bob/commands/*.md`      — personal, available in every project
//!   2. `<cwd>/.bob/commands/*.md`  — project-specific
//!
//! Optional YAML-ish frontmatter (between leading `---` fences) provides a
//! `description`. Everything after the frontmatter is the prompt body.

use std::path::{Path, PathBuf};

/// A loaded custom command: its `/name`, a one-line description (for the menu),
/// and the raw prompt-template body.
#[derive(Clone, Debug)]
pub struct CustomCommand {
    /// Command name WITHOUT the leading slash (e.g. "review").
    pub name: String,
    pub description: String,
    /// The prompt template, with `$ARGUMENTS` / `$1`… placeholders intact.
    pub template: String,
}

impl CustomCommand {
    /// Expand the template with the user's argument string. `$ARGUMENTS` becomes
    /// the whole arg string; `$1`,`$2`,… become whitespace-split positional args
    /// (missing ones expand to empty). If the template references no placeholders
    /// and there ARE args, the args are appended so nothing is silently dropped.
    pub fn expand(&self, args: &str) -> String {
        let positional: Vec<&str> = args.split_whitespace().collect();
        let mut out = self.template.clone();
        let references_args =
            out.contains("$ARGUMENTS") || (1..=9).any(|n| out.contains(&format!("${n}")));
        out = out.replace("$ARGUMENTS", args);
        // Replace $9..$1 (high first so $1 doesn't clobber $10-style, though we
        // only support single digits).
        for n in (1..=9).rev() {
            let val = positional.get(n - 1).copied().unwrap_or("");
            out = out.replace(&format!("${n}"), val);
        }
        if !references_args && !args.is_empty() {
            out = format!("{}\n\n{}", out.trim_end(), args);
        }
        out
    }
}

/// Load all custom commands from the global (`~/.bob/commands`) then project
/// (`<cwd>/.bob/commands`) directories, project overriding global on name clash.
pub fn load_custom_commands(cwd: &Path) -> Vec<CustomCommand> {
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<String, CustomCommand> = BTreeMap::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".bob").join("commands"));
    }
    dirs.push(cwd.join(".bob").join("commands"));
    for dir in dirs {
        for cmd in load_dir(&dir) {
            by_name.insert(cmd.name.clone(), cmd);
        }
    }
    by_name.into_values().collect()
}

fn load_dir(dir: &Path) -> Vec<CustomCommand> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (description, template) = parse_command(&text);
        out.push(CustomCommand {
            name: stem.to_string(),
            description: if description.is_empty() {
                format!("custom command ({}.md)", stem)
            } else {
                description
            },
            template,
        });
    }
    out
}

/// Split optional `--- … ---` frontmatter (returning its `description:` field)
/// from the prompt body.
fn parse_command(text: &str) -> (String, String) {
    let trimmed = text.trim_start_matches(['\u{feff}']);
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            let body = &rest[end + 4..]; // skip "\n---"
            let body = body.trim_start_matches('\n');
            let mut description = String::new();
            for line in front.lines() {
                if let Some(v) = line.trim().strip_prefix("description:") {
                    description = v.trim().trim_matches(['"', '\'']).to_string();
                }
            }
            return (description, body.trim().to_string());
        }
    }
    (String::new(), trimmed.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(template: &str) -> CustomCommand {
        CustomCommand {
            name: "t".into(),
            description: String::new(),
            template: template.into(),
        }
    }

    #[test]
    fn expands_arguments_placeholder() {
        assert_eq!(
            cmd("Review $ARGUMENTS please").expand("src/x.rs"),
            "Review src/x.rs please"
        );
    }

    #[test]
    fn expands_positional() {
        assert_eq!(cmd("$1 then $2").expand("a b"), "a then b");
        assert_eq!(cmd("$1 then $2").expand("a"), "a then ");
    }

    #[test]
    fn appends_args_when_template_has_no_placeholder() {
        assert_eq!(
            cmd("Do the thing.").expand("on foo"),
            "Do the thing.\n\non foo"
        );
        assert_eq!(cmd("Do the thing.").expand(""), "Do the thing.");
    }

    #[test]
    fn parses_frontmatter_description() {
        let (d, body) = parse_command("---\ndescription: Review a PR\n---\nReview $ARGUMENTS");
        assert_eq!(d, "Review a PR");
        assert_eq!(body, "Review $ARGUMENTS");
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let (d, body) = parse_command("Just a prompt");
        assert_eq!(d, "");
        assert_eq!(body, "Just a prompt");
    }
}
