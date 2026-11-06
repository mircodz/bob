use super::*;
use crate::test_support::TestDir;

#[test]
fn first_provider_selection_creates_config_and_later_logins_keep_it() -> anyhow::Result<()> {
    for provider in ["copilot", "anthropic", "openai"] {
        let root = TestDir::new();
        let path = root.join(".bob/config.toml");
        assert!(set_default_provider_if_unset_at(&path, provider)?);
        let config = merge(BobConfig::default(), read_partial(&path)?.unwrap());
        assert_eq!(config.provider, provider);
        assert!(config.model.is_empty());
        let saved = std::fs::read(&path)?;
        for later in ["copilot", "anthropic", "openai"] {
            assert!(!set_default_provider_if_unset_at(&path, later)?);
            assert_eq!(std::fs::read(&path)?, saved);
        }
    }
    Ok(())
}

#[test]
fn first_provider_selection_preserves_comments_and_other_settings() -> anyhow::Result<()> {
    for blank in ["", "  "] {
        let root = TestDir::new();
        let before = format!("# keep this comment\nprovider = \"{blank}\" # login chooses this\nmodel = 'custom-model'\nreasoning = 'high'\ntheme = 'dark'\n[permissions]\ndefault = 'ask'\ndeny = ['bash']\n");
        let path = root.write("config.toml", &before);
        assert!(set_default_provider_if_unset_at(&path, "copilot")?);
        assert_eq!(
            std::fs::read_to_string(path)?,
            before.replacen(
                &format!("provider = \"{blank}\""),
                "provider = \"copilot\"",
                1
            )
        );
    }
    Ok(())
}

#[test]
fn first_provider_selection_fills_scaffold_without_resetting_it() -> anyhow::Result<()> {
    let root = TestDir::new();
    let before = default_config_toml();
    let path = root.write("config.toml", &before);
    assert!(set_default_provider_if_unset_at(&path, "openai")?);
    assert_eq!(
        std::fs::read_to_string(path)?,
        before.replacen("provider = \"\"", "provider = \"openai\"", 1)
    );
    Ok(())
}

#[test]
fn first_provider_selection_preserves_explicit_defaults_including_builtin() -> anyhow::Result<()> {
    let root = TestDir::new();
    for provider in ["anthropic", "copilot", "openai"] {
        let before =
            format!("# chosen explicitly\nprovider = '{provider}'\nmodel = 'custom-model'\n");
        let path = root.write(format!("{provider}.toml"), &before);
        assert!(!set_default_provider_if_unset_at(&path, "copilot")?);
        assert_eq!(std::fs::read_to_string(path)?, before);
    }
    Ok(())
}

#[test]
fn first_provider_selection_does_not_replace_invalid_or_unreadable_config() -> anyhow::Result<()> {
    let root = TestDir::new();
    for (index, before) in [
        "provider = [",
        "provider = 42\n",
        "[provider]\nid = 'openai'\n",
    ]
    .iter()
    .enumerate()
    {
        let path = root.write(format!("invalid-{index}.toml"), before);
        assert!(set_default_provider_if_unset_at(&path, "copilot").is_err());
        assert_eq!(std::fs::read_to_string(path)?, *before);
    }
    let directory = root.join("directory.toml");
    std::fs::create_dir(&directory)?;
    assert!(set_default_provider_if_unset_at(&directory, "copilot").is_err());
    assert!(directory.is_dir());
    Ok(())
}

struct ConfigWorker(Option<std::process::Child>);

impl ConfigWorker {
    fn start(root: &Path, mode: &str, value: &str) -> Self {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "core::config::login_tests::config_subprocess_worker",
                "--nocapture",
            ])
            .env("HOME", root)
            .env("USERPROFILE", root)
            .env("BOB_CONFIG_TEST_ROOT", root)
            .env("BOB_CONFIG_TEST_MODE", mode)
            .env("BOB_CONFIG_TEST_VALUE", value)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        Self(Some(child))
    }

    fn finish(mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "config subprocess timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for ConfigWorker {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn config_subprocess_worker() {
    let Ok(mode) = std::env::var("BOB_CONFIG_TEST_MODE") else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os("BOB_CONFIG_TEST_ROOT").unwrap());
    let value = std::env::var("BOB_CONFIG_TEST_VALUE").unwrap();
    assert_eq!(global_config_path().unwrap(), root.join(".bob/config.toml"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !root.join("go").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "config gate timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    match mode.as_str() {
        "model" => {
            for _ in 0..24 {
                set_default_model("openai", "fixture-model").unwrap();
            }
        }
        "reasoning" => {
            for _ in 0..24 {
                set_default_reasoning("high").unwrap();
            }
        }
        "mcp" => {
            for _ in 0..24 {
                add_mcp_server(McpServerConfig {
                    name: value.clone(),
                    command: "fixture-server".into(),
                    args: vec![],
                    env: Default::default(),
                    url: None,
                    oauth: None,
                })
                .unwrap();
            }
        }
        "theme" => {
            for _ in 0..24 {
                set_theme_in_project(&root, "light").unwrap();
            }
        }
        "lsp" => {
            for _ in 0..24 {
                add_lsp_server(
                    &root,
                    LspServerConfig {
                        name: value.clone(),
                        command: "fixture-lsp".into(),
                        args: vec![],
                        extensions: vec!["rs".into()],
                        root: ".".into(),
                    },
                )
                .unwrap();
            }
        }
        "login" => {
            let selected = set_default_provider_if_unset(&value).unwrap();
            std::fs::write(
                root.join(format!("winner-{value}")),
                if selected { "yes" } else { "no" },
            )
            .unwrap();
        }
        "init" => {
            let (_, written) = init_global_config(value == "force").unwrap();
            std::fs::write(root.join("init-result"), if written { "yes" } else { "no" }).unwrap();
        }
        _ => panic!("unknown config fixture mode"),
    }
}

#[test]
fn subprocess_config_updates_preserve_both_fields_comments_and_named_peers() {
    let root = TestDir::new();
    let path = root.write(".bob/config.toml", "# keep global\nprovider = 'anthropic' # keep choice note\nmodel = 'old' # keep model note\n[custom]\nkeep = true\n");
    let workers: Vec<_> = [
        ("model", ""),
        ("reasoning", ""),
        ("mcp", "one"),
        ("mcp", "two"),
    ]
    .into_iter()
    .map(|(mode, value)| ConfigWorker::start(&root, mode, value))
    .collect();
    root.write("go", "go");
    for worker in workers {
        worker.finish();
    }
    let text = std::fs::read_to_string(path).unwrap();
    for comment in ["# keep global", "# keep choice note", "# keep model note"] {
        assert!(text.contains(comment));
    }
    let doc: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(doc["provider"].as_str(), Some("openai"));
    assert_eq!(doc["model"].as_str(), Some("fixture-model"));
    assert_eq!(doc["reasoning"].as_str(), Some("high"));
    assert_eq!(doc["custom"]["keep"].as_bool(), Some(true));
    assert_eq!(doc["mcp_servers"].as_array().unwrap().len(), 2);
}

#[test]
fn subprocess_project_theme_and_lsp_updates_preserve_each_other() {
    let root = TestDir::new();
    std::fs::create_dir(root.join(".git")).unwrap();
    let path = root.write(".bob.config.toml", "# project\nprovider = 'copilot'\n");
    let theme = ConfigWorker::start(&root, "theme", "");
    let lsp = ConfigWorker::start(&root, "lsp", "rust");
    root.write("go", "go");
    theme.finish();
    lsp.finish();
    let text = std::fs::read_to_string(path).unwrap();
    let doc: toml::Value = toml::from_str(&text).unwrap();
    assert!(text.starts_with("# project"));
    assert_eq!(doc["theme"].as_str(), Some("light"));
    assert_eq!(doc["provider"].as_str(), Some("copilot"));
    assert_eq!(doc["lsp_servers"].as_array().unwrap().len(), 1);
}

#[test]
fn subprocess_first_login_has_exactly_one_winner_and_keeps_project_override() {
    for scaffold in [false, true] {
        let root = TestDir::new();
        std::fs::create_dir(root.join(".git")).unwrap();
        if scaffold {
            root.write(".bob/config.toml", default_config_toml());
        }
        let project = root.write(".bob.config.toml", "provider = 'project-choice'\n");
        let providers = ["anthropic", "copilot", "openai"];
        let workers: Vec<_> = providers
            .iter()
            .map(|provider| ConfigWorker::start(&root, "login", provider))
            .collect();
        root.write("go", "go");
        for worker in workers {
            worker.finish();
        }
        let winners: Vec<_> = providers
            .iter()
            .filter(|provider| {
                std::fs::read_to_string(root.join(format!("winner-{provider}"))).unwrap() == "yes"
            })
            .collect();
        assert_eq!(winners.len(), 1);
        let doc = read_config_doc_at(&root.join(".bob/config.toml")).unwrap();
        assert_eq!(doc["provider"].as_str(), Some(*winners[0]));
        assert_eq!(
            std::fs::read_to_string(project).unwrap(),
            "provider = 'project-choice'\n"
        );
        assert_eq!(
            load_project_config(BobConfig::default(), &root)
                .unwrap()
                .provider,
            "project-choice"
        );
    }
}

#[test]
fn subprocess_init_preserves_existing_unless_explicitly_forced() {
    let root = TestDir::new();
    let path = root.write(".bob/config.toml", "malformed = [");
    root.write("go", "go");
    ConfigWorker::start(&root, "init", "keep").finish();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "malformed = [");
    assert_eq!(
        std::fs::read_to_string(root.join("init-result")).unwrap(),
        "no"
    );
    ConfigWorker::start(&root, "init", "force").finish();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        default_config_toml()
    );
    assert_eq!(
        std::fs::read_to_string(root.join("init-result")).unwrap(),
        "yes"
    );
}

#[test]
fn config_mutations_preserve_semantically_invalid_input() {
    let root = TestDir::new();
    for original in ["model = 42\n", "provider = [", "reasoning = false\n"] {
        let path = root.write("config.toml", original);
        assert!(set_default_model_at(&path, "openai", "new").is_err());
        assert!(set_default_reasoning_at(&path, "high").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
