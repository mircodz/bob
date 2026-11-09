use super::admin::{Command, ConfigAction, LspAction, McpAction};
use super::*;
use clap::error::ErrorKind;

#[test]
fn terminal_title_opt_out_is_explicit_and_works_with_resume() {
    assert!(!Args::try_parse_from(["bob"]).unwrap().no_terminal_title);
    for tail in [
        vec!["--no-terminal-title"],
        vec!["--resume", "--no-terminal-title"],
        vec!["--resume", "saved", "--no-terminal-title"],
        vec!["--continue", "--no-terminal-title"],
        vec!["--inspect", "--no-terminal-title"],
    ] {
        let mut args = vec!["bob"];
        args.extend(tail);
        assert!(Args::try_parse_from(args).unwrap().no_terminal_title);
    }
    let help = Args::try_parse_from(["bob", "--help"])
        .unwrap_err()
        .to_string();
    assert!(help.contains("Usage: bob"));
    assert!(!help.contains("bob-lab --"));
    assert!(help.contains("--no-terminal-title"));
    assert!(help.contains("automatic session title generation"));
}

#[test]
fn resume_forms_preserve_production_semantics() {
    for (tail, resume, continuing, provider, model) in [
        (vec![], None, false, None, None),
        (vec!["--resume"], Some(""), false, None, None),
        (
            vec!["--resume", "session-123"],
            Some("session-123"),
            false,
            None,
            None,
        ),
        (
            vec!["--restore=session-123"],
            Some("session-123"),
            false,
            None,
            None,
        ),
        (
            vec!["--resume", "-p", "openai", "-m", "model"],
            Some(""),
            false,
            Some("openai"),
            Some("model"),
        ),
        (vec!["-c"], None, true, None, None),
    ] {
        let mut args = vec!["bob"];
        args.extend(tail);
        let cli = Args::try_parse_from(&args).unwrap();
        assert!(cli.command.is_none(), "{args:?}");
        assert!(cli.database.is_none(), "{args:?}");
        assert_eq!(cli.resume.as_deref(), resume, "{args:?}");
        assert_eq!(cli.r#continue, continuing, "{args:?}");
        assert_eq!(cli.provider.as_deref(), provider, "{args:?}");
        assert_eq!(cli.model.as_deref(), model, "{args:?}");
    }
}

#[test]
fn admin_commands_do_not_require_database_or_provider() {
    let parse = |args: &[&str]| Args::try_parse_from(args).unwrap().command;
    assert!(
        matches!(parse(&["bob", "login", "copilot"]), Some(Command::Login { provider }) if provider == "copilot")
    );
    assert!(
        matches!(parse(&["bob", "logout", "github"]), Some(Command::Logout { provider }) if provider == "github")
    );
    assert!(matches!(parse(&["bob", "auth"]), Some(Command::Auth)));
    assert!(matches!(
        parse(&["bob", "config"]),
        Some(Command::Config { action: None })
    ));
    assert!(matches!(
        parse(&["bob", "config", "init", "--force"]),
        Some(Command::Config {
            action: Some(ConfigAction::Init { force: true })
        })
    ));
    assert!(matches!(
        parse(&["bob", "mcp", "list"]),
        Some(Command::Mcp {
            action: McpAction::List
        })
    ));
    assert!(
        matches!(parse(&["bob", "mcp", "remove", "files"]), Some(Command::Mcp { action: McpAction::Remove { name } }) if name == "files")
    );
    assert!(matches!(
        parse(&["bob", "lsp", "list"]),
        Some(Command::Lsp {
            action: LspAction::List
        })
    ));
    assert!(
        matches!(parse(&["bob", "lsp", "remove", "rust"]), Some(Command::Lsp { action: LspAction::Remove { name } }) if name == "rust")
    );
    assert!(
        matches!(parse(&["bob", "mcp", "login", "remote", "--token", "fixture-token"]), Some(Command::Mcp { action: McpAction::Login { name, token: Some(token) } }) if name == "remote" && token == "fixture-token")
    );
}

#[test]
fn server_arguments_keep_spaces_flags_and_repeated_environment() {
    let cli = Args::try_parse_from([
        "bob",
        "mcp",
        "add",
        "files",
        "--env",
        "A=one=two",
        "-e",
        "B=hello world",
        "--",
        "node",
        "server script.js",
        "--flag",
    ])
    .unwrap();
    assert!(
        matches!(cli.command, Some(Command::Mcp { action: McpAction::Add { name, url: None, env, command } })
        if name == "files" && env == ["A=one=two", "B=hello world"] && command == ["node", "server script.js", "--flag"])
    );
    let cli = Args::try_parse_from([
        "bob",
        "lsp",
        "add",
        "ts",
        "--ext",
        "ts,tsx",
        "--root",
        "web app",
        "--",
        "typescript-language-server",
        "--stdio",
    ])
    .unwrap();
    assert!(
        matches!(cli.command, Some(Command::Lsp { action: LspAction::Add { name, ext, root, command } })
        if name == "ts" && ext == "ts,tsx" && root == "web app" && command == ["typescript-language-server", "--stdio"])
    );
}

#[test]
fn invalid_invocations_keep_actionable_errors() {
    for (args, kind) in [
        (vec!["bob", "login"], ErrorKind::MissingRequiredArgument),
        (vec!["bob", "logout"], ErrorKind::MissingRequiredArgument),
        (
            vec!["bob", "lsp", "add", "rust", "--", "rust-analyzer"],
            ErrorKind::MissingRequiredArgument,
        ),
        (
            vec!["bob", "lsp", "add", "rust", "--ext", "rs"],
            ErrorKind::MissingRequiredArgument,
        ),
        (vec!["bob", "--unknown"], ErrorKind::UnknownArgument),
        (
            vec!["bob", "config", "init", "--unknown"],
            ErrorKind::UnknownArgument,
        ),
        (
            vec!["bob", "--continue", "auth"],
            ErrorKind::ArgumentConflict,
        ),
    ] {
        let error = Args::try_parse_from(&args).unwrap_err();
        assert_eq!(error.kind(), kind, "{args:?}: {error}");
        assert!(error.to_string().contains("Usage:"));
    }
}

#[tokio::test]
async fn selection_defaults_new_and_continue_takes_precedence() {
    use bob::application::SessionSelection;
    let path = std::path::Path::new("unused.db");
    let workspace = std::path::Path::new(".");
    let parse = |tail: &[&str]| {
        let mut args = vec!["bob", "--database", "unused.db"];
        args.extend_from_slice(tail);
        Args::try_parse_from(args).unwrap()
    };
    assert!(matches!(
        selection(&parse(&[]), path, workspace).await.unwrap(),
        SessionSelection::New
    ));
    assert!(
        matches!(selection(&parse(&["--resume", "chosen"]), path, workspace).await.unwrap(), SessionSelection::Id(id) if id.0 == "chosen")
    );
    assert!(matches!(
        selection(
            &parse(&["--continue", "--resume", "ignored"]),
            path,
            workspace
        )
        .await
        .unwrap(),
        SessionSelection::Latest
    ));
}

#[test]
fn production_model_flags_are_not_restricted_to_copilot() {
    for model in ["gpt-5", "openai:gpt-5", "anthropic/claude-sonnet-4-5"] {
        let cli = Args::try_parse_from(["bob", "--database", "lab.db", "-m", model]).unwrap();
        assert_eq!(cli.model.as_deref(), Some(model));
    }
}

#[test]
fn reasoning_flag_parses_levels_and_leaves_the_lsp_root_short_flag_alone() {
    for (args, expected) in [
        (vec!["bob"], None),
        (vec!["bob", "-r", "off"], Some(ReasoningEffort::Off)),
        (
            vec!["bob", "--resume", "--reasoning", "MAX"],
            Some(ReasoningEffort::Max),
        ),
    ] {
        let cli = Args::try_parse_from(&args).unwrap();
        assert_eq!(cli.reasoning, expected, "{args:?}");
    }
    let error = Args::try_parse_from(["bob", "-r", "loud"]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ValueValidation);
    assert!(error
        .to_string()
        .contains("expected off, low, medium, high or max"));
    let cli = Args::try_parse_from([
        "bob", "lsp", "add", "ts", "--ext", "ts", "-r", "web", "--", "tsls",
    ])
    .unwrap();
    assert!(
        matches!(cli.command, Some(Command::Lsp { action: LspAction::Add { root, .. } }) if root == "web")
    );
}

#[cfg(unix)]
#[test]
fn isolated_admin_config_and_server_roundtrip() -> anyhow::Result<()> {
    const MARKER: &str = "BOB_ISOLATED_ADMIN_FIXTURE";
    if let Some(home) = std::env::var_os(MARKER) {
        let home = PathBuf::from(home);
        let config = bob_core::core::config::global_config_path()?;
        anyhow::ensure!(
            config == home.join(".bob/config.toml"),
            "fixture home must be isolated"
        );
        anyhow::ensure!(std::env::current_dir()?.canonicalize()? == home.canonicalize()?);
        let runtime = tokio::runtime::Runtime::new()?;
        let run = |args: &[&str]| -> anyhow::Result<()> {
            let command = Args::try_parse_from(args)?.command.unwrap();
            runtime.block_on(admin::run(command))
        };
        run(&["bob", "config", "init"])?;
        std::fs::write(&config, "# preserve me\nprovider = \"copilot\"\n")?;
        run(&["bob", "config", "init"])?;
        assert!(std::fs::read_to_string(&config)?.contains("# preserve me"));
        run(&[
            "bob",
            "mcp",
            "add",
            "fixture",
            "--env",
            "A=one=two",
            "--",
            "fixture-command",
            "arg with space",
        ])?;
        let servers = bob_core::core::config::list_mcp_servers()?;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].args, ["arg with space"]);
        assert_eq!(servers[0].env["A"], "one=two");
        run(&["bob", "mcp", "list"])?;
        run(&["bob", "mcp", "remove", "fixture"])?;
        assert!(bob_core::core::config::list_mcp_servers()?.is_empty());
        run(&[
            "bob",
            "lsp",
            "add",
            "fixture",
            "--ext",
            ".rs, ts",
            "--",
            "fixture-language-server",
        ])?;
        let servers = bob_core::core::config::list_lsp_servers(&home)?;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].extensions, ["rs", "ts"]);
        run(&["bob", "lsp", "list"])?;
        run(&["bob", "lsp", "remove", "fixture"])?;
        assert!(bob_core::core::config::list_lsp_servers(&home)?.is_empty());
        run(&["bob", "auth"])?;
        run(&["bob", "logout", "github"])?;
        assert!(!home.join(".bob/auth.json").exists());
        run(&["bob", "config", "init", "--force"])?;
        assert!(!std::fs::read_to_string(&config)?.contains("# preserve me"));
        return Ok(());
    }
    let fixture = TestDirectory::new("admin")?;
    let output = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "cli::tests::isolated_admin_config_and_server_roundtrip",
            "--nocapture",
        ])
        .env(MARKER, &fixture.0)
        .env("HOME", &fixture.0)
        .current_dir(&fixture.0)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "isolated fixture failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn packaged_sibling_layout_discovers_native_helper() -> anyhow::Result<()> {
    const MARKER: &str = "BOB_PACKAGED_LAYOUT_FIXTURE";
    let helper_name = format!("bob-exec{}", std::env::consts::EXE_SUFFIX);
    if let Some(directory) = std::env::var_os(MARKER) {
        let directory = PathBuf::from(directory);
        assert_eq!(
            bob::native_worker::helper_path()?,
            directory.join(helper_name).canonicalize()?
        );
        return Ok(());
    }
    let fixture = TestDirectory::new("package")?;
    let executable = fixture
        .0
        .join(format!("bob{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(std::env::current_exe()?, &executable)?;
    let helper = fixture.0.join(helper_name);
    std::fs::write(&helper, b"fixture helper, discovery only")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))?;
    }
    let output = std::process::Command::new(executable)
        .args([
            "--exact",
            "cli::tests::packaged_sibling_layout_discovers_native_helper",
            "--nocapture",
        ])
        .env(MARKER, &fixture.0)
        .env_remove("BOB_EXEC_HELPER")
        .current_dir(&fixture.0)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "packaged helper discovery failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_dir(&fixture.0)?.count(), 2);
    Ok(())
}

pub(super) struct TestDirectory(pub PathBuf);

impl TestDirectory {
    pub(super) fn new(label: &str) -> anyhow::Result<Self> {
        let path = std::env::temp_dir().join(format!("bob-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        Ok(Self(path.canonicalize()?))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
