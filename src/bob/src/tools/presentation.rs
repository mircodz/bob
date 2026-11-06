use super::*;
use bob_kernel::{ToolBody, ToolPresentation};

pub(super) fn repository_display(text: &str) -> Result<String> {
    let report: repository::SearchReport<String> = serde_json::from_str(text)?;
    let mut display = String::new();
    if report.cancelled {
        display.push_str("[!] Search cancelled.\n");
    }
    display.push_str(&repository::render(&report));
    Ok(display)
}

pub(super) fn repository_result(report: &repository::SearchReport) -> Option<String> {
    const FOOTER: &str = "\n[Preview; inspect for full result]";
    const LIMIT: usize = 2048 - FOOTER.len();
    let mut preview = String::new();
    if report.cancelled {
        preview.push_str("[!] Search cancelled.\n");
    }
    if report.truncated {
        preview
            .push_str("[!] Search incomplete; narrow the query or inspect continuation details.\n");
    }
    if let Some(error) = report.errors.first() {
        preview.push_str(&format!("[!] {error}\n"));
    }
    preview.push_str(&repository::render(report));
    if preview.len() > LIMIT {
        let mut end = LIMIT;
        while !preview.is_char_boundary(end) {
            end -= 1;
        }
        preview.truncate(end);
    }
    preview.push_str(FOOTER);
    Some(preview)
}

pub(super) fn diff_preview(before: &str, after: &str, limit: usize) -> String {
    let old: Vec<_> = before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let removed = old.len() - prefix - suffix;
    let added = new.len() - prefix - suffix;
    if removed == 0 && added == 0 {
        return "No content changes.".into();
    }
    let start = prefix.saturating_sub(3);
    let old_end = (old.len() - suffix + 3).min(old.len());
    let new_end = (new.len() - suffix + 3).min(new.len());
    let mut out = format!(
        "+{added} -{removed}\n@@ -{},{} +{},{} @@\n",
        start + 1,
        old_end - start,
        start + 1,
        new_end - start
    );
    for (marker, line) in old[start..prefix]
        .iter()
        .map(|line| (' ', line))
        .chain(
            old[prefix..old.len() - suffix]
                .iter()
                .map(|line| ('-', line)),
        )
        .chain(
            new[prefix..new.len() - suffix]
                .iter()
                .map(|line| ('+', line)),
        )
        .chain(
            new[new.len() - suffix..new_end]
                .iter()
                .map(|line| (' ', line)),
        )
    {
        if out.len() + line.len() + 2 > limit {
            out.push_str(
                "... [diff preview truncated; exact contents retained in prepared action]\n",
            );
            return out;
        }
        out.push(marker);
        out.push_str(line);
        if !line.ends_with('\n') {
            out.push_str("\n\\ No newline at end of file\n");
        }
    }
    out
}

pub(super) fn prepared(invocation: &Invocation, bound: &Bound) -> Result<ToolPresentation> {
    let mut details = vec![format!("Workspace: {}", bound.workspace_root.display())];
    if bound.root != bound.workspace_root {
        details.push(format!(
            "External path capability: {} (explicit approval required in Normal mode)",
            bound.root.display()
        ));
    }
    if let Action::Repository { metadata, .. } = &bound.action {
        for requirement in metadata.iter().flat_map(|metadata| &metadata.requirements) {
            details.push(format!(
                "External Git metadata root: {} (read-only; {:?} pointer: {}; explicit approval required in Normal mode)",
                requirement.root.display(), requirement.kind, requirement.source.display()
            ));
        }
        if !metadata.is_empty() {
            details.push(format!("Exact Git metadata manifest sha256: {}. Only pinned pointer files and info/exclude; file traversal stays within {}.", digest(&serde_json::to_vec(metadata)?), bound.root.display()));
        }
    }
    let (title, target, body) = match &bound.action {
        Action::Read { path, snapshot } => (
            "Read file".into(),
            Some(path.display().to_string()),
            ToolBody::Text {
                text: format!("UTF-8 file ({} bytes); numbered output is paged and clipped with continuation cursors", snapshot.len),
            },
        ),
        Action::Edit {
            path,
            snapshot,
            result_sha256,
            staged,
            backup,
        } => {
            details.extend([
                format!("Before sha256: {}", snapshot.sha256),
                format!("After sha256: {result_sha256}"),
                format!(
                    "Displaced original retained at: {}",
                    if cfg!(unix) { staged } else { backup }.display()
                ),
            ]);
            (
                match invocation.name.as_str() { "write_file" => "Write file", "multi_edit" => "Edit file", _ => "Edit file" }.into(),
                Some(path.display().to_string()),
                match invocation.name.as_str() {
                    "edit_file" => {
                        let input: EditInput = serde_json::from_value(invocation.input.clone())?;
                        ToolBody::Edit { old_string: input.old_string, new_string: input.new_string }
                    }
                    "write_file" => {
                        let input: create::CreateInput = serde_json::from_value(invocation.input.clone())?;
                        ToolBody::Create { content: input.content }
                    }
                    _ => ToolBody::Text { text: mutation_preview(invocation)? },
                },
            )
        }
        Action::Create {
            path,
            ancestor_parent,
            content_sha256,
            ..
        } => {
            let input: create::CreateInput = serde_json::from_value(invocation.input.clone())?;
            details.push(format!("Pinned existing parent: {}. Missing descendants to create: {}", ancestor_parent.display(), path.parent().context("missing creation parent")?.strip_prefix(ancestor_parent)?.display()));
            details.push(format!(
                "Create only; never overwrite. {} bytes, sha256 {content_sha256}",
                input.content.len()
            ));
            (
                "Create file".into(),
                Some(path.display().to_string()),
                ToolBody::Create {
                    content: input.content,
                },
            )
        }
        Action::Shell {
            executable,
            snapshot,
            dialect,
            helper,
        } => {
            let input: ShellInput = serde_json::from_value(invocation.input.clone())?;
            details.extend([
                format!("Dialect: {dialect}"),
                format!(
                    "Executable: {} (sha256 {})",
                    executable.display(),
                    snapshot.sha256
                ),
                format!("Timeout: {}", timeout_description(&input)),
                SHELL_LIMITATION.into(),
            ]);
            if dialect == "PowerShell 7" {
                details.push("After approval, a contained PowerShell 7 version probe runs before the command (up to 10 seconds).".into());
            }
            if let Some(helper) = helper {
                details.push(format!(
                    "Worker: {} (sha256 {})",
                    helper.path.display(),
                    helper.snapshot.sha256
                ));
            }
            (
                "Run shell command".into(),
                Some(bound.root.display().to_string()),
                ToolBody::Command {
                    command: input.command,
                },
            )
        }
        Action::Repository { path, .. } => (
            match invocation.name.as_str() {
                "list_dir" => "List directory",
                "glob" => "Find paths",
                _ => "Search contents",
            }
            .into(),
            Some(path.display().to_string()),
            ToolBody::Text {
                text: invocation.input.get("pattern").and_then(Value::as_str).map(|pattern| format!("Pattern: {pattern}")).unwrap_or_else(|| "Immediate directory entries".into()),
            },
        ),
        Action::Instructions { path, .. } => (
            "Read project conventions".into(),
            Some(path.display().to_string()),
            ToolBody::Text {
                text: crate::project_instructions::CONTRACT.into(),
            },
        ),
    };
    Ok(ToolPresentation {
        title,
        target,
        body,
        details,
    })
}
