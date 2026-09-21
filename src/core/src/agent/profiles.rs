use crate::agent::env::AgentDefinition;
use std::collections::HashMap;

pub(crate) fn with_builtins(
    system: &str,
    definitions: HashMap<String, AgentDefinition>,
) -> HashMap<String, AgentDefinition> {
    let mut profiles = HashMap::new();
    for (name, description, guidance, read_only) in [
        (
            "general-purpose",
            "General coding, implementation, and investigation tasks.",
            "Complete the delegated task within its supplied scope and report the result and verification.",
            false,
        ),
        (
            "reviewer",
            "Read-only code review for concrete bugs and regressions.",
            "Review without modifying files or running shell commands. Report concrete findings with file:line evidence and distinguish verified issues from uncertainty.",
            true,
        ),
    ] {
        profiles.insert(
            name.to_string(),
            AgentDefinition {
                description: description.into(),
                prompt: format!("{system}\n\n{guidance}"),
                tools: None,
                read_only,
                model: None,
            },
        );
    }
    profiles.extend(definitions);
    profiles
}

#[cfg(test)]
mod tests {
    use crate::agent::profiles::with_builtins;
    use std::collections::HashMap;

    #[test]
    fn builtins_preserve_context_and_explicit_definitions_win() {
        let profiles = with_builtins(
            "Project instructions: preserve compatibility",
            HashMap::new(),
        );
        assert_eq!(profiles.len(), 2);
        for profile in profiles.values() {
            assert!(profile
                .prompt
                .starts_with("Project instructions: preserve compatibility\n\n"));
            assert!(profile.tools.is_none());
        }
        assert!(!profiles["general-purpose"].read_only);
        assert!(profiles["reviewer"].read_only);
        let mut custom = profiles["general-purpose"].clone();
        custom.prompt = "caller prompt".into();
        let profiles = with_builtins("project", HashMap::from([("reviewer".into(), custom)]));
        assert_eq!(profiles["reviewer"].prompt, "caller prompt");
        assert!(!profiles["reviewer"].read_only);
    }
}
