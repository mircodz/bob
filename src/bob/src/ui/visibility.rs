use bob_kernel::{Entry, EntryKind, OperationState};

#[cfg(test)]
#[path = "visibility_tests.rs"]
mod tests;

pub(super) fn hidden_tool(entry: &Entry) -> bool {
    let Some(tool) = &entry.tool else {
        return false;
    };
    if matches!(tool.name.as_str(), "job_status" | "job_output") {
        return true;
    }
    if !matches!(tool.name.as_str(), "project_instructions" | "todo_write")
        || entry.kind == EntryKind::ToolError
    {
        return false;
    }
    let presentation = tool.presentation.as_ref();
    if presentation.is_some_and(|result| result.exit_code.is_some_and(|code| code != 0)) {
        return false;
    }
    match tool.state {
        OperationState::Pending
        | OperationState::Preparing
        | OperationState::Ready
        | OperationState::Started
        | OperationState::Waiting => true,
        OperationState::Done => {
            tool.name == "todo_write"
                || presentation
                    .and_then(|result| result.summary.as_ref())
                    .is_some()
        }
        _ => false,
    }
}
