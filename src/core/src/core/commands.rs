//! Markdown slash commands expand `$ARGUMENTS` and `$1`, `$2`, … into user turns.
//!
//! Search paths (a later one overrides an earlier same-named command):
//!   1. `~/.bob/commands/*.md`      — personal, available in every project
//!   2. `<repo-root>..=<cwd>/.bob/commands/*.md` — nearest project directory wins
//!
//! Optional YAML-ish frontmatter (between leading `---` fences) provides a
//! `description`. Everything after the frontmatter is the prompt body.

use super::project_context::ProjectContext;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct CustomCommand {
    /// Command name WITHOUT the leading slash (e.g. "review").
    pub name: String,
    pub description: String,
    /// The prompt template, with `$ARGUMENTS` / `$1`… placeholders intact.
    pub template: String,
}

impl CustomCommand {
    /// `$ARGUMENTS` inserts the raw arguments; `$1`, `$2`, … use shell-style splitting.
    /// Missing positions expand to empty; arguments are appended only without placeholders.
    pub fn expand(&self, args: &str) -> String {
        let positional = split_arguments(args);
        let mut out = String::new();
        let mut remaining = self.template.as_str();
        let mut references_args = false;
        while let Some(index) = remaining.find('$') {
            out.push_str(&remaining[..index]);
            remaining = &remaining[index + 1..];
            if let Some(rest) = remaining.strip_prefix("ARGUMENTS") {
                if !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
                    out.push_str(args);
                    remaining = rest;
                    references_args = true;
                    continue;
                }
            }
            let digits = remaining.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 && !remaining.starts_with('0') {
                if let Ok(position) = remaining[..digits].parse::<usize>() {
                    if let Some(value) = positional.get(position - 1) {
                        out.push_str(value);
                    }
                }
                remaining = &remaining[digits..];
                references_args = true;
            } else {
                out.push('$');
            }
        }
        out.push_str(remaining);
        if !references_args && !args.is_empty() {
            out = format!("{}\n\n{}", out.trim_end(), args);
        }
        out
    }
}

fn split_arguments(args: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = args.chars().peekable();
    while let Some(ch) = chars.next() {
        if quote == Some('\'') {
            if ch == '\'' {
                quote = None;
            } else {
                part.push(ch);
            }
        } else if ch == '\\' {
            if quote.is_none()
                || chars
                    .peek()
                    .is_some_and(|c| matches!(c, '"' | '\\' | '$' | '`' | '\n'))
            {
                if let Some(next) = chars.next() {
                    if next != '\n' {
                        part.push(next);
                    }
                } else {
                    part.push(ch);
                }
            } else {
                part.push(ch);
            }
            started = true;
        } else if quote == Some('"') {
            if ch == '"' {
                quote = None;
            } else {
                part.push(ch);
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            started = true;
        } else if ch.is_whitespace() {
            if started {
                parts.push(std::mem::take(&mut part));
                started = false;
            }
        } else {
            part.push(ch);
            started = true;
        }
    }
    if started {
        parts.push(part);
    }
    parts
}

/// Load global commands followed by bounded project ancestors, nearest last.
pub fn load_custom_commands(cwd: &Path) -> Vec<CustomCommand> {
    let global = dirs::home_dir().map(|home| home.join(".bob").join("commands"));
    load_custom_commands_from(cwd, global.as_deref())
}

pub fn load_custom_commands_from(cwd: &Path, global: Option<&Path>) -> Vec<CustomCommand> {
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<String, CustomCommand> = BTreeMap::new();
    if let Some(global) = global {
        for command in load_dir(global, None) {
            by_name.insert(command.name.clone(), command);
        }
    }
    if let Ok(context) = ProjectContext::discover(cwd) {
        for directory in context.directories() {
            for command in load_dir(&directory.join(".bob/commands"), Some(&context)) {
                by_name.insert(command.name.clone(), command);
            }
        }
    }
    by_name.into_values().collect()
}

fn load_dir(dir: &Path, context: Option<&ProjectContext>) -> Vec<CustomCommand> {
    let mut out = Vec::new();
    let bounded;
    let dir = if let Some(context) = context {
        bounded = match context.bounded_path(dir) {
            Ok(path) => path,
            Err(_) => return out,
        };
        bounded.as_path()
    } else {
        dir
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if context.is_some_and(|context| context.bounded_path(&path).is_err()) {
            continue;
        }
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
    use crate::test_support::TestDir;

    fn cmd(template: &str) -> CustomCommand {
        CustomCommand {
            name: "t".into(),
            description: String::new(),
            template: template.into(),
        }
    }

    #[test]
    fn expansion_is_one_pass_and_respects_quotes_and_multi_digit_positions() {
        for (case, template, args, expected) in [
            (
                "whole arguments",
                "Review $ARGUMENTS please",
                "src/x.rs",
                "Review src/x.rs please",
            ),
            ("positions", "$1 then $2", "a b", "a then b"),
            ("missing position", "$1 then $2", "a", "a then "),
            (
                "append without placeholders",
                "Do the thing.",
                "on foo",
                "Do the thing.\n\non foo",
            ),
            ("no arguments", "Do the thing.", "", "Do the thing."),
            (
                "whole arguments and quoted positions expand once",
                "$ARGUMENTS | $1 | $2",
                "'$2 literal' \"two words\"",
                "'$2 literal' \"two words\" | $2 literal | two words",
            ),
            (
                "multi-digit and missing positions",
                "$10 $1 $11",
                "1 2 3 4 5 6 7 8 9 ten",
                "ten 1 ",
            ),
            (
                "empty quotes and escaped spaces and quotes",
                "$1/$2/$3",
                "'' escaped\\ space \"a\\\"b\"",
                "/escaped space/a\"b",
            ),
            (
                "inserted placeholders are literal",
                "$1 $2",
                "'$ARGUMENTS' '$1'",
                "$ARGUMENTS $1",
            ),
            ("unicode", "$1", "你好", "你好"),
            (
                "placeholder suffixes remain literal",
                "$ARGUMENTS_suffix $ARGUMENTS2",
                "a",
                "$ARGUMENTS_suffix $ARGUMENTS2\n\na",
            ),
            ("zero position is literal", "$0 $01", "a", "$0 $01\n\na"),
            (
                "position overflow is missing",
                "$9999999999999999999999999999999999999999",
                "a",
                "",
            ),
            ("single quotes preserve backslashes", "$1", "'a\\b'", "a\\b"),
            (
                "double quotes preserve ordinary backslashes",
                "$1",
                "\"a\\b\"",
                "a\\b",
            ),
            (
                "adjacent quoted segments form one argument",
                "$1/$2",
                "pre\"two words\"post tail",
                "pretwo wordspost/tail",
            ),
        ] {
            assert_eq!(cmd(template).expand(args), expected, "{case}");
        }
    }

    #[test]
    fn nested_commands_override_ancestors_without_reading_siblings() {
        let root = TestDir::new();
        for directory in [
            ".git",
            ".bob/commands",
            "nested/.bob/commands",
            "sibling/.bob/commands",
            "personal",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(root.join("personal/review.md"), "personal").unwrap();
        std::fs::write(root.join(".bob/commands/review.md"), "root").unwrap();
        std::fs::write(root.join(".bob/commands/build.md"), "build").unwrap();
        std::fs::write(root.join("nested/.bob/commands/review.md"), "nearest").unwrap();
        std::fs::write(root.join("sibling/.bob/commands/leak.md"), "sibling").unwrap();
        let commands =
            load_custom_commands_from(&root.join("nested"), Some(&root.join("personal")));
        assert_eq!(
            commands
                .iter()
                .map(|command| (
                    command.name.as_str(),
                    command.description.as_str(),
                    command.template.as_str(),
                ))
                .collect::<Vec<_>>(),
            [
                ("build", "custom command (build.md)", "build"),
                ("review", "custom command (review.md)", "nearest"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_commands_reject_symlink_escape() {
        let base = TestDir::new();
        let root = base.join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".bob/commands")).unwrap();
        std::fs::write(base.join("outside.md"), "outside").unwrap();
        std::os::unix::fs::symlink(base.join("outside.md"), root.join(".bob/commands/leak.md"))
            .unwrap();
        assert!(load_custom_commands_from(&root, None).is_empty());
    }

    #[test]
    fn parses_frontmatter_description() {
        let (d, body) = parse_command("---\ndescription: Review a PR\n---\nReview $ARGUMENTS");
        assert_eq!(d, "Review a PR");
        assert_eq!(body, "Review $ARGUMENTS");
    }

    #[test]
    fn frontmatter_preserves_body_and_handles_bom_quotes_and_missing_fences() {
        for (text, description, body) in [
            ("\u{feff}---\ndescription: 'Review changes'\nignored: yes\n---\n\n  Review $1\n\nThen test.  \n", "Review changes", "Review $1\n\nThen test."),
            ("---\ndescription: \"Quoted description\"\n---\nPrompt", "Quoted description", "Prompt"),
            ("---\ntitle: not a description\n---\nPrompt", "", "Prompt"),
            ("---\ndescription: unfinished\nPrompt", "", "---\ndescription: unfinished\nPrompt"),
            ("  before\n---\ndescription: literal\n---\nafter  ", "", "before\n---\ndescription: literal\n---\nafter"),
            ("\u{feff}  Just a prompt  \n", "", "Just a prompt"),
            (" \n\t", "", ""),
        ] {
            assert_eq!(parse_command(text), (description.into(), body.into()), "{text:?}");
        }
    }

    #[test]
    fn argument_splitting_preserves_empty_words_and_shell_escape_rules() {
        for (args, expected) in [
            (" \t\n", vec![]),
            ("'' \"\" tail", vec!["", "", "tail"]),
            ("a\tb\nc", vec!["a", "b", "c"]),
            ("a\\\nb tail", vec!["ab", "tail"]),
            ("'a\\b' \"a\\b\"", vec!["a\\b", "a\\b"]),
            ("\"a\\$b\\`c\\\\d\"", vec!["a$b`c\\d"]),
            ("pre''\"two words\"post", vec!["pretwo wordspost"]),
            ("你好\u{2003}世界", vec!["你好", "世界"]),
        ] {
            assert_eq!(split_arguments(args), expected, "{args:?}");
        }
    }

    #[test]
    fn expansion_keeps_literal_dollars_and_only_trims_when_appending() {
        for (template, args, expected) in [
            ("cost $ and $HOME", "", "cost $ and $HOME"),
            ("cost $ and $HOME  \n", "extra", "cost $ and $HOME\n\nextra"),
            ("Keep whitespace  \n", "", "Keep whitespace  \n"),
            (
                "[$ARGUMENTS] [$1]",
                "  first   second  ",
                "[  first   second  ] [first]",
            ),
            ("$1/$1/$2/$3", "one two", "one/one/two/"),
            ("before $ARGUMENTS after", "", "before  after"),
            ("$ARGUMENTS!", "hello", "hello!"),
        ] {
            assert_eq!(cmd(template).expand(args), expected, "{template:?}");
        }
    }
}
