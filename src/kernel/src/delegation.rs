use crate::model::*;
use crate::store::*;
use anyhow::{ensure, Context};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};

pub(crate) const SCHEMA:&str="
CREATE TABLE agents(thread TEXT PRIMARY KEY REFERENCES threads(id), role TEXT NOT NULL, blocking INTEGER NOT NULL, operation TEXT REFERENCES operations(id), outcome TEXT REFERENCES artifacts(id), state TEXT NOT NULL DEFAULT 'queued', output_schema TEXT, reported INTEGER NOT NULL DEFAULT 0, subagent_type TEXT);
CREATE INDEX agents_operation ON agents(operation);
CREATE TABLE task_groups(operation TEXT PRIMARY KEY REFERENCES operations(id), job TEXT REFERENCES jobs(id), state TEXT NOT NULL DEFAULT 'running');
CREATE TABLE coordination(id TEXT PRIMARY KEY, source TEXT NOT NULL REFERENCES threads(id), destination TEXT NOT NULL REFERENCES threads(id), submission TEXT REFERENCES submissions(id), operation TEXT REFERENCES operations(id), kind TEXT NOT NULL, text TEXT NOT NULL);
";

pub(crate) fn role(db: &Connection, thread: &ThreadId) -> anyhow::Result<String> {
    Ok(db
        .query_row("SELECT role FROM agents WHERE thread=?1", [thread], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or_else(|| "root".into()))
}
pub(crate) fn allowed(role: &str, name: &str) -> bool {
    !(role != "root" && matches!(name, "enter_plan" | "exit_plan")
        || matches!(role, "task" | "explore" | "workflow")
            && matches!(
                name,
                "task" | "explore" | "spawn_agent" | "launch_agent" | "workflow"
            )
        || role == "explore"
            && matches!(
                name,
                "send_message"
                    | "wait_message"
                    | "stop_agent"
                    | "ask_user"
                    | "enter_plan"
                    | "exit_plan"
                    | "todo_write"
                    | "bash"
                    | "write_file"
                    | "edit_file"
                    | "multi_edit"
                    | "rename_symbol"
                    | "code_action"
            ))
}
pub(crate) fn stop_run(tx: &Transaction<'_>, run: &RunId) -> anyhow::Result<()> {
    let thread =
        ThreadId(tx.query_row("SELECT thread FROM runs WHERE id=?1", [run], |r| r.get(0))?);
    crate::lifecycle::stop_tree(tx, &thread, true)
}
pub(crate) fn stop_child(
    tx: &Transaction<'_>,
    source: &ThreadId,
    target: &ThreadId,
) -> anyhow::Result<()> {
    ensure!(
        crate::coordination::is_descendant(tx, source, target)?,
        "can only stop descendants"
    );
    crate::lifecycle::stop_tree(tx, target, false)?;
    change(tx, Some(target), "agent_stopping", &target.0)?;
    Ok(())
}
pub(crate) struct Child<'a> {
    pub name: &'a str,
    pub prompt: &'a str,
    pub role: &'a str,
    pub subagent_type: Option<&'a str>,
    pub model: Option<&'a str>,
    pub read_only: bool,
    pub blocking: bool,
    pub schema: Option<&'a Value>,
}
pub(crate) fn launch(
    tx: &Transaction<'_>,
    operation: &OperationId,
    run: &RunId,
    parent: &ThreadId,
    base: &Profile,
    models: &[ModelInfo],
    child: Child<'_>,
) -> anyhow::Result<ThreadId> {
    let session = thread_session(tx, parent)?;
    let count:usize=tx.query_row("SELECT COUNT(*) FROM threads t WHERE t.session=?1 AND t.held=0 AND (EXISTS(SELECT 1 FROM runs r WHERE r.thread=t.id AND r.state IN ('active','interrupted','stopping')) OR EXISTS(SELECT 1 FROM submissions s WHERE s.thread=t.id AND s.state='queued'))",[&session],|r|r.get(0))?;
    ensure!(count < 128, "active agent concurrency limit reached");
    let depth:usize=tx.query_row("WITH RECURSIVE ancestors(id,parent_run) AS (SELECT id,parent_run FROM threads WHERE id=?1 UNION ALL SELECT t.id,t.parent_run FROM threads t JOIN runs r ON r.thread=t.id JOIN ancestors a ON a.parent_run=r.id) SELECT COUNT(*) FROM ancestors",[parent],|r|r.get(0))?;
    ensure!(depth < 16, "agent nesting limit reached");
    ensure!(
        !child.prompt.trim().is_empty() && child.prompt.len() <= 256 * 1024,
        "child prompt must contain 1..262144 bytes"
    );
    ensure!(
        !child.name.trim().is_empty() && child.name.len() <= 160,
        "child name must contain 1..160 bytes"
    );
    let mut profile = base.clone();
    if let Some(model) = child.model {
        ensure!(
            child.role != "explore" || model == base.model,
            "explore must use the parent model"
        );
        profile.model = model.into();
    }
    profile.read_only |=
        child.read_only || child.role == "explore" || child.subagent_type == Some("reviewer");
    validate_profile(&profile, models)?;
    let thread = ThreadId::new();
    tx.execute(
        "INSERT INTO threads(id,session,parent_run,name,profile) VALUES(?1,?2,?3,?4,?5)",
        params![thread, session, run, child.name, encode(&profile)?],
    )?;
    tx.execute(
        "INSERT INTO agents(thread,role,blocking,operation,output_schema,subagent_type) VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            thread,
            child.role,
            child.blocking,
            operation,
            child.schema.map(encode).transpose()?,
            child.subagent_type
        ],
    )?;
    submit(
        tx,
        &CommandId::new(),
        &thread,
        "coordination",
        Some(parent),
        child.prompt,
    )?;
    entry(
        tx,
        parent,
        "agent_started",
        &format!(
            "Started agent {} ({thread}) using {}",
            child.name, profile.model
        ),
        EntryData {
            operation: Some(operation),
            ..Default::default()
        },
    )?;
    Ok(thread)
}
pub(crate) fn settle(
    tx: &Transaction<'_>,
    thread: &ThreadId,
    state: &str,
    text: &str,
) -> anyhow::Result<()> {
    let agent: Option<(Option<String>, String, Option<String>)> = tx
        .query_row(
            "SELECT output_schema,state,outcome FROM agents WHERE thread=?1",
            [thread],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((schema, previous, outcome)) = agent else {
        return Ok(());
    };
    if !matches!(previous.as_str(), "queued" | "running") && outcome.is_some() {
        return Ok(());
    }
    let uncertain: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE thread=?1 AND state='unknown')",
        [thread],
        |r| r.get(0),
    )?;
    let state = if uncertain || previous == "unknown" {
        "unknown"
    } else {
        state
    };
    let validation = if state == "completed" {
        schema
            .map(|schema| -> anyhow::Result<()> {
                let schema: Value = decode(schema)?;
                let value: Value =
                    serde_json::from_str(text).context("final answer is not JSON")?;
                let validator = crate::workflow::schema::CompiledSchema::compile(&schema)
                    .map_err(anyhow::Error::msg)?;
                validator.validate(&value).map_err(anyhow::Error::msg)?;
                Ok(())
            })
            .transpose()
    } else {
        Ok(None)
    };
    let (state, text) = match validation {
        Ok(_) => (state, text.to_owned()),
        Err(error) => (
            "failed",
            format!("Structured output validation failed: {error:#}\nActual output:\n{text}"),
        ),
    };
    let output = artifact(tx, &crate::jobs::bounded(text))?;
    tx.execute(
        "UPDATE agents SET state=?2,outcome=?3 WHERE thread=?1",
        params![thread, state, output],
    )?;
    Ok(())
}
pub(crate) fn reports(db: &Connection, operation: &OperationId) -> anyhow::Result<(bool, Value)> {
    let mut stmt=db.prepare("SELECT a.thread,t.name,a.state,a.outcome FROM agents a JOIN threads t ON t.id=a.thread WHERE a.operation=?1 ORDER BY t.rowid")?;
    let rows = stmt
        .query_map([operation], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut pending = false;
    let mut agents = Vec::new();
    let mut remaining = MAX_ARTIFACT / 8;
    let mut failed = false;
    for (thread, name, state, outcome) in rows {
        pending |= matches!(state.as_str(), "queued" | "running");
        failed |= matches!(state.as_str(), "failed" | "cancelled" | "unknown");
        let output = if let Some(id) = &outcome {
            let size: usize =
                db.query_row("SELECT length FROM artifacts WHERE id=?1", [id], |r| {
                    r.get(0)
                })?;
            if size <= remaining {
                remaining -= size;
                Some(full_artifact(db, &ArtifactId(id.clone()))?)
            } else {
                None
            }
        } else {
            None
        };
        let omitted = outcome.is_some() && output.is_none();
        agents.push(json!({"thread_id":thread,"name":name,"status":state,"output":output,"result_artifact":outcome,"output_omitted":omitted,"note":if omitted {Some("Aggregate output budget exceeded; use read_history with view=artifact, thread_id and artifact_id=result_artifact to retrieve retained output.")}else{None}}));
    }
    Ok((
        pending,
        json!({"status":if pending {"running"}else if failed {"failed"}else{"success"},"agents":agents}),
    ))
}
pub(crate) struct GroupRequest<'a> {
    pub operation: &'a OperationId,
    pub run: &'a RunId,
    pub thread: &'a ThreadId,
    pub name: &'a str,
    pub input: &'a Value,
}

pub(crate) fn launch_group(
    tx: &Transaction<'_>,
    request: GroupRequest<'_>,
    profile: &Profile,
    models: &[ModelInfo],
) -> anyhow::Result<Option<String>> {
    let GroupRequest {
        operation,
        run,
        thread,
        name,
        input,
    } = request;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_groups WHERE operation=?1)",
        [operation],
        |r| r.get(0),
    )?;
    if exists {
        let (pending, reports) = reports(tx, operation)?;
        return if pending {
            Ok(None)
        } else {
            Ok(Some(encode(&reports)?))
        };
    }
    let background = name == "task" && input["background"].as_bool().unwrap_or(false);
    let tasks = if name == "explore" {
        vec![
            json!({"description":input["description"].as_str().unwrap_or("explore"),"prompt":input["query"]}),
        ]
    } else {
        input["tasks"].as_array().context("tasks required")?.clone()
    };
    ensure!(
        !tasks.is_empty() && tasks.len() <= 32,
        "task batch must have 1..32 entries"
    );
    for task in &tasks {
        let subtype = task["subagent_type"].as_str().unwrap_or("general-purpose");
        ensure!(
            matches!(subtype, "general-purpose" | "reviewer"),
            "unknown subagent_type {subtype}"
        );
        launch(
            tx,
            operation,
            run,
            thread,
            profile,
            models,
            Child {
                name: task["description"]
                    .as_str()
                    .context("description required")?,
                prompt: task["prompt"].as_str().context("prompt required")?,
                role: if name == "explore" { "explore" } else { "task" },
                subagent_type: Some(subtype),
                model: task["model"].as_str().or(input["model"].as_str()),
                read_only: input["read_only"].as_bool().unwrap_or(false) || subtype == "reviewer",
                blocking: false,
                schema: None,
            },
        )?;
    }
    let job = if background {
        let job = JobId::new();
        let session = thread_session(tx, thread)?;
        tx.execute("INSERT INTO jobs(id,session,thread,operation,run,kind,state,title,background,epoch) SELECT ?1,?2,?3,?4,?5,'task','running','Background tasks',1,epoch FROM contexts WHERE thread=?3",params![job,session,thread,operation,run])?;
        Some(job)
    } else {
        None
    };
    tx.execute(
        "INSERT INTO task_groups(operation,job) VALUES(?1,?2)",
        params![operation, job],
    )?;
    Ok(job.map(|id|json!({"job_id":id,"message":"Tasks launched; successful and failed results remain inspectable."}).to_string()))
}
pub(crate) fn yield_group(tx: &Transaction<'_>, operation: &OperationId) -> anyhow::Result<JobId> {
    let existing: Option<String> = tx.query_row(
        "SELECT job FROM task_groups WHERE operation=?1",
        [operation],
        |r| r.get(0),
    )?;
    if let Some(job) = existing {
        return Ok(JobId(job));
    }
    let job = JobId::new();
    tx.execute("INSERT INTO jobs(id,session,thread,operation,run,kind,state,title,background,epoch) SELECT ?1,t.session,o.thread,o.id,o.run,'task','running','Tasks collecting after guidance',1,o.epoch FROM operations o JOIN threads t ON t.id=o.thread WHERE o.id=?2", params![job,operation])?;
    tx.execute(
        "UPDATE task_groups SET job=?2 WHERE operation=?1",
        params![operation, job],
    )?;
    Ok(job)
}

pub(crate) fn advance_groups(tx: &Transaction<'_>, session: &SessionId) -> anyhow::Result<()> {
    let groups: Vec<(String, Option<String>)> = {
        let mut stmt = tx.prepare("SELECT g.operation,g.job FROM task_groups g JOIN operations o ON o.id=g.operation JOIN threads t ON t.id=o.thread WHERE g.state='running' AND t.session=?1 LIMIT 128")?;
        let rows = stmt.query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (operation, job) in groups {
        let operation = OperationId(operation);
        if let Some(job) = &job {
            let stopping: bool = tx.query_row(
                "SELECT state='stopping' FROM jobs WHERE id=?1",
                [job],
                |r| r.get(0),
            )?;
            if stopping {
                let children: Vec<String> = {
                    let mut stmt=tx.prepare("SELECT thread FROM agents WHERE operation=?1 AND state IN ('queued','running')")?;
                    let rows = stmt.query_map([&operation], |r| r.get(0))?;
                    rows.collect::<Result<_, _>>()?
                };
                let parent = ThreadId(tx.query_row(
                    "SELECT thread FROM operations WHERE id=?1",
                    [&operation],
                    |r| r.get(0),
                )?);
                for child in children {
                    stop_child(tx, &parent, &ThreadId(child))?;
                }
            }
        }
        let (pending, reports) = reports(tx, &operation)?;
        if pending {
            continue;
        }
        let failed = reports["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["status"] != "completed");
        let state = if reports["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["status"] == "unknown")
        {
            "unknown"
        } else if failed {
            "failed"
        } else {
            "completed"
        };
        tx.execute(
            "UPDATE task_groups SET state=?2 WHERE operation=?1",
            params![operation, state],
        )?;
        if let Some(job) = job {
            let output = artifact(tx, &crate::jobs::bounded(encode(&reports)?))?;
            let updated=tx.execute("UPDATE jobs SET result=?2,state=CASE WHEN state='unknown' OR ?3='unknown' THEN 'unknown' WHEN state='stopping' THEN 'cancelled' ELSE ?3 END,delivered=1 WHERE id=?1 AND state IN ('running','stopping','unknown')",params![job,output,state])?;
            let thread = ThreadId(tx.query_row(
                "SELECT thread FROM operations WHERE id=?1",
                [&operation],
                |r| r.get(0),
            )?);
            if updated > 0 {
                let owner =
                    RunId(tx.query_row("SELECT run FROM jobs WHERE id=?1", [&job], |r| r.get(0))?);
                let summary = artifact_page(tx, &output, 0, 16 * 1024)?;
                crate::lifecycle::queue_completion(tx, &owner, None, &format!("Task job {job} {state}: {summary}\nUse job_output with id={job} for the complete aggregate; individual omitted reports are available through read_history."))?;
            }
            change(tx, Some(&thread), "task_settled", &job)?;
        }
    }
    Ok(())
}
pub(crate) fn tools() -> Vec<ToolDescriptor> {
    let text = json!({"type":"string","minLength":1,"maxLength":16000});
    let task = json!({"type":"object","properties":{"description":text,"prompt":text,"subagent_type":{"type":"string","enum":["general-purpose","reviewer"]},"model":{"type":"string"}},"required":["description","prompt"],"additionalProperties":false});
    [
        ("task","Run independent tasks with atomic admission, no delegation, and all sibling outcomes retained.",json!({"tasks":{"type":"array","minItems":1,"maxItems":32,"items":task},"background":{"type":"boolean"},"read_only":{"type":"boolean"},"model":{"type":"string"}}),vec!["tasks"]),
        ("explore","Investigate a self-contained repository question with same-model read-only tools; cannot execute shell, edit, or delegate.",json!({"query":text,"description":text}),vec!["query"]),
        ("spawn_agent","Launch a named collaborator. It receives a fresh task context and reports durably.",json!({"name":{"type":"string","minLength":1,"maxLength":80},"task":text,"description":text,"subagent_type":{"type":"string","enum":["general-purpose","reviewer"]},"model":{"type":"string"},"read_only":{"type":"boolean"}}),vec!["name","task"]),
    ].into_iter().map(|(name,description,properties,required)|ToolDescriptor{spec:bob_types::ToolSpec{name:name.into(),description:description.into(),input_schema:json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})},effect:EffectClass::Read}).collect()
}
