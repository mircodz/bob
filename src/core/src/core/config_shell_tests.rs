use super::*;
use crate::test_support::TestDir;

fn layer(text: &str) -> PartialConfig {
    toml::from_str(text).unwrap()
}

#[test]
fn shell_layers_replace_the_entire_selection_in_both_directions() {
    let global = merge(
        BobConfig::default(),
        layer("bash_executable = '/global/bash'"),
    );
    let project = merge(
        global,
        layer("[shell]\ndialect = 'pwsh'\nexecutable = '/project/pwsh'"),
    );
    assert_eq!(
        project.shell,
        Some(ShellSelection {
            dialect: ShellKind::Pwsh,
            executable: "/project/pwsh".into()
        })
    );
    let nearest = merge(project, layer("bash_executable = '/nearest/bash'"));
    assert_eq!(
        nearest.shell,
        Some(ShellSelection {
            dialect: ShellKind::Bash,
            executable: "/nearest/bash".into()
        })
    );
    let inherited = merge(nearest.clone(), layer("model = 'unchanged-shell'"));
    assert_eq!(inherited.shell, nearest.shell);
    let serialized = toml::to_string(&inherited).unwrap();
    assert!(serialized.contains("[shell]"));
    assert!(!serialized.contains("bash_executable"));
}

#[test]
fn shell_config_rejects_conflict_missing_fields_and_unknown_dialect() {
    for invalid in [
        "bash_executable = '/bash'\n[shell]\ndialect = 'bash'\nexecutable = '/bash'",
        "[shell]\ndialect = 'pwsh'",
        "[shell]\nexecutable = '/pwsh'",
        "[shell]\ndialect = 'powershell'\nexecutable = '/pwsh'",
        "[shell]\ndialect = 'pwsh'\nexecutable = '/pwsh'\nunknown = true",
    ] {
        assert!(
            toml::from_str::<PartialConfig>(invalid).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn each_file_is_normalized_before_ancestor_precedence() {
    let root = TestDir::new();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("nested/deep")).unwrap();
    std::fs::write(
        root.join(".bob.config.toml"),
        "[shell]\ndialect = 'pwsh'\nexecutable = '/project/pwsh'",
    )
    .unwrap();
    std::fs::write(
        root.join("nested/.bob.config.toml"),
        "bash_executable = '/nearest/bash'",
    )
    .unwrap();
    let config = load_project_config(BobConfig::default(), &root.join("nested/deep")).unwrap();
    assert_eq!(config.shell.unwrap().dialect, ShellKind::Bash);
    std::fs::write(root.join("nested/.bob.config.toml"), "bash_executable = '/nearest/bash'\n[shell]\ndialect = 'pwsh'\nexecutable = '/nearest/pwsh'").unwrap();
    let error = load_project_config(BobConfig::default(), &root.join("nested/deep"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("conflict"), "{error}");
    assert!(error.contains(".bob.config.toml"), "{error}");
}
