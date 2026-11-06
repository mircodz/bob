use super::{
    dsl::{self, Op, Spec, Step},
    input, params, validation,
};
use crate::contracts::RuleDecision;
use crate::delegation::{self, Child};
use crate::model::*;
use crate::store::*;
use anyhow::{ensure, Context};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub(crate) const SCHEMA:&str="
CREATE TABLE workflows(operation TEXT PRIMARY KEY REFERENCES operations(id), job TEXT NOT NULL REFERENCES jobs(id), state TEXT NOT NULL, executions INTEGER NOT NULL DEFAULT 0, result TEXT REFERENCES artifacts(id));
CREATE TABLE workflow_nodes(operation TEXT NOT NULL REFERENCES workflows(operation), path TEXT NOT NULL, state TEXT NOT NULL, specification TEXT NOT NULL, iteration INTEGER NOT NULL DEFAULT 0, child TEXT REFERENCES threads(id), result TEXT REFERENCES artifacts(id), error TEXT, PRIMARY KEY(operation,path));
CREATE TABLE workflow_dependencies(operation TEXT NOT NULL REFERENCES workflows(operation), node TEXT NOT NULL, dependency TEXT NOT NULL, PRIMARY KEY(operation,node,dependency));
CREATE TABLE workflow_iterations(operation TEXT NOT NULL REFERENCES workflows(operation), node TEXT NOT NULL, iteration INTEGER NOT NULL, result TEXT NOT NULL REFERENCES artifacts(id), PRIMARY KEY(operation,node,iteration));
";
#[derive(Clone, Serialize, Deserialize)]
struct Outcome {
    output: Value,
    error: Option<String>,
}
enum Progress {
    Pending,
    Settled(Outcome),
}
impl Outcome {
    fn success(output: Value) -> Self {
        Self {
            output,
            error: None,
        }
    }
    fn failed(output: Value, error: impl Into<String>) -> Self {
        Self {
            output,
            error: Some(error.into()),
        }
    }
}
struct Runner<'a, 'tx> {
    tx: &'a Transaction<'tx>,
    operation: &'a OperationId,
    run: &'a RunId,
    thread: &'a ThreadId,
    profile: Profile,
    models: &'a [ModelInfo],
    max_concurrency: usize,
}
pub(crate) fn execute(
    tx: &Transaction<'_>,
    operation: &OperationId,
    run: &RunId,
    thread: &ThreadId,
    profile: &Profile,
    models: &[ModelInfo],
    value: &Value,
) -> anyhow::Result<Option<String>> {
    input::validate(value).map_err(anyhow::Error::msg)?;
    let spec = if value.get("steps").is_some() {
        let spec: Spec = serde_json::from_value(value.clone())?;
        validation::validate(&spec).map_err(anyhow::Error::msg)?;
        Some(spec)
    } else {
        let shape: params::WorkflowParams = serde_json::from_value(value.clone())?;
        params::validate(&shape).map_err(anyhow::Error::msg)?;
        None
    };
    let existing: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT state,result FROM workflows WHERE operation=?1",
            [operation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((_, Some(result))) = &existing {
        return Ok(Some(full_artifact(tx, &ArtifactId(result.clone()))?));
    }
    if existing.is_none() {
        let job = JobId::new();
        let session = thread_session(tx, thread)?;
        tx.execute("INSERT INTO jobs(id,session,thread,operation,run,kind,state,title,background,epoch) SELECT ?1,?2,?3,?4,?5,'workflow','running',?6,0,epoch FROM contexts WHERE thread=?3",params![job,session,thread,operation,run,preview(value["title"].as_str().unwrap_or("Workflow"))])?;
        tx.execute(
            "INSERT INTO workflows(operation,job,state) VALUES(?1,?2,'running')",
            params![operation, job],
        )?;
    }
    let job_state: String = tx.query_row(
        "SELECT j.state FROM jobs j JOIN workflows w ON w.job=j.id WHERE w.operation=?1",
        [operation],
        |r| r.get(0),
    )?;
    let mut profile = profile.clone();
    profile.read_only |= value["read_only"].as_bool().unwrap_or(false);
    let runner = Runner {
        tx,
        operation,
        run,
        thread,
        profile,
        models,
        max_concurrency: value["max_concurrency"].as_u64().unwrap_or(64) as usize,
    };
    let failure: Option<String> = tx
        .query_row(
            "SELECT error FROM workflow_nodes WHERE operation=?1 AND path='!failure'",
            [operation],
            |r| r.get(0),
        )
        .optional()?;
    let progress = if failure.is_some()
        || matches!(job_state.as_str(), "unknown" | "stopping" | "cancelled")
    {
        let children: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT thread FROM agents WHERE operation=?1 AND state IN ('queued','running')",
            )?;
            let rows = stmt.query_map([operation], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        if job_state != "unknown" {
            for child in children {
                delegation::stop_child(tx, thread, &ThreadId(child))?;
            }
            if runner.occupancy(Some(operation))? > 0 {
                return Ok(None);
            }
        }
        Progress::Settled(Outcome::failed(runner.partial()?, failure.unwrap_or_else(|| format!("Workflow {job_state}; child effects are retained and are not replayed automatically."))))
    } else if let Some(spec) = spec {
        runner.steps("root", &spec.steps, &Map::new())?
    } else {
        runner.shape(serde_json::from_value(value.clone())?)?
    };
    let Progress::Settled(outcome) = progress else {
        return Ok(None);
    };
    let (_, agents) = delegation::reports(tx, operation)?;
    let result = json!({"status":if outcome.error.is_some(){"failed"}else{"success"},"output":outcome.output,"error":outcome.error,"agents":agents["agents"]});
    let text = encode(&result)?;
    let oversized = text.len() > MAX_ARTIFACT;
    let text = if oversized {
        encode(
            &json!({"status":"failed","error":"Aggregate exceeds retention budget; inspect each child thread and workflow node artifact for full outputs.","agents":result["agents"].as_array().unwrap().iter().map(|a|json!({"thread_id":a["thread_id"],"status":a["status"]})).collect::<Vec<_>>()}),
        )?
    } else {
        text
    };
    let artifact = artifact(tx, &text)?;
    let unknown = agents["agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|agent| agent["status"] == "unknown");
    let state = if job_state == "unknown" || unknown {
        "unknown"
    } else if matches!(job_state.as_str(), "stopping" | "cancelled") {
        "cancelled"
    } else if !oversized && result["status"] == "success" {
        "completed"
    } else {
        "failed"
    };
    tx.execute(
        "UPDATE workflows SET state=?2,result=?3 WHERE operation=?1",
        params![operation, state, artifact],
    )?;
    tx.execute(
        "UPDATE jobs SET state=?2,result=?3,delivered=1 WHERE operation=?1",
        params![operation, state, artifact],
    )?;
    change(tx, Some(thread), "workflow_settled", &operation.0)?;
    Ok(Some(text))
}
pub(crate) fn fail(
    tx: &Transaction<'_>,
    operation: &OperationId,
    error: &str,
) -> anyhow::Result<bool> {
    let existing: Option<Option<String>> = tx
        .query_row(
            "SELECT result FROM workflows WHERE operation=?1",
            [operation],
            |r| r.get(0),
        )
        .optional()?;
    let Some(result) = existing else {
        return Ok(false);
    };
    if result.is_some() {
        return Ok(true);
    }
    tx.execute("INSERT OR IGNORE INTO workflow_nodes(operation,path,state,specification,error) VALUES(?1,'!failure','failed','{}',?2)", params![operation, crate::jobs::bounded(format!("Workflow execution failed: {error}"))])?;
    tx.execute(
        "UPDATE workflows SET state='failing' WHERE operation=?1",
        [operation],
    )?;
    let parent = ThreadId(tx.query_row(
        "SELECT thread FROM operations WHERE id=?1",
        [operation],
        |r| r.get(0),
    )?);
    let children: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT thread FROM agents WHERE operation=?1 AND state IN ('queued','running')",
        )?;
        let rows = stmt.query_map([operation], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for child in children {
        delegation::stop_child(tx, &parent, &ThreadId(child))?;
    }
    change(tx, Some(&parent), "workflow_failing", &operation.0)?;
    Ok(true)
}

fn authorize_progress(
    tx: &Transaction<'_>,
    operation: &OperationId,
    thread: &ThreadId,
    profile: &Profile,
    input: &Value,
) -> anyhow::Result<()> {
    let role = delegation::role(tx, thread)?;
    ensure!(
        delegation::allowed(&role, "workflow"),
        "Workflow continuation is unavailable to this agent role."
    );
    let (policy, revision, workspace): (String, i64, String) = tx.query_row(
        "SELECT s.policy,s.revision,s.workspace FROM sessions s JOIN threads t ON t.session=s.id WHERE t.id=?1",
        [thread],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let decision = crate::policy::evaluate_with_fallback(
        &decode(policy)?,
        &crate::policy::permissions(tx, thread)?,
        profile,
        crate::policy::ToolRequest {
            name: "workflow",
            input,
            workspace: &workspace,
        },
        &crate::policy::internal_capabilities("workflow").unwrap(),
        Some(RuleDecision::Allow),
    );
    ensure!(
        decision != RuleDecision::Deny,
        "Workflow continuation denied by current policy or capability restrictions."
    );
    if decision != RuleDecision::Allow {
        let approved: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations o JOIN decisions d ON d.operation=o.id WHERE o.id=?1 AND o.policy_revision=?2 AND d.revision=?2 AND d.state='approved')",
            params![operation, revision],
            |r| r.get(0),
        )?;
        // A yielded call already has its sole tool result; fail the retained job rather
        // than reopening that call or borrowing an approval from another revision.
        ensure!(
            approved,
            "Workflow continuation requires current approval; the previous approval is missing or stale. Retained work is not replayed; submit a new workflow to authorize further work."
        );
    }
    Ok(())
}

pub(crate) fn cleanup(
    tx: &Transaction<'_>,
    session: &SessionId,
    models: &[ModelInfo],
) -> anyhow::Result<()> {
    let rows = {
        let mut stmt = tx.prepare("SELECT o.id,o.run,o.thread,o.input,t.profile,w.state='running' AND j.state='running' FROM workflows w JOIN jobs j ON j.id=w.job JOIN operations o ON o.id=w.operation JOIN threads t ON t.id=o.thread WHERE w.result IS NULL AND (w.state='failing' OR j.state IN ('stopping','unknown') OR (j.background=1 AND j.state='running' AND EXISTS(SELECT 1 FROM runs r JOIN contexts c ON c.thread=r.thread WHERE r.id=o.run AND r.state='active' AND r.epoch=c.epoch) AND t.held=0)) AND t.session=?1 LIMIT 128")?;
        let rows = stmt.query_map([session], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, bool>(5)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (operation, run, thread, input, profile, advancing) in rows {
        let operation = OperationId(operation);
        let run = RunId(run);
        let thread = ThreadId(thread);
        let profile = decode(profile)?;
        let input = decode(input)?;
        tx.execute_batch("SAVEPOINT workflow_progress")?;
        let result = (|| {
            if advancing {
                authorize_progress(tx, &operation, &thread, &profile, &input)?;
            }
            execute(tx, &operation, &run, &thread, &profile, models, &input)
        })();
        if result.is_err() {
            tx.execute_batch("ROLLBACK TO workflow_progress")?;
        }
        tx.execute_batch("RELEASE workflow_progress")?;
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                fail(tx, &operation, &format!("{error:#}"))?;
                execute(tx, &operation, &run, &thread, &profile, models, &input)?
            }
        };
        if let Some(output) = output {
            let (background, job): (bool, String) = tx.query_row(
                "SELECT background,id FROM jobs WHERE operation=?1",
                [&operation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if background {
                let mut end = output.len().min(16 * 1024);
                while !output.is_char_boundary(end) {
                    end -= 1;
                }
                crate::lifecycle::queue_completion(tx, &run, None, &format!("Workflow job {job} settled: {}\nUse job_output with id={job} for the complete aggregate.", &output[..end]))?;
            }
        }
    }
    Ok(())
}

impl Runner<'_, '_> {
    fn partial(&self) -> anyhow::Result<Value> {
        let mut stmt = self.tx.prepare(
            "SELECT path,state,result,error FROM workflow_nodes WHERE operation=?1 ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map([self.operation], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Value::Array(rows.into_iter().map(|(path,state,result,error)|json!({"path":path,"state":state,"result_artifact":result,"error":error})).collect()))
    }
    fn steps(
        &self,
        path: &str,
        steps: &[Step],
        outer: &Map<String, Value>,
    ) -> anyhow::Result<Progress> {
        let mut scope = outer.clone();
        let mut outputs = Map::new();
        let mut previous: Option<String> = None;
        for step in steps {
            let key = format!("{path}/{}", step.id);
            if let Some(previous) = &previous {
                self.tx.execute("INSERT OR IGNORE INTO workflow_dependencies(operation,node,dependency) VALUES(?1,?2,?3)",params![self.operation,key,previous])?;
            }
            match self.node(&key, &step.op, &scope)? {
                Progress::Pending => return Ok(Progress::Pending),
                Progress::Settled(outcome) => {
                    outputs.insert(step.id.clone(), outcome.output.clone());
                    scope.insert(step.id.clone(), outcome.output);
                    if let Some(error) = outcome.error {
                        return Ok(Progress::Settled(Outcome::failed(
                            Value::Object(outputs),
                            format!("step {}: {error}", step.id),
                        )));
                    }
                }
            }
            previous = Some(key);
        }
        Ok(Progress::Settled(Outcome::success(Value::Object(outputs))))
    }
    fn cached(&self, path: &str) -> anyhow::Result<Option<Outcome>> {
        let result: Option<String> = self
            .tx
            .query_row(
                "SELECT result FROM workflow_nodes WHERE operation=?1 AND path=?2",
                params![self.operation, path],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        result
            .map(|id| decode(full_artifact(self.tx, &ArtifactId(id))?))
            .transpose()
    }
    fn save(&self, path: &str, mut outcome: Outcome) -> anyhow::Result<Progress> {
        let text = encode(&outcome)?;
        if text.len() > MAX_ARTIFACT {
            outcome = Outcome::failed(
                Value::Null,
                "Node output exceeds 4 MiB; full child outcomes remain in child artifacts.",
            );
        }
        let result = artifact(self.tx, &encode(&outcome)?)?;
        self.tx.execute(
            "UPDATE workflow_nodes SET state=?3,result=?4,error=?5 WHERE operation=?1 AND path=?2",
            params![
                self.operation,
                path,
                if outcome.error.is_some() {
                    "failed"
                } else {
                    "completed"
                },
                result,
                outcome.error
            ],
        )?;
        change(self.tx, Some(self.thread), "workflow_node_settled", path)?;
        Ok(Progress::Settled(outcome))
    }
    fn node(&self, path: &str, op: &Op, scope: &Map<String, Value>) -> anyhow::Result<Progress> {
        if let Some(outcome) = self.cached(path)? {
            return Ok(Progress::Settled(outcome));
        }
        self.tx.execute("INSERT OR IGNORE INTO workflow_nodes(operation,path,state,specification) VALUES(?1,?2,'pending',?3)",params![self.operation,path,encode(op)?])?;
        if let Err(error) = dsl::preflight(op, scope) {
            return self.save(path, Outcome::failed(Value::Null, error));
        }
        let progress = match op {
            Op::Agent(agent) => {
                let prompt = match dsl::interpolate(&agent.prompt, scope, None) {
                    Ok(prompt) => prompt,
                    Err(error) => return self.save(path, Outcome::failed(Value::Null, error)),
                };
                self.agent(path, &prompt, agent.schema.as_ref())?
            }
            Op::FanOut(fan) => {
                let over = match fan.over.as_str() {
                    Some(reference) => match dsl::resolve_ref(reference, scope) {
                        Ok(v) => v,
                        Err(error) => return self.save(path, Outcome::failed(Value::Null, error)),
                    },
                    None => &fan.over,
                };
                let Some(items) = over.as_array() else {
                    return self.save(
                        path,
                        Outcome::failed(Value::Null, "fan-out over must resolve to an array"),
                    );
                };
                if let Err(error) = validation::validate_items(&fan.prompt, items) {
                    return self.save(path, Outcome::failed(Value::Null, error));
                }
                let repeat = fan.repeat.unwrap_or(1);
                let mut prompts = Vec::new();
                for item in items {
                    match dsl::interpolate(&fan.prompt, scope, Some(item)) {
                        Ok(prompt) => prompts.push(prompt),
                        Err(error) => return self.save(path, Outcome::failed(Value::Null, error)),
                    }
                }
                let mut pending = false;
                let mut failed = false;
                let mut outputs = Vec::new();
                for (i, prompt) in prompts.into_iter().enumerate() {
                    let mut row = Vec::new();
                    for replica in 0..repeat {
                        match self.leaf(
                            &format!("{path}/{i}/{replica}"),
                            &prompt,
                            fan.schema.as_ref(),
                        )? {
                            Progress::Pending => {
                                pending = true;
                                row.push(Value::Null);
                            }
                            Progress::Settled(outcome) => {
                                failed |= outcome.error.is_some();
                                row.push(outcome.output);
                            }
                        }
                    }
                    outputs.push(if repeat == 1 {
                        row.remove(0)
                    } else {
                        Value::Array(row)
                    });
                }
                if pending {
                    Progress::Pending
                } else {
                    Progress::Settled(Outcome {
                        output: Value::Array(outputs),
                        error: failed
                            .then(|| "fan-out incomplete; all child reports retained".into()),
                    })
                }
            }
            Op::Parallel(parallel) => {
                let mut outputs = Map::new();
                let mut pending = false;
                let mut errors = Vec::new();
                for (name, branch) in &parallel.branches {
                    match self.node(&format!("{path}/{name}"), branch, scope)? {
                        Progress::Pending => pending = true,
                        Progress::Settled(outcome) => {
                            outputs.insert(name.clone(), outcome.output);
                            if let Some(error) = outcome.error {
                                errors.push(format!("{name}: {error}"));
                            }
                        }
                    }
                }
                if pending {
                    Progress::Pending
                } else {
                    Progress::Settled(Outcome {
                        output: Value::Object(outputs),
                        error: (!errors.is_empty()).then(|| errors.join("; ")),
                    })
                }
            }
            Op::Conditional(conditional) => self.conditional(path, conditional, scope)?,
            Op::Loop(loop_op) => {
                let iteration: usize = self.tx.query_row(
                    "SELECT iteration FROM workflow_nodes WHERE operation=?1 AND path=?2",
                    params![self.operation, path],
                    |r| r.get(0),
                )?;
                match self.steps(&format!("{path}/pass{iteration}"), &loop_op.steps, scope)? {
                    Progress::Pending => Progress::Pending,
                    Progress::Settled(mut outcome) => {
                        let mut local = outcome.output.as_object().cloned().unwrap_or_default();
                        let mut until_scope = scope.clone();
                        until_scope.extend(local.clone());
                        let condition = if let Some(until) = &loop_op.until {
                            dsl::resolve_ref(until, &until_scope).and_then(|v| {
                                v.as_bool()
                                    .ok_or_else(|| "loop condition must be a boolean".into())
                            })
                        } else {
                            Ok(false)
                        };
                        let condition = match condition {
                            Ok(value) => value,
                            Err(error) => {
                                outcome.error = Some(error);
                                false
                            }
                        };
                        let stop = condition
                            || iteration + 1 >= loop_op.max.unwrap_or(5)
                            || outcome.error.is_some();
                        local.insert("_iterations".into(), json!(iteration + 1));
                        local.insert(
                            "_stop_reason".into(),
                            json!(if outcome.error.is_some() {
                                "failed"
                            } else if condition {
                                "condition_met"
                            } else {
                                "max_rounds"
                            }),
                        );
                        outcome.output = Value::Object(local);
                        let result = artifact(self.tx, &encode(&outcome)?)?;
                        self.tx.execute("INSERT OR IGNORE INTO workflow_iterations(operation,node,iteration,result) VALUES(?1,?2,?3,?4)",params![self.operation,path,iteration,result])?;
                        if stop {
                            Progress::Settled(outcome)
                        } else {
                            self.tx.execute("UPDATE workflow_nodes SET iteration=iteration+1 WHERE operation=?1 AND path=?2",params![self.operation,path])?;
                            Progress::Pending
                        }
                    }
                }
            }
        };
        match progress {
            Progress::Pending => Ok(Progress::Pending),
            Progress::Settled(outcome) => self.save(path, outcome),
        }
    }
    fn conditional(
        &self,
        path: &str,
        conditional: &dsl::ConditionalOp,
        scope: &Map<String, Value>,
    ) -> anyhow::Result<Progress> {
        let choice_path = format!("{path}/!choice");
        let condition = if let Some(choice) = self.cached(&choice_path)? {
            choice.output["condition"]
                .as_bool()
                .context("persisted conditional choice is not a boolean")?
        } else {
            let condition =
                match dsl::resolve_ref(&conditional.condition, scope).and_then(|value| {
                    value
                        .as_bool()
                        .ok_or_else(|| "conditional condition must be a boolean".into())
                }) {
                    Ok(condition) => condition,
                    Err(error) => {
                        return Ok(Progress::Settled(Outcome::failed(Value::Null, error)))
                    }
                };
            self.tx.execute(
                "INSERT INTO workflow_nodes(operation,path,state,specification) VALUES(?1,?2,'pending',?3)",
                params![self.operation, choice_path, encode(&json!({"conditional_choice":{"if":conditional.condition}}))?],
            )?;
            self.save(
                &choice_path,
                Outcome::success(
                    json!({"condition":condition,"branch":if condition {"then"} else {"else"}}),
                ),
            )?;
            condition
        };
        let branch = if condition { "then" } else { "else" };
        let steps = if condition {
            conditional.then.as_slice()
        } else {
            conditional.otherwise.as_deref().unwrap_or_default()
        };
        for step in steps {
            if let Err(error) = dsl::preflight(&step.op, scope) {
                return Ok(Progress::Settled(Outcome::failed(
                    json!({"branch":branch,"output":{}}),
                    format!("step '{}': {error}", step.id),
                )));
            }
        }
        match self.steps(&format!("{path}/{branch}"), steps, scope)? {
            Progress::Pending => Ok(Progress::Pending),
            Progress::Settled(outcome) => Ok(Progress::Settled(Outcome {
                output: json!({"branch":branch,"output":outcome.output}),
                error: outcome.error,
            })),
        }
    }
    fn leaf(&self, path: &str, prompt: &str, schema: Option<&Value>) -> anyhow::Result<Progress> {
        if let Some(outcome) = self.cached(path)? {
            return Ok(Progress::Settled(outcome));
        }
        self.tx.execute("INSERT OR IGNORE INTO workflow_nodes(operation,path,state,specification) VALUES(?1,?2,'pending',?3)",params![self.operation,path,encode(&json!({"prompt":prompt,"schema":schema}))?])?;
        match self.agent(path, prompt, schema)? {
            Progress::Pending => Ok(Progress::Pending),
            Progress::Settled(outcome) => self.save(path, outcome),
        }
    }
    fn agent(&self, path: &str, prompt: &str, schema: Option<&Value>) -> anyhow::Result<Progress> {
        let child: Option<String> = self.tx.query_row(
            "SELECT child FROM workflow_nodes WHERE operation=?1 AND path=?2",
            params![self.operation, path],
            |r| r.get(0),
        )?;
        if let Some(child) = child {
            let (state, result, stopping): (String, Option<String>, bool) = self.tx.query_row(
                "SELECT a.state,a.outcome,EXISTS(SELECT 1 FROM runs r WHERE r.thread=a.thread AND r.state='stopping') FROM agents a WHERE a.thread=?1",
                [&child],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            if stopping || matches!(state.as_str(), "queued" | "running" | "stopping") {
                return Ok(Progress::Pending);
            }
            let text = result
                .map(|id| full_artifact(self.tx, &ArtifactId(id)))
                .transpose()?
                .unwrap_or_default();
            let output = if state == "completed" {
                if schema.is_some() {
                    serde_json::from_str(&text)?
                } else {
                    Value::String(text)
                }
            } else {
                Value::Null
            };
            return Ok(Progress::Settled(Outcome {
                output,
                error: (state != "completed")
                    .then(|| format!("child {child} {state}; inspect its retained report")),
            }));
        }
        if self.occupancy(Some(self.operation))? >= self.max_concurrency
            || self.occupancy(None)? >= 64
        {
            self.tx.execute(
                "UPDATE workflow_nodes SET state='waiting_capacity' WHERE operation=?1 AND path=?2 AND state!='waiting_capacity'",
                params![self.operation, path],
            )?;
            return Ok(Progress::Pending);
        }
        let count: usize = self.tx.query_row(
            "SELECT executions FROM workflows WHERE operation=?1",
            [self.operation],
            |r| r.get(0),
        )?;
        if count >= 1024 {
            return Ok(Progress::Settled(Outcome::failed(
                Value::Null,
                "workflow exceeds 1024 child executions",
            )));
        }
        if prompt.len() > 256 * 1024 {
            return Ok(Progress::Settled(Outcome::failed(
                Value::Null,
                "expanded prompt exceeds 256 KiB; no child launched",
            )));
        }
        let child = delegation::launch(
            self.tx,
            self.operation,
            self.run,
            self.thread,
            &self.profile,
            self.models,
            Child {
                name: &format!("workflow-{}", count + 1),
                prompt,
                role: "workflow",
                subagent_type: None,
                model: None,
                read_only: false,
                blocking: false,
                schema,
            },
        )?;
        self.tx.execute(
            "UPDATE workflow_nodes SET child=?3,state='running' WHERE operation=?1 AND path=?2",
            params![self.operation, path, child],
        )?;
        self.tx.execute(
            "UPDATE workflows SET executions=executions+1 WHERE operation=?1",
            [self.operation],
        )?;
        Ok(Progress::Pending)
    }
    fn occupancy(&self, operation: Option<&OperationId>) -> anyhow::Result<usize> {
        let session = thread_session(self.tx, self.thread)?;
        Ok(self.tx.query_row(
            "SELECT COUNT(*) FROM agents a JOIN threads t ON t.id=a.thread WHERE t.session=?2 AND (?1 IS NULL OR a.operation=?1) AND (a.state IN ('queued','running','stopping') OR EXISTS(SELECT 1 FROM runs r WHERE r.thread=a.thread AND r.state='stopping'))",
            params![operation, session],
            |r| r.get(0),
        )?)
    }
    fn shape(&self, params: params::WorkflowParams) -> anyhow::Result<Progress> {
        if params.shape == params::Shape::Loop {
            return self.finder(&params);
        }
        let mut results = Vec::new();
        let mut outcomes = Vec::new();
        let mut pending = false;
        let mut failed = false;
        for (i, item) in params.items.iter().enumerate() {
            let prompt = params.map_prompt.replace("{item}", item);
            match self.leaf(&format!("map/{i}"), &prompt, params.map_schema.as_ref())? {
                Progress::Pending => {
                    pending = true;
                }
                Progress::Settled(outcome) => {
                    failed |= outcome.error.is_some();
                    results.push(outcome.output.clone());
                    outcomes.push(json!({"input":item,"status":if outcome.error.is_some(){"failed"}else{"success"},"output":outcome.output,"error":outcome.error}));
                }
            }
        }
        if pending {
            return Ok(Progress::Pending);
        }
        let mut output = json!({"results":results,"outcomes":outcomes});
        if failed {
            return Ok(Progress::Settled(Outcome::failed(
                output,
                "map phase incomplete; dependent reduction skipped",
            )));
        }
        if params.shape == params::Shape::MapReduce {
            let prompt = format!(
                "{}\n\nResults:\n{}",
                params
                    .reduce_prompt
                    .as_deref()
                    .unwrap_or("Synthesize these results into a single summary."),
                encode(&results)?
            );
            self.tx.execute("INSERT OR IGNORE INTO workflow_dependencies(operation,node,dependency) VALUES(?1,'reduce','map')",[self.operation])?;
            match self.leaf("reduce", &prompt, params.reduce_schema.as_ref())? {
                Progress::Pending => return Ok(Progress::Pending),
                Progress::Settled(outcome) => {
                    output["reduced"] = outcome.output;
                    return Ok(Progress::Settled(Outcome {
                        output,
                        error: outcome.error,
                    }));
                }
            }
        }
        Ok(Progress::Settled(Outcome::success(output)))
    }
    fn finder(&self, params: &params::WorkflowParams) -> anyhow::Result<Progress> {
        let mut seen = Vec::<String>::new();
        for round in 0..params.max_rounds.unwrap_or(5) {
            let schema = json!({"type":"object","required":["findings"],"properties":{"findings":{"type":"array","items":{"type":"string"}}}});
            let prompt=format!("{}\n\nAlready found (do NOT repeat):\n{}\nReport findings as an array of strings; empty if nothing new.",params.find_prompt.as_deref().unwrap_or("Find new items"),encode(&seen)?);
            match self.leaf(&format!("find/pass{round}"), &prompt, Some(&schema))? {
                Progress::Pending => return Ok(Progress::Pending),
                Progress::Settled(outcome) => {
                    if let Some(error) = outcome.error {
                        return Ok(Progress::Settled(Outcome::failed(
                            json!({"findings":seen,"rounds":round+1,"termination_reason":"failed"}),
                            error,
                        )));
                    }
                    let findings = outcome.output["findings"]
                        .as_array()
                        .context("validated findings missing")?;
                    let previous = seen.len();
                    for finding in findings {
                        let finding = finding
                            .as_str()
                            .context("validated finding is not a string")?;
                        if !seen.iter().any(|s| s == finding) {
                            seen.push(finding.into());
                        }
                    }
                    self.tx.execute("INSERT OR IGNORE INTO workflow_iterations(operation,node,iteration,result) SELECT ?1,'find',?2,result FROM workflow_nodes WHERE operation=?1 AND path=?3",params![self.operation,round,format!("find/pass{round}")])?;
                    if seen.len() == previous {
                        return Ok(Progress::Settled(Outcome::success(
                            json!({"findings":seen,"rounds":round+1,"termination_reason":"dry"}),
                        )));
                    }
                }
            }
        }
        Ok(Progress::Settled(Outcome::success(
            json!({"findings":seen,"rounds":params.max_rounds.unwrap_or(5),"termination_reason":"max_rounds"}),
        )))
    }
}
