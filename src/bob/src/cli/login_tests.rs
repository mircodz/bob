#[cfg(unix)]
#[test]
fn first_successful_login_selects_global_default_without_changing_other_choices(
) -> anyhow::Result<()> {
    use super::*;
    use crate::cli::tests::TestDirectory;
    use bob::application::{provider_spec, LaunchOptions, SessionSelection};
    use bob_core::auth::{AuthStore, Credential};
    use bob_core::core::config::global_config_path;
    use clap::Parser;
    use std::path::PathBuf;

    const MARKER: &str = "BOB_LOGIN_DEFAULT_FIXTURE";
    if let Some(home) = std::env::var_os(MARKER) {
        let home = PathBuf::from(home);
        let path = global_config_path()?;
        anyhow::ensure!(
            path == home.join(".bob/config.toml"),
            "fixture home must be isolated"
        );
        anyhow::ensure!(std::env::current_dir()?.canonicalize()? == home.canonicalize()?);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        assert!(runtime
            .block_on(run_login("unknown-fixture-provider"))
            .is_err());
        assert!(!path.exists());

        runtime.block_on(AuthStore::set_credential(
            "copilot",
            Credential {
                token: "fixture-not-a-token".into(),
                ..Default::default()
            },
        ))?;
        let auth_path = home.join(".bob/auth.json");
        let credentials = std::fs::read(&auth_path)?;
        done("Copilot", "copilot")?;
        assert_eq!(std::fs::read(&auth_path)?, credentials);
        let config = load_config(&home)?;
        assert_eq!(config.provider, "copilot");
        let args = crate::cli::Args::try_parse_from(["bob"])?;
        let options = LaunchOptions {
            database: home.join("unused.sqlite"),
            workspace: home.clone(),
            provider: args.provider,
            model: args.model,
            worker_model: args.worker_model,
            selection: SessionSelection::New,
            page_size: args.limit,
        };
        assert_eq!(provider_spec(&options, &config), "copilot");
        let chosen = std::fs::read(&path)?;

        runtime.block_on(AuthStore::set_credential(
            "openai",
            Credential {
                token: "fixture-not-a-token".into(),
                ..Default::default()
            },
        ))?;
        let credentials = std::fs::read(&auth_path)?;
        done("OpenAI", "openai")?;
        assert_eq!(std::fs::read(&path)?, chosen);
        assert_eq!(std::fs::read(&auth_path)?, credentials);

        let project_path = home.join(".bob.config.toml");
        let project = "# explicit project choice\nprovider = 'anthropic'\n";
        std::fs::write(&project_path, project)?;
        done("Copilot", "copilot")?;
        assert_eq!(std::fs::read(&path)?, chosen);
        assert_eq!(std::fs::read_to_string(&project_path)?, project);
        assert_eq!(load_config(&home)?.provider, "anthropic");

        let malformed = "provider = [";
        std::fs::write(&path, malformed)?;
        let error = done("OpenAI", "openai").unwrap_err();
        assert!(error.to_string().contains("saved credentials"));
        assert!(error.to_string().contains("bob --provider openai"));
        assert_eq!(std::fs::read_to_string(&path)?, malformed);
        assert_eq!(std::fs::read(&auth_path)?, credentials);
        assert!(!options.database.exists());
        return Ok(());
    }

    let fixture = TestDirectory::new("login-default")?;
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "cli::admin::login_tests::first_successful_login_selects_global_default_without_changing_other_choices", "--nocapture"])
        .env(MARKER, &fixture.0)
        .env("HOME", &fixture.0)
        .env("USERPROFILE", &fixture.0)
        .current_dir(&fixture.0)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "isolated login fixture failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(text.matches("Set as your default provider").count(), 1);
    assert!(text.contains("Start with:  bob"));
    assert!(text.contains("bob --provider openai"));
    Ok(())
}
