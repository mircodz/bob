use anyhow::{ensure, Context, Result};
use bob::application::{open_with_titles, LaunchOptions, SessionSelection};
use bob_kernel::{
    Command, CommandId, EntryKind, Handle, SessionCatalog, SessionQuery, SessionSnapshot,
    ThreadState,
};
use rusqlite::{Connection, OpenFlags};
use std::{path::PathBuf, time::Duration};

fn model_id(value: &str) -> Result<String> {
    let id = value.strip_prefix("copilot:").context(
        "set BOB_TEST_MODEL=copilot:<advertised-model>; direct-billing backends are not allowed",
    )?;
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "invalid explicit Copilot model ID"
    );
    Ok(value.to_owned())
}

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    database: PathBuf,
}
impl Fixture {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("bob-title-live-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root)?;
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace)?;
        std::fs::write(workspace.join(".bob.config.toml"), "provider = 'copilot'\nreasoning = ''\nmcp_servers = []\nlsp_servers = []\n[permissions]\ndefault = 'deny'\nallow = []\ndeny = []\nallow_bash = []\n")?;
        Ok(Self {
            database: root.join("lab.db"),
            workspace,
            root,
        })
    }
    fn options(&self, model: &str, selection: SessionSelection) -> LaunchOptions {
        LaunchOptions {
            database: self.database.clone(),
            workspace: self.workspace.clone(),
            provider: Some("copilot".into()),
            model: Some(model.into()),
            worker_model: None,
            reasoning: None,
            selection,
            page_size: 80,
        }
    }
    fn connection(&self) -> Result<Connection> {
        Ok(Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?)
    }
    fn counts(&self) -> Result<(i64, i64, i64)> {
        Ok(self.connection()?.query_row("SELECT (SELECT COUNT(*) FROM entries),(SELECT COUNT(*) FROM operations),(SELECT COUNT(*) FROM submissions)", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?)
    }
}

async fn turn(
    handle: &Handle,
    session: &SessionSnapshot,
    text: &str,
    expected: &str,
    replies: usize,
) -> Result<()> {
    handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread: session.thread.clone(),
            text: text.into(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let view = handle
                .transcript(Some(session.thread.clone()), None, 80)
                .await?;
            ensure!(view.error.is_none(), "kernel failure: {:?}", view.error);
            let thread = view
                .threads
                .iter()
                .find(|thread| thread.id == session.thread)
                .context("root thread missing")?;
            ensure!(
                !matches!(
                    thread.state,
                    ThreadState::Failed | ThreadState::Interrupted | ThreadState::Held
                ),
                "root did not complete: {:?}",
                thread.state
            );
            let messages: Vec<_> = view
                .entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Assistant)
                .collect();
            if thread.state == ThreadState::Completed && messages.len() == replies {
                ensure!(
                    messages.last().unwrap().text.trim() == expected,
                    "unexpected assistant response"
                );
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("normal turn timed out")?
}

async fn wait_for_title(
    fixture: &Fixture,
    handle: &Handle,
    session: &SessionSnapshot,
) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (state, selected_model, dispatched): (String, String, i64) =
                fixture.connection()?.query_row(
                    "SELECT state,model,dispatched FROM session_titles WHERE session=?1",
                    [&session.id.0],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
            ensure!(
                state != "fallback",
                "live title generation fell back instead of completing"
            );
            ensure!(
                selected_model == session.profile.model,
                "title used a different model/provider"
            );
            let current = handle.session(session.id.clone()).await?;
            let usage = handle.usage(session.id.clone()).await?;
            ensure!(usage.requests <= 2, "title generated more than once");
            if state == "generated" {
                ensure!(
                    usage.requests == 2 && dispatched == 1,
                    "title dispatch accounting mismatch"
                );
                ensure!(
                    (3..=7).contains(&current.title.split_whitespace().count()),
                    "title is not 3–7 words: {:?}",
                    current.title
                );
                ensure!(
                    current.title.chars().count() <= 120
                        && !current.title.chars().any(char::is_control),
                    "title is not bounded plain text"
                );
                ensure!(
                    current.title != session.title,
                    "title did not change from startup fallback"
                );
                ensure!(usage.compaction_requests == 0, "title triggered compaction");
                return Ok(current.title);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("generated title did not settle")?
}

async fn scenario(fixture: &Fixture, model: &str) -> Result<()> {
    let application = open_with_titles(fixture.options(model, SessionSelection::New), true).await?;
    let handle = application.handle.clone();
    let session = application.session.clone();
    let first = async {
        ensure!(handle.usage(session.id.clone()).await?.requests == 0, "startup generated a title");
        turn(&handle, &session, "We are planning a read-only Kusto MCP integration. Do not call tools. Reply only TITLE-ONE.", "TITLE-ONE", 1).await?;
        let title = wait_for_title(fixture, &handle, &session).await?;
        turn(&handle, &session, "Keep the same topic. Do not call tools. Reply only TITLE-TWO.", "TITLE-TWO", 2).await?;
        ensure!(handle.session(session.id.clone()).await?.title == title, "second turn changed the title");
        ensure!(handle.usage(session.id.clone()).await?.requests == 3, "expected two normal requests plus one title request");
        ensure!(handle.context(session.thread.clone()).await?.message_count == 4, "title polluted model context");
        ensure!(handle.jobs(session.id.clone()).await?.is_empty(), "title appeared as a job");
        let db = fixture.connection()?;
        let tool_count: i64 = db.query_row("SELECT COUNT(*) FROM operations WHERE kind='tool'", [], |row| row.get(0))?;
        ensure!(tool_count == 0, "title or normal turn admitted a tool");
        let model_count: i64 = db.query_row("SELECT COUNT(*) FROM operations WHERE kind='model' AND name='model'", [], |row| row.get(0))?;
        ensure!(model_count == 2, "unexpected normal generation operation count");
        let other_count: i64 = db.query_row("SELECT COUNT(*) FROM operations WHERE name NOT IN ('model','context')", [], |row| row.get(0))?;
        ensure!(other_count == 0, "title became a conversation operation");
        let list = handle.sessions(SessionQuery { search: Some(title.clone()), ..Default::default() }).await?;
        ensure!(list.sessions.iter().any(|row| row.id == session.id && row.title == title), "generated title is not searchable in the catalog");
        Ok::<_, anyhow::Error>(title)
    }.await;
    let shutdown = handle.shutdown().await;
    let title = first?;
    shutdown?;
    drop(application);
    let counts = fixture.counts()?;
    ensure!(
        SessionCatalog::open(&fixture.database)?
            .session(session.id.clone())?
            .title
            == title,
        "catalog lost title after close"
    );
    let resumed = open_with_titles(
        fixture.options(model, SessionSelection::Id(session.id.clone())),
        true,
    )
    .await?;
    let reopened = async {
        ensure!(resumed.session.title == title, "resume lost title");
        ensure!(
            resumed.handle.usage(session.id.clone()).await?.requests == 3,
            "resume generated another title"
        );
        ensure!(
            fixture.counts()? == counts,
            "resume changed transcript, requests, or submissions"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let shutdown = resumed.handle.shutdown().await;
    reopened?;
    shutdown?;
    drop(resumed);

    let disabled = open_with_titles(fixture.options(model, SessionSelection::New), false).await?;
    let opted_out = async {
        turn(
            &disabled.handle,
            &disabled.session,
            "Do not call tools. Reply only TITLE-OFF.",
            "TITLE-OFF",
            1,
        )
        .await?;
        ensure!(
            disabled
                .handle
                .usage(disabled.session.id.clone())
                .await?
                .requests
                == 1,
            "disabled title feature made an extra request"
        );
        let title_rows: i64 = fixture.connection()?.query_row(
            "SELECT COUNT(*) FROM session_titles WHERE session=?1",
            [&disabled.session.id.0],
            |row| row.get(0),
        )?;
        ensure!(
            title_rows == 0,
            "disabled feature reserved title generation"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let shutdown = disabled.handle.shutdown().await;
    opted_out?;
    shutdown?;
    Ok(())
}

#[tokio::test]
#[ignore = "live Copilot acceptance; set BOB_TEST_MODEL=copilot:<advertised-id> and prebuild bob-exec"]
async fn copilot_session_title_is_generated_once_and_survives_reopen() -> Result<()> {
    let model =
        model_id(&std::env::var("BOB_TEST_MODEL").context("explicit Copilot model required")?)?;
    let fixture = Fixture::new()?;
    let result = scenario(&fixture, &model).await;
    if let Err(error) = &result {
        std::fs::write(fixture.root.join("failure.txt"), format!("{error:#}"))?;
        eprintln!(
            "Private title acceptance fixture retained at {}",
            fixture.root.display()
        );
    } else {
        std::fs::remove_dir_all(&fixture.root)?;
        eprintln!("Copilot session title acceptance passed; model={model}");
    }
    result
}

#[test]
fn title_acceptance_requires_explicit_copilot_model() {
    assert!(model_id("copilot:fixture").is_ok());
    for value in [
        "fixture",
        "openai:fixture",
        "anthropic:fixture",
        "copilot:",
        "copilot:bad model",
    ] {
        assert!(model_id(value).is_err());
    }
}
