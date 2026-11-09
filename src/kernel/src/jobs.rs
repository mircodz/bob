use crate::model::*;
use crate::store::*;
use anyhow::{ensure, Context};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Shell,
    Task,
    Workflow,
    Turn,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Stopping,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: JobId,
    pub session: SessionId,
    pub thread: ThreadId,
    pub operation: Option<OperationId>,
    pub run: Option<RunId>,
    pub kind: JobKind,
    pub state: JobState,
    pub title: String,
    pub result: Option<ArtifactId>,
    pub started_at: i64,
    pub background: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuedInput {
    pub id: CommandId,
    pub thread: ThreadId,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueSnapshot {
    /// Changes snapshot revision, matching View.sequence.
    pub sequence: i64,
    pub pending: Vec<QueuedInput>,
    pub submissions: Vec<EntrySubmission>,
}

#[derive(Clone)]
pub struct ToolProgress {
    pub(crate) operation: OperationId,
    pub(crate) tx: mpsc::Sender<ProgressUpdate>,
}
pub(crate) struct ProgressUpdate {
    pub operation: OperationId,
    pub text: String,
    pub reply: oneshot::Sender<anyhow::Result<()>>,
}
impl ToolProgress {
    pub async fn append(&self, text: String) -> anyhow::Result<()> {
        ensure!(text.len() <= 16 * 1024, "progress chunk exceeds 16 KiB");
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ProgressUpdate {
                operation: self.operation.clone(),
                text,
                reply,
            })
            .await
            .map_err(|_| anyhow::anyhow!("job progress owner stopped"))?;
        rx.await.context("job progress owner stopped")?
    }
}

pub(crate) const SCHEMA: &str = "
CREATE TABLE jobs(id TEXT PRIMARY KEY, session TEXT NOT NULL REFERENCES sessions(id), thread TEXT NOT NULL REFERENCES threads(id), operation TEXT UNIQUE REFERENCES operations(id), run TEXT REFERENCES runs(id), kind TEXT NOT NULL, state TEXT NOT NULL, title TEXT NOT NULL, result TEXT REFERENCES artifacts(id), started_at INTEGER NOT NULL DEFAULT (unixepoch()), background INTEGER NOT NULL, epoch INTEGER NOT NULL, delivered INTEGER NOT NULL DEFAULT 0, spool_bytes INTEGER NOT NULL DEFAULT 0, truncated INTEGER NOT NULL DEFAULT 0);
CREATE INDEX jobs_session ON jobs(session,started_at);
CREATE INDEX jobs_run ON jobs(run);
CREATE TABLE job_spool(job TEXT NOT NULL REFERENCES jobs(id), ordinal INTEGER NOT NULL, text TEXT NOT NULL, PRIMARY KEY(job,ordinal));
";

pub(crate) fn list(db: &Connection, session: &SessionId) -> anyhow::Result<Vec<JobSummary>> {
    let mut stmt = db.prepare("SELECT id,thread,operation,run,kind,state,title,result,started_at,background FROM jobs WHERE session=?1 AND (kind!='shell' OR background=1) ORDER BY rowid DESC LIMIT 256")?;
    let rows = stmt.query_map([session], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, bool>(9)?,
        ))
    })?;
    rows.map(|row| {
        let (id, thread, operation, run, kind, state, title, result, started_at, background) = row?;
        Ok(JobSummary {
            id: JobId(id),
            session: session.clone(),
            thread: ThreadId(thread),
            operation: operation.map(OperationId),
            run: run.map(RunId),
            kind: serde_json::from_value(Value::String(kind))?,
            state: serde_json::from_value(Value::String(state))?,
            title,
            result: result.map(ArtifactId),
            started_at,
            background,
        })
    })
    .collect()
}
pub(crate) fn queue(db: &Connection, thread: &ThreadId) -> anyhow::Result<Vec<QueuedInput>> {
    let mut stmt=db.prepare("SELECT id,text FROM submissions WHERE thread=?1 AND state='queued' AND source='human' ORDER BY rowid LIMIT 128")?;
    let rows = stmt.query_map([thread], |r| {
        Ok(QueuedInput {
            id: CommandId(r.get(0)?),
            thread: thread.clone(),
            text: r.get(1)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}
pub(crate) fn submission_state(state: &str) -> rusqlite::Result<SubmissionState> {
    match state {
        "queued" => Ok(SubmissionState::Pending),
        "in_use" | "settled" => Ok(SubmissionState::Consumed),
        "recalled" => Ok(SubmissionState::Recalled),
        "cancelled" => Ok(SubmissionState::Cancelled),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

pub(crate) fn queue_snapshot(
    db: &Connection,
    thread: &ThreadId,
    tracked: &[CommandId],
) -> anyhow::Result<QueueSnapshot> {
    ensure!(tracked.len() <= 1024, "too many tracked submissions");
    let snapshot = db.unchecked_transaction()?;
    let sequence =
        snapshot.query_row("SELECT COALESCE(MAX(seq),0) FROM changes", [], |r| r.get(0))?;
    let pending = queue(&snapshot, thread)?;
    let mut ids: std::collections::HashSet<_> = tracked.iter().collect();
    ids.extend(pending.iter().map(|input| &input.id));
    let mut stmt = snapshot.prepare(
        "SELECT state,rowid FROM submissions WHERE id=?1 AND thread=?2 AND source='human'",
    )?;
    let mut submissions = Vec::with_capacity(ids.len());
    for id in ids {
        let state: Option<(String, i64)> = stmt
            .query_row(params![id, thread], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((state, ordinal)) = state {
            submissions.push(EntrySubmission {
                id: id.clone(),
                state: submission_state(&state)?,
                ordinal,
                sequence,
            });
        }
    }
    Ok(QueueSnapshot {
        sequence,
        pending,
        submissions,
    })
}

pub(crate) fn tool_input(db: &Connection, operation: &OperationId) -> anyhow::Result<Value> {
    let size: usize = db.query_row(
        "SELECT length(CAST(input AS BLOB)) FROM operations WHERE id=?1 AND kind='tool'",
        [operation],
        |r| r.get(0),
    )?;
    ensure!(
        size <= 64 * 1024,
        "tool input exceeds 64 KiB; use the raw artifact inspector"
    );
    decode(db.query_row(
        "SELECT input FROM operations WHERE id=?1",
        [operation],
        |r| r.get(0),
    )?)
}
pub(crate) fn command(
    tx: &Transaction<'_>,
    command: &Command,
) -> anyhow::Result<(SessionId, ThreadId, String)> {
    match command {
        Command::RecallSubmission { submission, .. } => {
            let thread = ThreadId(tx.query_row(
                "SELECT thread FROM submissions WHERE id=?1 AND state='queued' AND source='human'",
                [submission],
                |r| r.get(0),
            )?);
            tx.execute(
                "UPDATE submissions SET state='recalled' WHERE id=?1",
                [submission],
            )?;
            Ok((
                thread_session(tx, &thread)?,
                thread,
                "queued input recalled".into(),
            ))
        }
        Command::Background { thread, .. } => {
            let session = thread_session(tx, thread)?;
            let run = RunId(tx.query_row(
                "SELECT id FROM runs WHERE thread=?1 AND state='active'",
                [thread],
                |r| r.get(0),
            )?);
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM jobs WHERE run=?1 AND kind='turn'",
                    [&run],
                    |r| r.get(0),
                )
                .optional()?;
            let id = existing.map(JobId).unwrap_or_default();
            tx.execute("INSERT OR IGNORE INTO jobs(id,session,thread,run,kind,state,title,background,epoch) SELECT ?1,?2,?3,?4,'turn','running','Background turn',1,epoch FROM contexts WHERE thread=?3",params![id,session,thread,run])?;
            Ok((
                session,
                thread.clone(),
                format!("Tracking turn as job {id}; queued input still waits"),
            ))
        }
        Command::StopJob { job, .. } => {
            let (session, thread): (String, String) = tx.query_row(
                "SELECT session,thread FROM jobs WHERE id=?1 AND (kind!='shell' OR background=1)",
                [job],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            stop(tx, job)?;
            Ok((
                SessionId(session),
                ThreadId(thread),
                "job stop requested".into(),
            ))
        }
        _ => unreachable!(),
    }
}
pub(crate) fn stop(tx: &Transaction<'_>, job: &JobId) -> anyhow::Result<()> {
    let (kind, run): (String, Option<String>) =
        tx.query_row("SELECT kind,run FROM jobs WHERE id=?1", [job], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    tx.execute(
        "UPDATE jobs SET state='stopping' WHERE id=?1 AND state IN ('queued','running')",
        [job],
    )?;
    if kind == "turn" {
        if let Some(run) = run {
            crate::delegation::stop_run(tx, &RunId(run))?;
        }
    }
    change(tx, None, "job_stopping", &job.0)?;
    Ok(())
}
pub(crate) fn append_data(db: &mut Connection, update: &ProgressUpdate) -> anyhow::Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let job: Option<(String, usize, bool)> = tx
        .query_row(
            "SELECT id,spool_bytes,truncated FROM jobs WHERE operation=?1 AND state IN ('running','stopping')",
            [&update.operation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((id, bytes, truncated)) = job {
        if !truncated && !update.text.is_empty() {
            let mut text = update.text.clone();
            let remaining = (MAX_ARTIFACT - 128).saturating_sub(bytes);
            let clipped = text.len() > remaining;
            if clipped {
                let mut end = remaining;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str("\n[Partial output retention limit reached; final result remains authoritative.]\n");
            }
            tx.execute("INSERT INTO job_spool(job,ordinal,text) VALUES(?1,?2,?3) ON CONFLICT(job,ordinal) DO UPDATE SET text=job_spool.text||excluded.text",params![id,bytes/(16*1024),text])?;
            tx.execute(
                "UPDATE jobs SET spool_bytes=spool_bytes+?2,truncated=?3 WHERE id=?1",
                params![id, text.len(), clipped],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn append(db: &mut Connection, update: ProgressUpdate) -> anyhow::Result<()> {
    let result = append_data(db, &update);
    let failed = result
        .as_ref()
        .err()
        .map(|e: &anyhow::Error| anyhow::anyhow!("{e:#}"));
    let _ = update.reply.send(result);
    if let Some(error) = failed {
        return Err(error);
    }
    Ok(())
}

pub(crate) fn recover(tx: &Transaction<'_>, session: &SessionId) -> anyhow::Result<()> {
    tx.execute("DELETE FROM job_spool WHERE job IN (SELECT id FROM jobs WHERE session=?1 AND kind='shell' AND background=0)", [session])?;
    tx.execute(
        "DELETE FROM jobs WHERE session=?1 AND kind='shell' AND background=0",
        [session],
    )?;
    let background: Vec<String> = {
        let mut stmt = tx.prepare("SELECT operation FROM jobs WHERE session=?1 AND kind='shell' AND background=1 AND state IN ('queued','running','stopping')")?;
        let rows = stmt.query_map([session], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for operation in background {
        hold_background_unknown(tx, &OperationId(operation))?;
    }
    let jobs: Vec<(String, String)> = {
        let mut stmt = tx
            .prepare("SELECT id,thread FROM jobs WHERE session=?1 AND state IN ('running','stopping','queued')")?;
        let rows = stmt.query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (id, thread) in jobs {
        tx.execute("UPDATE jobs SET state='unknown' WHERE id=?1", [&id])?;
        entry(
            tx,
            &ThreadId(thread.clone()),
            "coordination",
            &format!("Job {id} interrupted; outcome unknown. Inspect effects before any retry."),
            EntryData::default(),
        )?;
        change(tx, Some(&ThreadId(thread)), "job_recovered", &id)?;
    }
    Ok(())
}
pub(crate) fn settle_turn(
    tx: &Transaction<'_>,
    run: &RunId,
    state: &str,
    text: &str,
) -> anyhow::Result<()> {
    let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE run=?1 AND kind='turn' AND state IN ('running','stopping'))",[run],|r|r.get(0))?;
    if exists {
        let output = artifact(tx, &bounded(text.to_owned()))?;
        tx.execute("UPDATE jobs SET state=?2,result=?3,delivered=1 WHERE run=?1 AND kind='turn' AND state IN ('running','stopping')",params![run,state,output])?;
    }
    Ok(())
}
pub(crate) fn start_shell(tx: &Transaction<'_>, tool: &PreparedTool) -> anyhow::Result<JobId> {
    let inv = &tool.invocation;
    let session = thread_session(tx, &inv.thread_id)?;
    let active: usize = tx.query_row(
        "SELECT COUNT(*) FROM jobs WHERE session=?1 AND (kind!='shell' OR background=1) AND state IN ('running','queued','stopping')",
        [&session],
        |r| r.get(0),
    )?;
    ensure!(
        active < 64,
        "active job limit reached; wait for work to settle"
    );
    let id = JobId::new();
    tx.execute("INSERT INTO jobs(id,session,thread,operation,run,kind,state,title,background,epoch) SELECT ?1,?2,?3,?4,?5,'shell','running',?6,1,epoch FROM contexts WHERE thread=?3",params![id,session,inv.thread_id,inv.operation_id,inv.run_id,preview(inv.input["command"].as_str().unwrap_or("Shell command"))])?;
    finish_tool_entry(tx,&inv.operation_id,&serde_json::json!({"job_id":id,"state":"running","message":"Kernel-owned shell started; collect with job_output."}).to_string(),false)?;
    tx.execute(
        "UPDATE operations SET state='done' WHERE id=?1",
        [&inv.operation_id],
    )?;
    change(tx, Some(&inv.thread_id), "job_started", &id.0)?;
    Ok(id)
}
pub(crate) fn track_foreground(tx: &Transaction<'_>, tool: &PreparedTool) -> anyhow::Result<()> {
    let inv = &tool.invocation;
    tx.execute("INSERT INTO jobs(id,session,thread,operation,run,kind,state,title,background,epoch) SELECT ?1,t.session,t.id,?2,?3,'shell','running',?4,0,c.epoch FROM threads t JOIN contexts c ON c.thread=t.id WHERE t.id=?5",params![JobId::new(),inv.operation_id,inv.run_id,preview(inv.input["command"].as_str().unwrap_or("Shell command")),inv.thread_id])?;
    Ok(())
}

pub(crate) fn clear_foreground(
    tx: &Transaction<'_>,
    operation: &OperationId,
) -> anyhow::Result<()> {
    tx.execute("DELETE FROM job_spool WHERE job IN (SELECT id FROM jobs WHERE operation=?1 AND kind='shell' AND background=0)", [operation])?;
    tx.execute(
        "DELETE FROM jobs WHERE operation=?1 AND kind='shell' AND background=0",
        [operation],
    )?;
    Ok(())
}

pub(crate) fn promote_for_input(
    tx: &Transaction<'_>,
    thread: &ThreadId,
) -> anyhow::Result<Option<String>> {
    let candidate: Option<(String, String, String, bool)> = tx.query_row(
        "SELECT j.id,o.id,o.run,j.background FROM jobs j JOIN operations o ON o.id=j.operation JOIN runs r ON r.id=o.run JOIN threads t ON t.id=o.thread JOIN contexts c ON c.thread=t.id WHERE t.id=?1 AND t.held=0 AND r.state='active' AND r.epoch=c.epoch AND o.epoch=c.epoch AND o.profile_revision=t.profile_revision AND j.epoch=c.epoch AND j.kind='shell' AND j.state='running' AND o.kind='tool' AND o.name='bash' AND COALESCE(json_extract(o.input,'$.run_in_background'),0)!=1 AND ((j.background=0 AND o.state='started') OR (j.background=1 AND o.state='done')) AND NOT EXISTS(SELECT 1 FROM operations u WHERE u.thread=t.id AND u.state='unknown') AND (j.background=1 OR NOT EXISTS(SELECT 1 FROM operations p WHERE p.run=r.id AND p.id!=o.id AND p.state IN ('preparing','started'))) ORDER BY j.background,o.rowid LIMIT 1",
        [thread], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).optional()?;
    let Some((id, operation, run, background)) = candidate else {
        return Ok(None);
    };
    if !background {
        let active: usize = tx.query_row("SELECT COUNT(*) FROM jobs WHERE session=(SELECT session FROM threads WHERE id=?1) AND (kind!='shell' OR background=1) AND state IN ('running','queued','stopping')", [thread], |r| r.get(0))?;
        if active >= 64 {
            return Ok(Some("Input durably queued; active job limit reached, so the foreground shell was not backgrounded.".into()));
        }
        tx.execute("UPDATE jobs SET background=1 WHERE id=?1", [&id])?;
        let operation = OperationId(operation);
        finish_tool_entry(tx, &operation, &serde_json::json!({"job_id":id,"state":"running","message":"The same foreground shell continues in the background with its original timeout. Collect with job_output; do not restart it."}).to_string(), false)?;
        tx.execute(
            "UPDATE operations SET state='done' WHERE id=?1",
            [&operation],
        )?;
        let unstarted: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM operations WHERE run=?1 AND kind='tool' AND state IN ('pending','ready','approval') ORDER BY rowid")?;
            let rows = stmt.query_map([&run], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for pending in unstarted {
            let pending = OperationId(pending);
            finish_tool_entry(
                tx,
                &pending,
                "Not executed; new human input needs consideration before further tool calls.",
                true,
            )?;
            tx.execute(
                "UPDATE decisions SET state='stale' WHERE operation=?1",
                [&pending],
            )?;
            tx.execute(
                "UPDATE operations SET state='cancelled',prepared=NULL WHERE id=?1",
                [&pending],
            )?;
            change(tx, Some(thread), "operation_updated", &pending.0)?;
        }
        entry(tx, thread, "notice", &format!("Shell continues in background as job {id}; original timeout unchanged. Stop with /jobs stop {id}."), EntryData::default())?;
        change(tx, Some(thread), "job_started", &id)?;
    }
    tx.execute("UPDATE submissions SET owner_run=?2 WHERE thread=?1 AND state='queued' AND source='human' AND owner_run IS NULL", params![thread,run])?;
    Ok(Some(format!(
        "Input queued for the next model boundary; shell continues as job {id}."
    )))
}

fn hold_background_unknown(tx: &Transaction<'_>, operation: &OperationId) -> anyhow::Result<()> {
    let current: Option<String> = tx.query_row("SELECT o.thread FROM operations o JOIN jobs j ON j.operation=o.id JOIN contexts c ON c.thread=o.thread WHERE o.id=?1 AND j.kind='shell' AND j.background=1 AND o.epoch=c.epoch", [operation], |r| r.get(0)).optional()?;
    let background: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs j JOIN operations o ON o.id=j.operation WHERE j.operation=?1 AND j.kind='shell' AND j.background=1 AND COALESCE(json_extract(o.input,'$.run_in_background'),0)!=1)", [operation], |r| r.get(0))?;
    if !background {
        return Ok(());
    }
    tx.execute(
        "UPDATE operations SET state='unknown' WHERE id=?1",
        [operation],
    )?;
    if let Some(thread) = current {
        tx.execute("UPDATE threads SET held=1 WHERE id=?1", [&thread])?;
        let runs: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM runs WHERE thread=?1 AND state IN ('active','interrupted')",
            )?;
            let rows = stmt.query_map([&thread], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for run in runs {
            tx.execute("UPDATE runs SET state='interrupted' WHERE id=?1", [&run])?;
            crate::lifecycle::stop_owned(tx, &RunId(run))?;
        }
        entry(tx, &ThreadId(thread), "recovery", &format!("Background shell {operation} has an unknown outcome. Inspect its job output and effects, resolve the original operation, then explicitly resume; it was not replayed."), EntryData::default())?;
    }
    Ok(())
}

pub(crate) fn bounded(mut text: String) -> String {
    if text.len() > MAX_ARTIFACT {
        let suffix = "\n[Output retention limit reached; do not repeat uncertain effects.]";
        let mut end = MAX_ARTIFACT - suffix.len();
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(suffix);
    }
    text
}
pub(crate) fn finish_shell(
    tx: &Transaction<'_>,
    operation: &OperationId,
    result: anyhow::Result<(ToolOutcome, Option<String>)>,
) -> anyhow::Result<()> {
    let (id, thread, state, epoch): (String, String, String, i64) = tx.query_row(
        "SELECT id,thread,state,epoch FROM jobs WHERE operation=?1",
        [operation],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if !matches!(state.as_str(), "running" | "stopping") {
        return Ok(());
    }
    let (state, text) = match result {
        Ok((result, display)) => (
            if result.interrupted {
                "unknown"
            } else if state == "stopping" {
                "cancelled"
            } else if result.is_error {
                "failed"
            } else {
                "completed"
            },
            display.unwrap_or(result.text),
        ),
        Err(error) => {
            let promoted: bool = tx.query_row("SELECT COALESCE(json_extract(input,'$.run_in_background'),0)!=1 FROM operations WHERE id=?1", [operation], |r| r.get(0))?;
            let partial = if promoted {
                let mut stmt =
                    tx.prepare("SELECT text FROM job_spool WHERE job=?1 ORDER BY ordinal")?;
                let parts = stmt.query_map([&id], |r| r.get::<_, String>(0))?;
                parts.collect::<Result<Vec<_>, _>>()?.concat()
            } else {
                String::new()
            };
            let failure =
                format!("Unknown shell outcome: {error:#}. Inspect effects before retrying.");
            (
                "unknown",
                if partial.is_empty() {
                    failure
                } else {
                    format!("{partial}\n{failure}")
                },
            )
        }
    };
    if state == "unknown" {
        hold_background_unknown(tx, operation)?;
    }
    let output = artifact(tx, &bounded(text))?;
    tx.execute(
        "UPDATE jobs SET state=?2,result=?3,delivered=1 WHERE id=?1",
        params![id, state, output],
    )?;
    tx.execute("DELETE FROM job_spool WHERE job=?1", [&id])?;
    let current: i64 = tx.query_row(
        "SELECT epoch FROM contexts WHERE thread=?1",
        [&thread],
        |r| r.get(0),
    )?;
    if current == epoch {
        let notice = CommandId::new();
        submit(
            tx,
            &notice,
            &ThreadId(thread.clone()),
            "coordination",
            None,
            &format!("Shell job {id} {state}. Use job_output to inspect its retained result."),
        )?;
        // An idle agent would otherwise never see this until the next human message.
        tx.execute(
            "UPDATE submissions SET state='queued' WHERE id=?1 AND state='retained'",
            [&notice],
        )?;
    }
    crate::effect_claims::release_settled(tx, operation)?;
    change(tx, Some(&ThreadId(thread)), "job_settled", &id)?;
    Ok(())
}
pub(crate) fn tools() -> Vec<ToolDescriptor> {
    use serde_json::json;
    [
        ("job_status","Inspect kernel-owned jobs; omit id to list recent jobs.",json!({"id":{"type":"string"}})),
        ("job_output","Read retained partial or final output. Final output replaces partial output; byte offsets are UTF-8 boundaries.",json!({"id":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":24000}})),
    ].into_iter().map(|(name,description,properties)|ToolDescriptor {spec:bob_types::ToolSpec{name:name.into(),description:description.into(),input_schema:json!({"type":"object","properties":properties,"additionalProperties":false})},effect:EffectClass::Read}).collect()
}
pub(crate) fn execute(
    db: &Connection,
    thread: &ThreadId,
    name: &str,
    input: &Value,
) -> anyhow::Result<String> {
    let session = thread_session(db, thread)?;
    if let Some(id) = input["id"].as_str() {
        let same: bool = db.query_row(
            "SELECT session=?2 FROM jobs WHERE id=?1 AND (kind!='shell' OR background=1)",
            params![id, session],
            |r| r.get(0),
        )?;
        ensure!(same, "job belongs to another session");
        if name == "job_output" {
            return encode(&page(
                db,
                &JobId(id.into()),
                input["offset"].as_u64().unwrap_or(0) as usize,
                input["limit"].as_u64().unwrap_or(24000) as usize,
            )?);
        }
        let job = list(db, &session)?
            .into_iter()
            .find(|j| j.id.0 == id)
            .context("job outside recent status window; use job_output")?;
        encode(&job)
    } else {
        ensure!(name == "job_status", "job_output requires id");
        encode(&list(db, &session)?)
    }
}

pub(crate) fn page(
    db: &Connection,
    job: &JobId,
    offset: usize,
    limit: usize,
) -> anyhow::Result<TextPage> {
    ensure!(
        limit > 0 && limit <= 24_000,
        "job page limit must be 1..24000 bytes"
    );
    let (result, total): (Option<String>, usize) = db.query_row(
        "SELECT result,spool_bytes FROM jobs WHERE id=?1 AND (kind!='shell' OR background=1)",
        [job],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let text = if let Some(result) = result {
        full_artifact(db, &ArtifactId(result))?
    } else {
        let mut stmt = db.prepare("SELECT text FROM job_spool WHERE job=?1 ORDER BY ordinal")?;
        let parts = stmt
            .query_map([job], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut text = String::with_capacity(total);
        for part in parts {
            text.push_str(&part);
        }
        text
    };
    ensure!(
        offset <= text.len() && text.is_char_boundary(offset),
        "invalid output byte offset"
    );
    let mut end = (offset + limit).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset && end < text.len() {
        end += text[offset..].chars().next().unwrap().len_utf8();
    }
    Ok(TextPage {
        text: text[offset..end].into(),
        next_offset: (end < text.len()).then_some(end),
        total_bytes: text.len(),
    })
}
