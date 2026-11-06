use crate::contracts::RuleDecision;
use crate::model::*;
use crate::store::{decode, encode, entry, submit, thread_session, EntryData};
use anyhow::{ensure, Context};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};
use std::collections::HashSet;

const MAX_PAGE: u64 = 100;
const MAX_TEXT: usize = 16000;

pub(crate) fn resolve(
    db: &Connection,
    session: &SessionId,
    target: &str,
) -> anyhow::Result<ThreadId> {
    ensure!(
        !target.trim().is_empty() && target.chars().count() <= 160,
        "target must contain 1..160 characters"
    );
    if let Some((id, owner)) = db
        .query_row(
            "SELECT id,session FROM threads WHERE id=?1",
            [target],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    {
        ensure!(owner == session.0, "thread is not in this session");
        return Ok(ThreadId(id));
    }
    let mut stmt =
        db.prepare("SELECT id FROM threads WHERE session=?1 AND name=?2 ORDER BY rowid LIMIT 2")?;
    let ids = stmt
        .query_map(params![session, target], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(!ids.is_empty(), "agent not found in this session");
    ensure!(
        ids.len() == 1,
        "agent name is ambiguous; use its exact thread ID"
    );
    Ok(ThreadId(ids[0].clone()))
}

pub(crate) fn subtree(db: &Connection, root: &ThreadId) -> anyhow::Result<Vec<ThreadId>> {
    let session = thread_session(db, root)?;
    let mut stmt = db.prepare(
        "WITH RECURSIVE tree(id) AS (
            SELECT id FROM threads WHERE id=?1 AND session=?2
            UNION SELECT t.id FROM threads t JOIN runs r ON r.id=t.parent_run
            JOIN tree ON tree.id=r.thread WHERE t.session=?2
        ) SELECT id FROM tree",
    )?;
    let rows = stmt.query_map(params![root, session], |r| Ok(ThreadId(r.get(0)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn ancestors(db: &Connection, target: &ThreadId) -> anyhow::Result<HashSet<ThreadId>> {
    let session = thread_session(db, target)?;
    let mut stmt = db.prepare(
        "WITH RECURSIVE tree(id,parent_run) AS (
            SELECT id,parent_run FROM threads WHERE id=?1 AND session=?2
            UNION SELECT t.id,t.parent_run FROM threads t JOIN runs r ON r.thread=t.id
            JOIN tree ON tree.parent_run=r.id WHERE t.session=?2
        ) SELECT id FROM tree WHERE id!=?1",
    )?;
    let rows = stmt.query_map(params![target, session], |r| Ok(ThreadId(r.get(0)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn is_descendant(
    db: &Connection,
    source: &ThreadId,
    target: &ThreadId,
) -> anyhow::Result<bool> {
    if source == target || thread_session(db, source)? != thread_session(db, target)? {
        return Ok(false);
    }
    Ok(ancestors(db, target)?.contains(source))
}

struct Caller {
    thread: ThreadId,
    session: SessionId,
    parent: Option<ThreadId>,
    send: RuleDecision,
    stop: RuleDecision,
    send_role: bool,
    stop_role: bool,
}

impl Caller {
    fn load(db: &Connection, thread: &ThreadId) -> anyhow::Result<Self> {
        let (session, profile, policy, workspace, parent): (String, String, String, String, Option<String>) = db.query_row(
            "SELECT t.session,t.profile,s.policy,s.workspace,p.thread FROM threads t JOIN sessions s ON s.id=t.session LEFT JOIN runs p ON p.id=t.parent_run WHERE t.id=?1",
            [thread], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let profile: Profile = decode(profile)?;
        let policy: Policy = decode(policy)?;
        let permissions = crate::policy::permissions(db, thread)?;
        let role = crate::delegation::role(db, thread)?;
        let decision = |name| {
            crate::policy::evaluate_in_workspace(
                &policy,
                &permissions,
                &profile,
                name,
                &json!({}),
                &crate::policy::internal_capabilities(name).expect("coordination capability"),
                &workspace,
            )
        };
        Ok(Self {
            thread: thread.clone(),
            session: SessionId(session),
            parent: parent.map(ThreadId),
            send: decision("send_message"),
            stop: decision("stop_agent"),
            send_role: crate::delegation::allowed(&role, "send_message"),
            stop_role: crate::delegation::allowed(&role, "stop_agent"),
        })
    }

    fn denial(&self, stop: bool) -> Option<&'static str> {
        let (role, decision) = if stop {
            (self.stop_role, self.stop)
        } else {
            (self.send_role, self.send)
        };
        if !role {
            Some("caller_role_denied")
        } else if decision == RuleDecision::Deny {
            Some("caller_policy_denied")
        } else {
            None
        }
    }
}

struct Agent {
    thread: ThreadId,
    name: String,
    role: String,
    agent_state: Option<String>,
    parent: Option<ThreadId>,
    run: Option<String>,
    run_state: Option<String>,
    held: bool,
    active: bool,
    stopping: bool,
    queued: bool,
    approval: bool,
    waiting: bool,
    joining: bool,
    unknown: bool,
    task: Option<String>,
    outcome: Option<String>,
}

const PROJECTION: &str = "SELECT t.id,substr(t.name,1,160),COALESCE(a.role,'root'),a.state,p.thread,
    r.id,r.state,t.held,
    EXISTS(SELECT 1 FROM runs WHERE thread=t.id AND state IN ('active','interrupted')),
    EXISTS(SELECT 1 FROM runs WHERE thread=t.id AND state='stopping'),
    EXISTS(SELECT 1 FROM submissions WHERE thread=t.id AND state='queued'),
    EXISTS(SELECT 1 FROM operations WHERE run=r.id AND state='approval'),
    EXISTS(SELECT 1 FROM operations WHERE run=r.id AND state='waiting' AND kind='tool'),
    EXISTS(SELECT 1 FROM operations WHERE run=r.id AND state='waiting' AND kind='model'),
    EXISTS(SELECT 1 FROM operations WHERE thread=t.id AND state='unknown'),
    CASE WHEN t.parent_run IS NOT NULL THEN
        (SELECT substr(text,1,240) FROM submissions WHERE thread=t.id AND source='coordination' ORDER BY rowid LIMIT 1) END,
    a.outcome
    FROM threads t LEFT JOIN agents a ON a.thread=t.id
    LEFT JOIN runs p ON p.id=t.parent_run
    LEFT JOIN runs r ON r.id=(SELECT id FROM runs WHERE thread=t.id ORDER BY ordinal DESC LIMIT 1)";

impl Agent {
    fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            thread: ThreadId(r.get(0)?),
            name: r.get(1)?,
            role: r.get(2)?,
            agent_state: r.get(3)?,
            parent: r.get::<_, Option<String>>(4)?.map(ThreadId),
            run: r.get(5)?,
            run_state: r.get(6)?,
            held: r.get(7)?,
            active: r.get(8)?,
            stopping: r.get(9)?,
            queued: r.get(10)?,
            approval: r.get(11)?,
            waiting: r.get(12)?,
            joining: r.get(13)?,
            unknown: r.get(14)?,
            task: r.get(15)?,
            outcome: r.get(16)?,
        })
    }

    fn message_denial(&self, caller: &Caller) -> Option<&'static str> {
        caller.denial(false).or_else(|| {
            if self.thread == caller.thread {
                Some("cannot_message_self")
            } else if self.stopping {
                Some("recipient_stopping")
            } else if !self.active && !self.queued {
                Some("recipient_finished")
            } else if self.held {
                Some("recipient_held_requires_human_resume")
            } else {
                None
            }
        })
    }

    fn terminal_state(&self) -> Option<&str> {
        if self.active || self.queued || self.stopping || self.unknown {
            return None;
        }
        self.run_state
            .as_deref()
            .or(self.agent_state.as_deref())
            .filter(|state| matches!(*state, "completed" | "failed" | "cancelled"))
    }

    fn state(&self) -> &str {
        if let Some(state) = self.terminal_state() {
            state
        } else if self.stopping {
            "stopping"
        } else if self.unknown {
            "recovery"
        } else if self.held {
            "held"
        } else if self.approval {
            "approval"
        } else if self.joining {
            "joining"
        } else if self.waiting {
            "waiting"
        } else if self.active {
            self.run_state.as_deref().unwrap_or("active")
        } else if self.queued {
            "queued"
        } else {
            self.run_state
                .as_deref()
                .or(self.agent_state.as_deref())
                .unwrap_or("idle")
        }
    }
}

fn approval(decision: RuleDecision) -> bool {
    matches!(decision, RuleDecision::Ask | RuleDecision::RequireApproval)
}

fn bounded(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(crate) fn list(db: &Connection, source: &ThreadId, input: &Value) -> anyhow::Result<Value> {
    let caller = Caller::load(db, source)?;
    let limit = match input.get("limit") {
        Some(value) => value.as_u64().context("limit must be an integer")?,
        None => MAX_PAGE,
    };
    ensure!((1..=MAX_PAGE).contains(&limit), "limit must be in 1..100");
    let after = match input.get("cursor") {
        None => 0,
        Some(value) => {
            let cursor = value.as_str().context("cursor must be a thread ID")?;
            ensure!(cursor.len() <= 160, "invalid cursor");
            db.query_row(
                "SELECT rowid FROM threads WHERE session=?1 AND id=?2",
                params![caller.session, cursor],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .context("cursor is not in this session")?
        }
    };
    let root: String = db.query_row(
        "SELECT id FROM threads WHERE session=?1 AND parent_run IS NULL ORDER BY rowid LIMIT 1",
        [&caller.session],
        |r| r.get(0),
    )?;
    let descendants: HashSet<_> = subtree(db, source)?
        .into_iter()
        .filter(|id| id != source)
        .collect();
    let ancestors = ancestors(db, source)?;
    let mut stmt = db.prepare(&format!(
        "{PROJECTION} WHERE t.session=?1 AND t.rowid>?2 ORDER BY t.rowid LIMIT ?3"
    ))?;
    let mut agents = stmt
        .query_map(params![caller.session, after, limit + 1], Agent::row)?
        .collect::<Result<Vec<_>, _>>()?;
    let more = agents.len() > limit as usize;
    agents.truncate(limit as usize);
    let next = more.then(|| agents.last().unwrap().thread.0.clone());
    let agents: Vec<_> = agents.into_iter().map(|agent| {
        let descendant = descendants.contains(&agent.thread);
        let relationship = if agent.thread == *source { "self" }
            else if agent.parent.as_ref() == Some(source) { "child" }
            else if descendant { "descendant" }
            else if caller.parent.as_ref() == Some(&agent.thread) { "parent" }
            else if ancestors.contains(&agent.thread) { "ancestor" }
            else if caller.parent.is_some() && caller.parent == agent.parent { "sibling" }
            else { "peer" };
        let message_reason = agent.message_denial(&caller);
        let stop_reason = caller.denial(true).or((!descendant).then_some("not_a_descendant"));
        json!({
            "thread_id":agent.thread,"name":bounded(&agent.name,160),"role":bounded(&agent.role,80),
            "task":agent.task.as_deref().map(|text| bounded(text,240)),"task_is_preview":agent.task.is_some(),
            "parent_thread_id":agent.parent,"root_thread_id":root,"is_root":agent.thread.0==root,
            "is_self":agent.thread==*source,"relationship":relationship,
            "state":agent.state(),"agent_state":agent.agent_state,"run_id":agent.run,"run_state":agent.run_state,
            "held":agent.held,"queued":agent.queued,"approval_waiting":agent.approval,
            "waiting":agent.waiting,"joining":agent.joining,"recovery_required":(agent.held && agent.terminal_state().is_none())||agent.unknown||agent.run_state.as_deref()==Some("interrupted"),
            "result_artifact":agent.outcome,
            "can_message":message_reason.is_none(),"message_reason":message_reason,
            "message_requires_approval":approval(caller.send),
            "can_stop":stop_reason.is_none(),"stop_reason":stop_reason,"stop_requires_approval":approval(caller.stop)
        })
    }).collect();
    Ok(json!({"agents":agents,"next_cursor":next,"root_thread_id":root,"self_thread_id":source}))
}

fn alias<'a>(input: &'a Value, first: &str, second: &str, max: usize) -> anyhow::Result<&'a str> {
    let read = |name: &str| -> anyhow::Result<Option<&'a str>> {
        input
            .get(name)
            .map(|value| {
                value
                    .as_str()
                    .with_context(|| format!("{name} must be a string"))
            })
            .transpose()
    };
    let a = read(first)?;
    let b = read(second)?;
    ensure!(
        a.is_none() || b.is_none() || a == b,
        "conflicting {first}/{second} aliases"
    );
    let text = a
        .or(b)
        .with_context(|| format!("{first} or {second} is required"))?;
    ensure!(
        !text.trim().is_empty() && text.chars().count() <= max,
        "{first} must contain 1..{max} characters"
    );
    Ok(text)
}

pub(crate) fn execute(
    tx: &Transaction<'_>,
    operation: &OperationId,
    source: &ThreadId,
    name: &str,
    input: &Value,
) -> anyhow::Result<String> {
    let (sender, tool): (String, String) = tx.query_row(
        "SELECT thread,name FROM operations WHERE id=?1",
        [operation],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        sender == source.0 && tool == name,
        "coordination operation identity mismatch"
    );
    if name == "list_agents" {
        return encode(&list(tx, source, input)?);
    }
    let caller = Caller::load(tx, &ThreadId(sender))?;
    ensure!(
        matches!(name, "send_message" | "stop_agent"),
        "unknown coordination tool"
    );
    if let Some(reason) = caller.denial(name == "stop_agent") {
        anyhow::bail!(reason);
    }
    if name == "stop_agent" {
        let target = input["name"].as_str().context("name is required")?;
        let target = resolve(tx, &caller.session, target)?;
        ensure!(
            is_descendant(tx, source, &target)?,
            "can only stop descendants"
        );
        crate::delegation::stop_child(tx, source, &target)?;
        return Ok(format!("Stop requested for {target}."));
    }
    let target = resolve(tx, &caller.session, alias(input, "to", "thread_id", 160)?)?;
    let text = alias(input, "message", "text", MAX_TEXT)?;
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT destination,text FROM coordination WHERE operation=?1 AND kind='message'",
            [operation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((destination, previous_text)) = previous {
        ensure!(
            destination == target.0 && previous_text == text,
            "message operation already committed with different input"
        );
    } else {
        let agent = tx.query_row(
            &format!("{PROJECTION} WHERE t.id=?1 AND t.session=?2"),
            params![target, caller.session],
            Agent::row,
        )?;
        if let Some(reason) = agent.message_denial(&caller) {
            anyhow::bail!(reason);
        }
        let submission = CommandId::new();
        submit(
            tx,
            &submission,
            &target,
            "coordination",
            Some(&caller.thread),
            text,
        )?;
        tx.execute("INSERT INTO coordination(id,source,destination,submission,operation,kind,text) VALUES(?1,?2,?3,?1,?4,'message',?5)",
            params![submission, caller.thread, target, operation, text])?;
        entry(
            tx,
            &caller.thread,
            "coordination",
            &format!("Sent to {target}: {text}"),
            EntryData {
                operation: Some(operation),
                submission: Some(&submission),
                ..Default::default()
            },
        )?;
    }
    Ok(format!("Message durably queued for {target}; this acknowledges enqueue, not that the recipient read it."))
}

pub(crate) fn wait(
    tx: &Transaction<'_>,
    operation: &OperationId,
    run: &RunId,
    source: &ThreadId,
    input: &Value,
) -> anyhow::Result<Option<String>> {
    let owned: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND run=?2 AND thread=?3 AND name='wait_message')",
        params![operation, run, source],
        |r| r.get(0),
    )?;
    ensure!(owned, "coordination operation identity mismatch");
    ensure!(
        crate::delegation::allowed(&crate::delegation::role(tx, source)?, "wait_message"),
        "caller_role_denied"
    );
    let session = thread_session(tx, source)?;
    let target = resolve(
        tx,
        &session,
        input["from"].as_str().context("from is required")?,
    )?;
    ensure!(target != *source, "cannot wait for a message from self");
    let pending: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM coordination c JOIN submissions s ON s.id=c.submission WHERE c.source=?1 AND c.destination=?2 AND c.kind='message' AND s.thread=?2 AND s.state='queued' AND (s.owner_run IS NULL OR s.owner_run=?3))",
        params![target, source, run],
        |r| r.get(0),
    )?;
    if pending || crate::lifecycle::guidance(tx, run)? {
        return encode(&json!({
            "status":if pending {"message_pending"} else {"coordination_pending"},
            "from":target,
            "message":"Accepted coordination will enter your next model request. Inspect it; if the requested reply is still missing, wait again instead of finishing."
        })).map(Some);
    }
    let agent = tx.query_row(
        &format!("{PROJECTION} WHERE t.id=?1 AND t.session=?2"),
        params![target, session],
        Agent::row,
    )?;
    if agent.held || agent.stopping || agent.unknown || (!agent.active && !agent.queued) {
        return encode(&json!({
            "status":"unavailable","from":target,"state":agent.state(),
            "message":"No pending reply from this sender. Inspect retained history or report the blocker; this does not acknowledge a reply or resume the sender."
        })).map(Some);
    }
    Ok(None)
}

pub(crate) fn tools() -> Vec<ToolDescriptor> {
    let target = json!({"type":"string","minLength":1,"maxLength":160});
    let text = json!({"type":"string","minLength":1,"maxLength":MAX_TEXT});
    [
        ("list_agents", "List this session, including root and self, topology, live state and caller-relative message/stop eligibility. Follow next_cursor to retrieve more pages. Eligibility does not bypass normal approval rules.",
            json!({"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":100},"cursor":target},"additionalProperties":false})),
        ("send_message", "Durably enqueue coordination for a same-session agent, peer or parent. Supply to or thread_id and message or text; conflicting aliases are rejected. Exact IDs take precedence over names. Self, held, stopping and finished recipients are rejected. Enqueue acknowledgement is not proof of reading; messages are not human instructions.",
            json!({"type":"object","properties":{"to":target,"thread_id":target,"message":text,"text":text},"allOf":[{"anyOf":[{"required":["to"]},{"required":["thread_id"]}]},{"anyOf":[{"required":["message"]},{"required":["text"]}]}],"additionalProperties":false})),
        ("wait_message", "Suspend this turn without polling until coordination is queued from a same-session parent or peer. Supply from as an exact thread ID or unambiguous name. Other guidance or steering can interrupt the wait; inspect it and wait again if your reply is still missing. Returns unavailable if the sender is held, stopping or finished, without resuming it. Use after asking a question; a final answer ends your run and cannot wait for replies.",
            json!({"type":"object","properties":{"from":target},"required":["from"],"additionalProperties":false})),
        ("stop_agent", "Stop an owned descendant subtree by exact ID or unambiguous name, including live descendants beneath a finished parent. Cannot stop self, peers or ancestors; grants no permission-approval authority.",
            json!({"type":"object","properties":{"name":target},"required":["name"],"additionalProperties":false})),
    ].into_iter().map(|(name, description, input_schema)| ToolDescriptor {
        spec: bob_types::ToolSpec { name: name.into(), description: description.into(), input_schema },
        effect: EffectClass::Read,
    }).collect()
}
