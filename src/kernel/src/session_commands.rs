use crate::contracts::*;
use crate::model::*;
use crate::store::*;
use anyhow::{bail, ensure, Context};
use rusqlite::{params, Transaction};

pub(crate) fn invalidate_approvals(
    tx: &Transaction<'_>,
    session: &SessionId,
) -> anyhow::Result<()> {
    tx.execute("UPDATE decisions SET state='stale' WHERE operation IN (SELECT o.id FROM operations o JOIN threads t ON t.id=o.thread WHERE t.session=?1 AND o.state IN ('approval','ready'))",[session])?;
    tx.execute("UPDATE operations SET state='pending',prepared=NULL WHERE thread IN (SELECT id FROM threads WHERE session=?1) AND state IN ('approval','ready')",[session])?;
    tx.execute("UPDATE human_controls SET revision=(SELECT revision FROM sessions WHERE id=?1) WHERE state='pending' AND operation IN (SELECT o.id FROM operations o JOIN threads t ON t.id=o.thread WHERE t.session=?1)",[session])?;
    Ok(())
}

pub(crate) fn grant_approved(
    tx: &Transaction<'_>,
    session: &SessionId,
    operation: &OperationId,
    grant: &PermissionGrant,
) -> anyhow::Result<()> {
    let (name, prepared, authorizations): (String, Option<String>, String) = tx.query_row(
        "SELECT name,prepared,authorizations FROM operations WHERE id=?1",
        [operation],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let authorizations: Vec<PreparedAuthorizationRequirement> = decode(authorizations)?;
    let prepared_tool: PreparedTool = decode(
        prepared
            .clone()
            .context("approved operation lacks prepared input")?,
    )?;
    let input = prepared_tool.invocation.input;
    ensure!(grant.tool == name, "grant must match the approved tool");
    if let Some(shell) = &grant.shell_command {
        ensure!(
            name == "bash"
                && shell.well_formed()
                && grant.command.is_none()
                && grant.path_prefix.is_none(),
            "shell program grants require an offered Bash-only program scope"
        );
    } else if name == "bash" {
        ensure!(
            grant.command.as_deref() == input["command"].as_str()
                && grant.command.is_some()
                && grant.path_prefix.is_none(),
            "shell grants must bind the exact approved command"
        );
    }
    if let Some(command) = &grant.command {
        ensure!(
            input["command"].as_str() == Some(command),
            "grant command differs from the approved invocation"
        );
    }
    if let Some(prefix) = &grant.path_prefix {
        let path = authorizations
            .iter()
            .find_map(|requirement| match requirement {
                PreparedAuthorizationRequirement::ToolPath { path } => Some(path.clone()),
                PreparedAuthorizationRequirement::EditFile { .. } => None,
            })
            .or_else(|| {
                input["path"].as_str().and_then(|path| {
                    crate::policy::grant_path(&prepared_tool.invocation.workspace, path)
                })
            })
            .context("path grant requires an approved absolute target")?;
        ensure!(
            path.is_absolute() && path == std::path::Path::new(prefix),
            "path grant must equal the prepared primary authorization target"
        );
    }
    let mut permissions = crate::policy::permissions(
        tx,
        &ThreadId(tx.query_row(
            "SELECT thread FROM operations WHERE id=?1",
            [operation],
            |r| r.get(0),
        )?),
    )?;
    ensure!(
        permissions.grants.len() < 1024,
        "permission grant limit reached"
    );
    permissions.grants.push(grant.clone());
    tx.execute(
        "UPDATE sessions SET permissions=?1,revision=revision+1 WHERE id=?2",
        params![encode(&permissions)?, session],
    )?;
    invalidate_approvals(tx, session)?;
    tx.execute("UPDATE operations SET state='ready',prepared=?1,policy_revision=(SELECT revision FROM sessions WHERE id=?2) WHERE id=?3",params![prepared,session,operation])?;
    tx.execute("UPDATE decisions SET state='approved',revision=(SELECT revision FROM sessions WHERE id=?1) WHERE operation=?2",params![session,operation])?;
    Ok(())
}

pub(crate) fn apply(
    tx: &Transaction<'_>,
    command: &Command,
    models: &[ModelInfo],
) -> anyhow::Result<(SessionId, ThreadId, String)> {
    match command {
        Command::StopThread { thread, .. } => {
            let session = thread_session(tx, thread)?;
            let root: bool = tx.query_row(
                "SELECT parent_run IS NULL FROM threads WHERE id=?1",
                [thread],
                |r| r.get(0),
            )?;
            crate::lifecycle::stop_tree(tx, thread, root)?;
            Ok((
                session,
                thread.clone(),
                "subtree stop requested; retained outcomes preserved".into(),
            ))
        }
        Command::UpdateProfile {
            thread,
            profile,
            expected_revision,
            ..
        } => {
            validate_profile(profile, models)?;
            let session = thread_session(tx, thread)?;
            ensure!(tx.execute("UPDATE threads SET profile=?1,profile_revision=profile_revision+1 WHERE id=?2 AND profile_revision=?3",params![encode(profile)?,thread,expected_revision])? == 1,"profile changed; refresh before updating it");
            tx.execute(
                "UPDATE sessions SET revision=revision+1 WHERE id=?1",
                [&session],
            )?;
            invalidate_approvals(tx, &session)?;
            Ok((
                session,
                thread.clone(),
                "profile updated; in-flight model output fenced".into(),
            ))
        }
        Command::ClearContext {
            thread,
            expected_epoch,
            ..
        } => {
            let session = thread_session(tx, thread)?;
            let root: bool = tx.query_row(
                "SELECT parent_run IS NULL FROM threads WHERE id=?1",
                [thread],
                |r| r.get(0),
            )?;
            ensure!(root, "clear the session root, not an individual child");
            let epoch: i64 = tx.query_row(
                "SELECT epoch FROM contexts WHERE thread=?1",
                [thread],
                |r| r.get(0),
            )?;
            ensure!(
                epoch == *expected_epoch,
                "context epoch changed; refresh before clearing"
            );
            crate::lifecycle::stop_tree(tx, thread, false)?;
            tx.execute("UPDATE jobs SET state='stopping' WHERE session=?1 AND state IN ('queued','running')",[&session])?;
            tx.execute("UPDATE contexts SET epoch=epoch+1,revision=revision+1,checkpoint=NULL,compact_pending=0,newest_request='' WHERE thread IN (SELECT id FROM threads WHERE session=?1)",[&session])?;
            tx.execute(
                "DELETE FROM compactions WHERE thread IN (SELECT id FROM threads WHERE session=?1)",
                [&session],
            )?;
            tx.execute(
                "DELETE FROM working WHERE thread IN (SELECT id FROM threads WHERE session=?1)",
                [&session],
            )?;
            tx.execute("UPDATE runs SET state='stopping' WHERE thread IN (SELECT id FROM threads WHERE session=?1) AND state IN ('active','interrupted')",[&session])?;
            tx.execute("UPDATE submissions SET state='cancelled' WHERE thread IN (SELECT id FROM threads WHERE session=?1) AND state='queued'",[&session])?;
            tx.execute("UPDATE human_controls SET state='cancelled' WHERE operation IN (SELECT o.id FROM operations o JOIN threads t ON t.id=o.thread WHERE t.session=?1)",[&session])?;
            tx.execute(
                "DELETE FROM todos WHERE thread IN (SELECT id FROM threads WHERE session=?1)",
                [&session],
            )?;
            tx.execute(
                "UPDATE threads SET held=(parent_run IS NOT NULL) WHERE session=?1",
                [&session],
            )?;
            entry(
                tx,
                thread,
                "notice",
                "Working context cleared; transcript and uncertain effects remain inspectable.",
                EntryData::default(),
            )?;
            Ok((
                session,
                thread.clone(),
                "new context epoch committed".into(),
            ))
        }
        Command::Compact {
            thread,
            expected_revision,
            ..
        } => {
            let session = thread_session(tx, thread)?;
            ensure!(
                tx.execute(
                    "UPDATE contexts SET compact_pending=1 WHERE thread=?1 AND revision=?2",
                    params![thread, expected_revision]
                )? == 1,
                "context changed; refresh before compacting"
            );
            let reset = tx.execute("DELETE FROM compactions WHERE thread=?1 AND json_extract(state,'$.phase')='blocked'",[thread])?;
            if reset > 0 {
                let unresolved: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM operations WHERE thread=?1 AND state='unknown')",
                    [thread],
                    |row| row.get(0),
                )?;
                ensure!(
                    !unresolved,
                    "resolve uncertain operations before retrying compaction"
                );
                tx.execute("UPDATE runs SET state='active',retry_at=0 WHERE thread=?1 AND state='interrupted'",[thread])?;
                tx.execute("UPDATE threads SET held=0 WHERE id=?1", [thread])?;
            }
            let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE thread=?1 AND state IN ('active','interrupted','stopping'))",[thread],|r|r.get(0))?;
            if !busy {
                let unresolved: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM operations WHERE thread=?1 AND state='unknown')",
                    [thread],
                    |r| r.get(0),
                )?;
                ensure!(
                    !unresolved,
                    "resolve uncertain operations before compacting"
                );
                tx.execute(
                    "INSERT INTO runs(id,thread,state,compact_only) VALUES(?1,?2,'active',1)",
                    params![RunId::new(), thread],
                )?;
                tx.execute("UPDATE threads SET held=0 WHERE id=?1", [thread])?;
            }
            Ok((
                session,
                thread.clone(),
                "compaction queued for a complete tool-exchange boundary".into(),
            ))
        }
        Command::Steer { id, thread, text } => {
            ensure!(
                !text.trim().is_empty() && text.len() <= 65536,
                "steering must contain 1..65536 bytes"
            );
            let session = thread_session(tx, thread)?;
            submit(tx, id, thread, "steering", None, text)?;
            let detail = crate::jobs::promote_for_input(tx, thread)?
                .unwrap_or_else(|| "steering queued for the next model boundary".into());
            Ok((session, thread.clone(), detail))
        }
        Command::SetPermissions {
            session,
            permissions,
            expected_revision,
            ..
        } => {
            ensure!(
                permissions.rules.len() <= 1024 && permissions.grants.len() <= 1024,
                "too many permission records"
            );
            ensure!(tx.execute("UPDATE sessions SET permissions=?1,revision=revision+1 WHERE id=?2 AND revision=?3",params![encode(permissions)?,session,expected_revision])?==1,"permissions changed; refresh before updating");
            let raw: String =
                tx.query_row("SELECT policy FROM sessions WHERE id=?1", [session], |r| {
                    r.get(0)
                })?;
            let mut policy: Policy = decode(raw)?;
            policy.plan = permissions.mode == PermissionMode::Plan;
            tx.execute(
                "UPDATE sessions SET policy=?1 WHERE id=?2",
                params![encode(&policy)?, session],
            )?;
            invalidate_approvals(tx, session)?;
            let thread = ThreadId(tx.query_row(
                "SELECT id FROM threads WHERE session=?1 AND parent_run IS NULL",
                [session],
                |r| r.get(0),
            )?);
            Ok((
                session.clone(),
                thread,
                "permissions updated; previous approvals invalidated".into(),
            ))
        }
        Command::AnswerQuestion {
            operation,
            revision,
            answer,
            ..
        } => {
            ensure!(
                !answer.trim().is_empty() && answer.len() <= 65536,
                "answer must contain 1..65536 bytes"
            );
            crate::controls::answer(tx, operation, *revision, "question", answer, false)
        }
        Command::DecidePlan {
            operation,
            revision,
            approve,
            feedback,
            ..
        } => {
            ensure!(
                feedback.as_ref().is_none_or(|text| text.len() <= 65536),
                "feedback too large"
            );
            let text = if *approve {
                "Plan approved; implementation may proceed.".to_owned()
            } else {
                format!(
                    "Plan not approved. {}",
                    feedback
                        .as_deref()
                        .unwrap_or("Revise the proposal before implementation.")
                )
            };
            crate::controls::answer(tx, operation, *revision, "plan", &text, *approve)
        }
        _ => bail!("unsupported command"),
    }
}
