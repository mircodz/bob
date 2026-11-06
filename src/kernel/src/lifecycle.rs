use crate::model::*;
use crate::store::*;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

pub(crate) fn owner_open(db: &Connection, run: &RunId) -> anyhow::Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM runs r JOIN contexts c ON c.thread=r.thread WHERE r.id=?1 AND r.state IN ('active','interrupted') AND r.epoch=c.epoch)", [run], |r| r.get(0))?)
}

pub(crate) fn pending(db: &Connection, run: &RunId) -> anyhow::Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM threads t LEFT JOIN agents a ON a.thread=t.id WHERE t.parent_run=?1 AND (a.state IN ('queued','running') OR EXISTS(SELECT 1 FROM runs r WHERE r.thread=t.id AND r.state IN ('active','interrupted','stopping')))) OR EXISTS(SELECT 1 FROM jobs WHERE run=?1 AND kind IN ('task','workflow') AND state IN ('queued','running','stopping'))", [run], |r| r.get(0))?)
}

pub(crate) fn blocked(db: &Connection, run: &RunId) -> anyhow::Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM threads t JOIN agents a ON a.thread=t.id WHERE t.parent_run=?1 AND NOT EXISTS(SELECT 1 FROM runs stopping WHERE stopping.thread=t.id AND stopping.state='stopping') AND (a.state='unknown' OR (t.held=1 AND (a.state IN ('queued','running') OR EXISTS(SELECT 1 FROM runs r WHERE r.thread=t.id AND r.state='interrupted'))))) OR EXISTS(SELECT 1 FROM jobs WHERE run=?1 AND kind IN ('task','workflow') AND state='unknown')", [run], |r| r.get(0))?)
}

pub(crate) fn hold_join(
    tx: &Transaction<'_>,
    run: &RunId,
    thread: &ThreadId,
) -> anyhow::Result<()> {
    tx.execute(
        "UPDATE runs SET state='interrupted' WHERE id=?1 AND state='active'",
        [run],
    )?;
    tx.execute("UPDATE threads SET held=1 WHERE id=?1", [thread])?;
    entry(tx, thread, "recovery", "Owned agent work requires recovery. Inspect held or unknown descendants before explicitly resuming this join; no work was replayed.", EntryData::default())?;
    change(tx, Some(thread), "join_held", &run.0)?;
    Ok(())
}

pub(crate) fn actionable(db: &Connection, run: &RunId) -> anyhow::Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM submissions s JOIN runs r ON r.thread=s.thread WHERE r.id=?1 AND s.state='queued' AND ((s.source IN ('coordination','recovery','steering') AND (s.owner_run IS NULL OR s.owner_run=r.id)) OR (s.source='human' AND s.owner_run=r.id)))", [run], |r| r.get(0))?)
}

pub(crate) fn guidance(db: &Connection, run: &RunId) -> anyhow::Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM submissions s JOIN runs r ON r.thread=s.thread WHERE r.id=?1 AND s.state='queued' AND ((s.source IN ('coordination','recovery','steering') AND (s.owner_run IS NULL OR s.owner_run=r.id)) OR (s.source='human' AND s.owner_run=r.id)) AND NOT EXISTS(SELECT 1 FROM coordination c WHERE c.submission=s.id AND c.kind='completion'))", [run], |r| r.get(0))?)
}

pub(crate) fn queue_completion(
    tx: &Transaction<'_>,
    owner: &RunId,
    sender: Option<&ThreadId>,
    text: &str,
) -> anyhow::Result<Option<CommandId>> {
    if !owner_open(tx, owner)? {
        return Ok(None);
    }
    let parent =
        ThreadId(tx.query_row("SELECT thread FROM runs WHERE id=?1", [owner], |r| r.get(0))?);
    let submission = CommandId::new();
    submit(tx, &submission, &parent, "coordination", sender, text)?;
    tx.execute("INSERT INTO coordination(id,source,destination,submission,kind,text) VALUES(?1,?2,?3,?1,'completion',?4)", params![submission,sender.unwrap_or(&parent),parent,text])?;
    Ok(Some(submission))
}

pub(crate) fn publish(tx: &Transaction<'_>, thread: &ThreadId) -> anyhow::Result<()> {
    let row: Option<(String, String, Option<String>, bool, String)> = tx.query_row(
        "SELECT t.parent_run,a.state,a.outcome,a.reported,a.role FROM agents a JOIN threads t ON t.id=a.thread WHERE a.thread=?1 AND a.outcome IS NOT NULL AND a.state NOT IN ('queued','running') AND NOT EXISTS(SELECT 1 FROM runs r WHERE r.thread=a.thread AND r.state IN ('active','interrupted','stopping'))",
        [thread], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    ).optional()?;
    let Some((owner, state, outcome, reported, role)) = row else {
        return Ok(());
    };
    if reported {
        return Ok(());
    }
    let owner = RunId(owner);
    tx.execute("UPDATE agents SET reported=1 WHERE thread=?1", [thread])?;
    if matches!(role.as_str(), "task" | "explore" | "workflow") || !owner_open(tx, &owner)? {
        return Ok(());
    }
    let parent = ThreadId(
        tx.query_row("SELECT thread FROM runs WHERE id=?1", [&owner], |r| {
            r.get(0)
        })?,
    );
    let output = outcome
        .as_ref()
        .map(|id| artifact_page(tx, &ArtifactId(id.clone()), 0, 16 * 1024))
        .transpose()?
        .unwrap_or_else(|| "No retained output.".into());
    let text = format!("Agent {thread} {state}: {output}\nRetained report: read_history with view=artifact, thread_id={thread}, artifact_id={}; use offset pages if needed.", outcome.as_deref().unwrap_or("none"));
    let submission = queue_completion(tx, &owner, Some(thread), &text)?
        .expect("owner checked in this transaction");
    entry(
        tx,
        thread,
        "coordination",
        &format!("Report queued for {parent}: {state}"),
        EntryData {
            submission: Some(&submission),
            ..Default::default()
        },
    )?;
    Ok(())
}

pub(crate) fn collect(tx: &Transaction<'_>, run: &RunId) -> anyhow::Result<()> {
    let children: Vec<String> = {
        let mut stmt = tx.prepare("SELECT a.thread FROM agents a JOIN threads t ON t.id=a.thread WHERE t.parent_run=?1 AND a.reported=0 AND a.state NOT IN ('queued','running')")?;
        let rows = stmt.query_map([run], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for child in children {
        publish(tx, &ThreadId(child))?;
    }
    Ok(())
}

pub(crate) fn stop_tree(
    tx: &Transaction<'_>,
    root: &ThreadId,
    preserve_root_shell: bool,
) -> anyhow::Result<()> {
    let threads = crate::coordination::subtree(tx, root)?;
    for id in &threads {
        tx.execute("UPDATE threads SET held=1 WHERE id=?1", [id])?;
        tx.execute("UPDATE runs SET state='stopping' WHERE thread=?1 AND state IN ('active','interrupted')", [id])?;
        tx.execute("UPDATE submissions SET state='cancelled' WHERE thread=?1 AND state='queued' AND source!='human'", [id])?;
        tx.execute("UPDATE jobs SET state='stopping' WHERE thread=?1 AND (kind!='shell' OR background=0 OR ?2=0) AND state IN ('queued','running')", params![id, preserve_root_shell && id == root])?;
        tx.execute("UPDATE human_controls SET state='cancelled' WHERE operation IN (SELECT id FROM operations WHERE thread=?1)", [id])?;
    }
    for thread in threads {
        let unstarted: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM agents a WHERE a.thread=?1 AND a.state IN ('queued','running','unknown') AND NOT EXISTS(SELECT 1 FROM runs r WHERE r.thread=a.thread AND r.state IN ('active','interrupted','stopping')))", [&thread], |r| r.get(0))?;
        if unstarted {
            crate::delegation::settle(
                tx,
                &thread,
                "cancelled",
                "Stopped before execution; no work was replayed.",
            )?;
            publish(tx, &thread)?;
        }
        change(tx, Some(&thread), "agent_stopping", &thread.0)?;
    }
    Ok(())
}

pub(crate) fn stop_owned(tx: &Transaction<'_>, run: &RunId) -> anyhow::Result<()> {
    tx.execute("UPDATE jobs SET state='stopping' WHERE run=?1 AND kind IN ('task','workflow') AND state IN ('queued','running')", [run])?;
    let children: Vec<String> = {
        let mut stmt = tx.prepare("SELECT id FROM threads WHERE parent_run=?1")?;
        let rows = stmt.query_map([run], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for child in children {
        stop_tree(tx, &ThreadId(child), false)?;
    }
    Ok(())
}

/// A human message explicitly continues a held thread; uncertain tool results already warn the model.
pub(crate) fn continue_for_input(tx: &Transaction<'_>, thread: &ThreadId) -> anyhow::Result<()> {
    let (held, active, interrupted): (bool, bool, Option<String>) = tx.query_row(
        "SELECT held,EXISTS(SELECT 1 FROM runs WHERE thread=?1 AND state='active'),(SELECT id FROM runs WHERE thread=?1 AND state='interrupted') FROM threads WHERE id=?1",
        [thread],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if active {
        return Ok(());
    }
    let unknown: Vec<String> = {
        let mut stmt =
            tx.prepare("SELECT id FROM operations WHERE thread=?1 AND state='unknown'")?;
        let rows = stmt.query_map([thread], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    if !held && interrupted.is_none() && unknown.is_empty() {
        return Ok(());
    }
    // Stopping cancels queued steering; keep the user's words as ordinary queued input instead.
    tx.execute(
        "UPDATE submissions SET source='human' WHERE thread=?1 AND state='queued' AND source='steering'",
        [thread],
    )?;
    for operation in unknown {
        let operation = OperationId(operation);
        tx.execute(
            "UPDATE operations SET state='resolved' WHERE id=?1",
            [&operation],
        )?;
        tx.execute(
            "UPDATE jobs SET state='cancelled' WHERE operation=?1 AND state='unknown'",
            [&operation],
        )?;
        crate::effect_claims::release_settled(tx, &operation)?;
        change(tx, Some(thread), "operation_updated", &operation.0)?;
    }
    if let Some(run) = interrupted {
        crate::delegation::stop_run(tx, &RunId(run))?;
    }
    tx.execute("UPDATE threads SET held=0 WHERE id=?1", [thread])?;
    Ok(())
}

pub(crate) fn fail(tx: &Transaction<'_>, run: &RunId, text: &str) -> anyhow::Result<()> {
    let thread =
        ThreadId(tx.query_row("SELECT thread FROM runs WHERE id=?1", [run], |r| r.get(0))?);
    let root: bool = tx.query_row(
        "SELECT parent_run IS NULL FROM threads WHERE id=?1",
        [&thread],
        |r| r.get(0),
    )?;
    stop_tree(tx, &thread, root)?;
    tx.execute(
        "UPDATE runs SET failure=?2,retry_at=0 WHERE id=?1 AND state='stopping'",
        params![run, text],
    )?;
    Ok(())
}
