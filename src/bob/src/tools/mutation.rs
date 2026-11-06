use super::*;

pub(super) fn mutation_path(invocation: &Invocation) -> Result<String> {
    Ok(match invocation.name.as_str() {
        "edit_file" => serde_json::from_value::<EditInput>(invocation.input.clone())?.path,
        "multi_edit" => serde_json::from_value::<MultiEditInput>(invocation.input.clone())?.path,
        "write_file" => {
            serde_json::from_value::<create::CreateInput>(invocation.input.clone())?.path
        }
        _ => bail!("not a file mutation"),
    })
}

pub(super) fn mutation_result(text: &str, invocation: &Invocation) -> Result<String> {
    match invocation.name.as_str() {
        "edit_file" => replacement(text, &serde_json::from_value(invocation.input.clone())?),
        "multi_edit" => {
            let input: MultiEditInput = serde_json::from_value(invocation.input.clone())?;
            ensure!(!input.edits.is_empty(), "edits must be a nonempty array");
            let mut result = text.to_owned();
            for (index, edit) in input.edits.into_iter().enumerate() {
                result = replacement(
                    &result,
                    &EditInput {
                        path: input.path.clone(),
                        old_string: edit.old_string,
                        new_string: edit.new_string,
                        replace_all: edit.replace_all,
                    },
                )
                .with_context(|| format!("edit {}", index + 1))?;
            }
            Ok(result)
        }
        "write_file" => {
            let input: create::CreateInput = serde_json::from_value(invocation.input.clone())?;
            ensure!(
                input.content.len() <= READ_LIMIT,
                "content exceeds the {READ_LIMIT}-byte limit"
            );
            Ok(input.content)
        }
        _ => bail!("not a file mutation"),
    }
}

pub(super) fn mutation_preview(invocation: &Invocation) -> Result<String> {
    Ok(match invocation.name.as_str() {
        "edit_file" => {
            let input: EditInput = serde_json::from_value(invocation.input.clone())?;
            format!(
                "Replace {}:\n{}\nWith:\n{}",
                if input.replace_all {
                    "all occurrences"
                } else {
                    "one occurrence"
                },
                input.old_string,
                input.new_string
            )
        }
        "multi_edit" => {
            let input: MultiEditInput = serde_json::from_value(invocation.input.clone())?;
            input
                .edits
                .iter()
                .enumerate()
                .map(|(index, edit)| {
                    format!(
                        "Edit {} (replace_all={}):\n{}\nWith:\n{}",
                        index + 1,
                        edit.replace_all,
                        edit.old_string,
                        edit.new_string
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        "write_file" => {
            let input: create::CreateInput = serde_json::from_value(invocation.input.clone())?;
            format!(
                "Replace complete contents ({} bytes):\n{}",
                input.content.len(),
                input.content
            )
        }
        _ => bail!("not a file mutation"),
    })
}
