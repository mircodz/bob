use super::*;
use bob_kernel::ToolSummary;

pub(crate) fn summary(tool: &ToolSummary) -> Option<String> {
    (tool.name == "read_file")
        .then(|| tool.presentation.as_ref()?.summary.as_deref().map(safe))
        .flatten()
        .map(|summary| summary.replace(" (incomplete)", ""))
        .filter(|summary| !summary.is_empty())
}
