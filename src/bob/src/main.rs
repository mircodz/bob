#[cfg(test)]
use bob::adapters;
mod cli;
mod ui;
#[cfg(target_os = "macos")]
mod voice;

use cli::Args;

use anyhow::{Context, Result};
#[cfg(test)]
use bob_kernel::RequestSpec;
use bob_kernel::{ArtifactId, SessionCatalog, ThreadId};
use clap::Parser;
#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
fn copilot_fixture_model_id(text: &str) -> std::result::Result<String, String> {
    let text = text.trim();
    let id = text.strip_prefix("copilot:").unwrap_or(text);
    if id.is_empty() || id.contains(':') || id.chars().any(char::is_whitespace) {
        Err("use a raw advertised Copilot model ID, optionally prefixed with copilot:".into())
    } else {
        Ok(format!("copilot:{id}"))
    }
}

fn page_limit(text: &str) -> std::result::Result<usize, String> {
    bounded(text, 200)
}

fn artifact_limit(text: &str) -> std::result::Result<usize, String> {
    bounded(text, 65536)
}

fn bounded(text: &str, max: usize) -> std::result::Result<usize, String> {
    let value = text.parse::<usize>().map_err(|error| error.to_string())?;
    if (1..=max).contains(&value) {
        Ok(value)
    } else {
        Err(format!("must be between 1 and {max}"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(command) = args.command {
        return cli::admin::run(command).await;
    }
    if args.inspect || args.artifact.is_some() {
        return inspect(&args).await;
    }
    let database = cli::launch::pin_database(bob::storage::resolve_database(
        args.database.as_deref(),
        dirs::home_dir,
    )?)?;
    let workspace = cli::launch::validate_workspace(&args.workspace)?;
    let selection = cli::selection(&args, &database, &workspace).await?;
    let workspace = cli::launch::resume_workspace(&database, &workspace, &selection)?;
    std::env::set_current_dir(&workspace)
        .with_context(|| format!("enter workspace {}", workspace.display()))?;
    ui::run(
        bob::application::LaunchOptions {
            database,
            workspace,
            provider: args.provider,
            model: args.model,
            worker_model: args.worker_model,
            selection,
            page_size: args.limit,
        },
        !args.no_terminal_title,
    )
    .await
}

async fn inspect(args: &Args) -> Result<()> {
    let database = bob::storage::resolve_database(args.database.as_deref(), dirs::home_dir)?;
    let catalog = SessionCatalog::open(&database)?;
    let view = if args.inspect {
        Some(serde_json::to_value(catalog.view(
            args.thread.clone().map(ThreadId),
            args.before,
            args.limit,
        )?)?)
    } else {
        None
    };
    let artifact = if let Some(id) = &args.artifact {
        let text = catalog.artifact(ArtifactId(id.clone()), args.offset, args.artifact_limit)?;
        Some(serde_json::json!({
            "artifact": id, "offset": args.offset, "limit": args.artifact_limit, "text": text
        }))
    } else {
        None
    };
    let output = match (view, artifact) {
        (Some(view), Some(artifact)) => serde_json::json!({ "view": view, "artifact": artifact }),
        (Some(view), None) => view,
        (None, Some(artifact)) => artifact,
        (None, None) => unreachable!(),
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_override_is_optional_and_inspection_is_bounded() {
        let default = Args::try_parse_from(["bob", "--inspect"]).unwrap();
        assert!(default.database.is_none());
        assert!(Args::try_parse_from([
            "bob",
            "--database",
            "bob.db",
            "--inspect",
            "--limit",
            "201"
        ])
        .is_err());
        let args = Args::try_parse_from(["bob", "--database", "bob.db", "--inspect"]).unwrap();
        assert_eq!(
            args.database.as_deref(),
            Some(std::path::Path::new("bob.db"))
        );
        assert!(args.model.is_none());
    }

    #[tokio::test]
    #[ignore = "live Copilot validation; requires explicit BOB_TEST_MODEL and BOB_TEST_WORKER_MODEL"]
    async fn copilot_root_child_file_and_shell() -> Result<()> {
        use bob_kernel::{
            Command, CommandId, EntryKind, OperationState, Policy, Profile, Runtime, ThreadState,
        };
        use bob_types::ReasoningEffort;
        use std::time::{Duration, Instant};

        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let model = copilot_fixture_model_id(
            &std::env::var("BOB_TEST_MODEL").context("explicit root model required")?,
        )
        .map_err(anyhow::Error::msg)?;
        let worker = copilot_fixture_model_id(
            &std::env::var("BOB_TEST_WORKER_MODEL").context("explicit worker model required")?,
        )
        .map_err(anyhow::Error::msg)?;
        let executor = adapters::create_executor(&[model.clone(), worker.clone()]).await?;
        let fixture =
            Fixture(std::env::temp_dir().join(format!("bob-live-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir(&fixture.0)?;
        std::fs::write(fixture.0.join("answer.txt"), "before\n")?;
        let database = fixture.0.join("bob.db");
        let handle = Runtime::open(&database, executor).await?;
        let start = Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(240), async {
            let receipt = handle.command(Command::CreateSession {
                id: CommandId::new(), workspace: fixture.0.to_string_lossy().into_owned(),
                profile: Profile { model: model.clone(), instructions: "Follow the bounded fixture task exactly. Do not explore unrelated files.".into(), read_only: false, max_requests: 16, reasoning: ReasoningEffort::Low },
                policy: Policy::default(),
            }).await?;
            handle.command(Command::Submit { id: CommandId::new(), thread: receipt.thread.clone(), text: format!("Complete these steps in order: launch exactly one read-only child using model {worker}, named reader, to read answer.txt and report its content. Wait for that child using wait_agent. Then personally read answer.txt (your child's read does not grant you edit evidence), use edit_file with path answer.txt, old_string before and new_string after. Run exactly the shell command `echo bob-check`, without extra commands. Finally answer bob-complete. Do not use shell for file operations or touch any other file.") }).await?;
            loop {
                let view = handle.view(Some(receipt.thread.clone()), None, 100).await?;
                anyhow::ensure!(view.error.is_none(), "kernel halted: {:?}", view.error);
                for decision in &view.decisions {
                    let operation = view.operations.iter().find(|op| op.id == decision.operation).context("missing decision operation")?;
                    let input = &operation.specification["invocation"]["input"];
                    let allowed = match operation.name.as_str() {
                        "edit_file" => input["path"] == "answer.txt" && input["old_string"] == "before" && input["new_string"] == "after",
                        "bash" => input["command"].as_str().is_some_and(|text| text.trim() == "echo bob-check"),
                        _ => false,
                    };
                    anyhow::ensure!(allowed, "live fixture refused unexpected effect: {} {input}", operation.name);
                    handle.command(Command::Decide { id: CommandId::new(), operation: decision.operation.clone(), revision: decision.revision, approve: true }).await?;
                }
                let root = view.threads.iter().find(|t| t.id == receipt.thread).context("root missing")?;
                anyhow::ensure!(!matches!(root.state, ThreadState::Interrupted | ThreadState::Failed | ThreadState::Cancelled), "live run stopped: {} {:?}", root.state, view.entries);
                if root.state == ThreadState::Completed {
                    anyhow::ensure!(std::fs::read_to_string(fixture.0.join("answer.txt"))? == "after\n", "fixture was not edited correctly");
                    anyhow::ensure!(view.operations.iter().any(|op| op.name == "bash" && op.state == OperationState::Done), "shell was not completed");
                    let children: Vec<_> = view.threads.iter().filter(|t| t.parent_run.is_some()).collect();
                    anyhow::ensure!(children.len() == 1 && children[0].model == worker && children[0].state == ThreadState::Completed, "wrong child model or outcome");
                    let child = handle.view(Some(children[0].id.clone()), None, 100).await?;
                    anyhow::ensure!(child.operations.iter().any(|op| op.name == "read_file" && op.state == OperationState::Done), "child did not read the fixture");
                    for (thread, expected) in [(&view, &model), (&child, &worker)] {
                        let request = thread.operations.iter().find(|op| op.name == "model").context("missing model operation")?;
                        let manifest = request.specification["manifest"].as_str().context("missing request manifest")?;
                        let request: RequestSpec = serde_json::from_str(&handle.artifact(ArtifactId(manifest.into()), 0, 256 * 1024).await?)?;
                        anyhow::ensure!(&request.model.id == expected, "manifest model mismatch");
                    }
                    println!("Copilot root={model}, child={worker}: read/edit/shell/report completed in {:?}; sequence={}", start.elapsed(), view.sequence);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok::<_, anyhow::Error>(receipt.thread)
        }).await.context("live Copilot fixture timed out");
        handle.shutdown().await?;
        let thread = result??;
        let offline = SessionCatalog::open(&database)?;
        let restored = offline.view(Some(thread), None, 100);
        anyhow::ensure!(
            restored?
                .entries
                .iter()
                .any(|entry| entry.kind == EntryKind::Assistant
                    && entry.text.contains("bob-complete")),
            "final answer was not retained"
        );
        Ok(())
    }

    #[test]
    fn model_ids_are_normalized_without_provider_fallback() {
        assert_eq!(
            copilot_fixture_model_id("copilot:gpt-5").unwrap(),
            "copilot:gpt-5"
        );
        assert_eq!(copilot_fixture_model_id("gpt-5").unwrap(), "copilot:gpt-5");
        assert!(copilot_fixture_model_id("anthropic:claude").is_err());
        assert!(copilot_fixture_model_id("copilot:").is_err());
    }
}
