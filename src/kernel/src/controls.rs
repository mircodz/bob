use crate::contracts::*;
use crate::model::*;
use crate::store::*;
use anyhow::{ensure, Context};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};

pub(crate) fn tools() -> Vec<ToolDescriptor> {
    [
        ("ask_user","Ask a human a real question. Permission bypass never supplies an answer.",json!({"question":{"type":"string","minLength":1,"maxLength":16000},"detail":{"type":"string","maxLength":32000},"options":{"type":"array","minItems":2,"maxItems":4,"items":{"type":"string","minLength":1,"maxLength":2000}}}),vec!["question","options"]),
        ("enter_plan","Enter read-only plan mode before planning a risky implementation.",json!({}),vec![]),
        ("exit_plan","Present a plan for explicit human approval. Implementation remains blocked until approval.",json!({"plan":{"type":"string","minLength":1,"maxLength":65536}}),vec!["plan"]),
        ("todo_write","Replace the durable task list. Keep exactly one unfinished item in progress.",json!({"todos":{"type":"array","maxItems":200,"items":{"type":"object","properties":{"content":{"type":"string","minLength":1,"maxLength":2000},"active_form":{"type":"string","maxLength":2000},"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["content","status"],"additionalProperties":false}}}),vec!["todos"]),
    ].into_iter().map(|(name,description,properties,required)|ToolDescriptor{spec:bob_types::ToolSpec{name:name.into(),description:description.into(),input_schema:json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})},effect:EffectClass::Write}).collect()
}

pub(crate) fn snapshot(db: &Connection, thread: &ThreadId) -> anyhow::Result<ControlSnapshot> {
    let mut snapshot = ControlSnapshot::default();
    let mut stmt=db.prepare("SELECT h.operation,h.kind,h.revision,h.payload FROM human_controls h JOIN operations o ON o.id=h.operation JOIN runs r ON r.id=o.run JOIN contexts c ON c.thread=o.thread WHERE o.thread=?1 AND h.state='pending' AND o.state='waiting' AND r.state IN ('active','interrupted') AND h.epoch=c.epoch ORDER BY h.rowid LIMIT 100")?;
    let rows = stmt.query_map([thread], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (operation, kind, revision, payload) = row?;
        let payload: Value = decode(payload)?;
        if kind == "question" {
            snapshot.questions.push(HumanQuestion {
                operation: OperationId(operation),
                revision,
                question: payload["question"].as_str().unwrap_or_default().into(),
                detail: payload["detail"].as_str().map(str::to_owned),
                options: serde_json::from_value(payload["options"].clone())?,
            });
        } else {
            snapshot.plans.push(PlanProposal {
                operation: OperationId(operation),
                revision,
                plan: payload["plan"].as_str().unwrap_or_default().into(),
            });
        }
    }
    let todos: Option<String> = db
        .query_row("SELECT items FROM todos WHERE thread=?1", [thread], |r| {
            r.get(0)
        })
        .optional()?;
    snapshot.todos = todos.map(decode).transpose()?.unwrap_or_default();
    Ok(snapshot)
}

pub(crate) fn set_plan(tx: &Transaction<'_>, thread: &ThreadId, plan: bool) -> anyhow::Result<()> {
    let session = thread_session(tx, thread)?;
    let (policy, permissions): (String, String) = tx.query_row(
        "SELECT policy,permissions FROM sessions WHERE id=?1",
        [&session],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut policy: Policy = decode(policy)?;
    let mut permissions: PermissionPolicy = decode(permissions)?;
    policy.plan = plan;
    permissions.mode = if plan {
        PermissionMode::Plan
    } else {
        PermissionMode::Normal
    };
    tx.execute(
        "UPDATE sessions SET policy=?1,permissions=?2,revision=revision+1 WHERE id=?3",
        params![encode(&policy)?, encode(&permissions)?, session],
    )?;
    crate::session_commands::invalidate_approvals(tx, &session)?;
    Ok(())
}

pub(crate) fn execute(
    tx: &Transaction<'_>,
    operation: &OperationId,
    thread: &ThreadId,
    name: &str,
    input: &Value,
) -> anyhow::Result<Option<String>> {
    match name {
        "ask_user" | "exit_plan" => {
            if name == "exit_plan" {
                let root: bool = tx.query_row(
                    "SELECT parent_run IS NULL FROM threads WHERE id=?1",
                    [thread],
                    |r| r.get(0),
                )?;
                ensure!(root, "only the root agent may propose a session plan");
                set_plan(tx, thread, true)?;
            }
            let (revision,epoch):(i64,i64)=tx.query_row("SELECT s.revision,c.epoch FROM threads t JOIN sessions s ON s.id=t.session JOIN contexts c ON c.thread=t.id WHERE t.id=?1",[thread],|r|Ok((r.get(0)?,r.get(1)?)))?;
            tx.execute("INSERT INTO human_controls(operation,kind,revision,epoch,payload,state) VALUES(?1,?2,?3,?4,?5,'pending') ON CONFLICT(operation) DO NOTHING",params![operation,if name=="ask_user" {"question"} else {"plan"},revision,epoch,encode(input)?])?;
            Ok(None)
        }
        "enter_plan" => {
            let root: bool = tx.query_row(
                "SELECT parent_run IS NULL FROM threads WHERE id=?1",
                [thread],
                |r| r.get(0),
            )?;
            ensure!(root, "only the root agent may change the session mode");
            set_plan(tx, thread, true)?;
            Ok(Some("Entered plan mode. Workspace mutations and shell execution are blocked until a human approves a plan.".into()))
        }
        "todo_write" => {
            let items: Vec<TodoItem> = serde_json::from_value(input["todos"].clone())?;
            let unfinished = items
                .iter()
                .any(|item| item.status != TodoStatus::Completed);
            let active = items
                .iter()
                .filter(|item| item.status == TodoStatus::InProgress)
                .count();
            ensure!(
                (unfinished && active == 1) || (!unfinished && active == 0),
                "keep exactly one unfinished todo in progress"
            );
            tx.execute("INSERT INTO todos(thread,items) VALUES(?1,?2) ON CONFLICT(thread) DO UPDATE SET items=excluded.items",params![thread,encode(&items)?])?;
            Ok(Some(format!("Task list updated ({} items).", items.len())))
        }
        _ => anyhow::bail!("unknown internal control"),
    }
}

pub(crate) fn answer(
    tx: &Transaction<'_>,
    operation: &OperationId,
    revision: i64,
    kind: &str,
    text: &str,
    approve: bool,
) -> anyhow::Result<(SessionId, ThreadId, String)> {
    let (thread,stored,current,epoch,current_epoch,state,run_state):(String,i64,i64,i64,i64,String,String)=tx.query_row("SELECT o.thread,h.revision,s.revision,h.epoch,c.epoch,h.state,r.state FROM human_controls h JOIN operations o ON o.id=h.operation JOIN threads t ON t.id=o.thread JOIN sessions s ON s.id=t.session JOIN contexts c ON c.thread=t.id JOIN runs r ON r.id=o.run WHERE h.operation=?1 AND h.kind=?2 AND o.state='waiting'",params![operation,kind],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).context("question or plan is no longer pending")?;
    ensure!(
        revision == stored && revision == current && epoch == current_epoch && state == "pending",
        "decision is stale; refresh before answering"
    );
    ensure!(
        run_state == "active",
        "resume the owning run before answering"
    );
    let thread = ThreadId(thread);
    let session = thread_session(tx, &thread)?;
    tx.execute(
        "UPDATE human_controls SET state='answered' WHERE operation=?1",
        [operation],
    )?;
    if kind == "plan" && approve {
        set_plan(tx, &thread, false)?;
    }
    finish_tool_entry(tx, operation, text, false)?;
    tx.execute(
        "UPDATE operations SET state='done' WHERE id=?1",
        [operation],
    )?;
    Ok((
        session,
        thread,
        if kind == "plan" {
            "plan decision recorded"
        } else {
            "human answer recorded"
        }
        .into(),
    ))
}
