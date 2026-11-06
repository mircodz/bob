use super::*;

impl Actor {
    pub(super) fn scheduled_operation(&self, run: &RunId) -> anyhow::Result<Option<Op>> {
        let mut stmt=self.store.db.prepare("SELECT id,name,kind,state FROM operations WHERE run=?1 AND state IN ('pending','preparing','started','approval','ready','waiting') ORDER BY rowid LIMIT 32")?;
        let rows = stmt
            .query_map([run], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let Some(first) = rows.first() else {
            return Ok(None);
        };
        let mut selected = &first.0;
        if first.2 == "tool"
            && matches!(first.3.as_str(), "preparing" | "started")
            && self.independent_read(&first.1)
        {
            for row in rows.iter().skip(1) {
                if row.2 != "tool"
                    || !self.independent_read(&row.1)
                    || matches!(row.3.as_str(), "approval" | "waiting")
                {
                    break;
                }
                if matches!(row.3.as_str(), "pending" | "ready") {
                    selected = &row.0;
                    break;
                }
            }
        }
        let (thread,name,kind,input,state,prepared,revision)=self.store.db.query_row("SELECT thread,name,kind,input,state,prepared,policy_revision FROM operations WHERE id=?1",[selected],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,i64>(6)?)))?;
        Ok(Some(Op {
            id: OperationId(selected.clone()),
            run: run.clone(),
            thread: ThreadId(thread),
            name,
            kind,
            input: decode(input)?,
            state,
            prepared,
            revision,
        }))
    }
    fn independent_read(&self, name: &str) -> bool {
        if crate::policy::internal_capabilities(name).is_some() {
            return false;
        }
        let caps = self.executor.tool_capabilities(name);
        self.tools
            .get(name)
            .is_some_and(|t| t.effect == EffectClass::Read)
            && !caps.mutates_workspace
            && !caps.executes_process
            && !caps.mutates_state
    }
    #[cfg(test)]
    pub(super) fn execution_blocked(
        &self,
        operation: &OperationId,
        workspace: &str,
        write: bool,
    ) -> anyhow::Result<bool> {
        let session = self.store.db.query_row(
            "SELECT t.session FROM operations o JOIN threads t ON t.id=o.thread WHERE o.id=?1",
            [operation],
            |r| r.get::<_, String>(0),
        )?;
        execution_blocked(
            &self.store.db,
            operation,
            workspace,
            write,
            &SessionId(session),
        )
    }
}

pub(super) fn execution_blocked(
    db: &rusqlite::Connection,
    operation: &OperationId,
    workspace: &str,
    write: bool,
    session: &SessionId,
) -> anyhow::Result<bool> {
    // A foreground shell handed off to the background is a background job: it no longer blocks.
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN threads t ON t.id=o.thread JOIN sessions s ON s.id=t.session JOIN runs r ON r.id=o.run WHERE t.session=?4 AND s.workspace=?2 AND o.kind='tool' AND o.id!=?1 AND ((o.state='started' AND (?3 OR o.effect_write=1)) OR (o.state='ready' AND o.effect_write=1 AND r.state='active' AND t.held=0 AND o.rowid<(SELECT rowid FROM operations WHERE id=?1))))",params![operation,workspace,write,session],|r|r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    struct Fixture;
    #[async_trait]
    impl Executor for Fixture {
        fn models(&self) -> Vec<ModelInfo> {
            vec![]
        }
        fn tools(&self) -> Vec<ToolDescriptor> {
            ["read_file", "edit_file"]
                .into_iter()
                .map(|name| ToolDescriptor {
                    spec: ToolSpec {
                        name: name.into(),
                        description: String::new(),
                        input_schema: json!({"type":"object"}),
                    },
                    effect: if name == "read_file" {
                        EffectClass::Read
                    } else {
                        EffectClass::Write
                    },
                })
                .collect()
        }
        async fn generate(
            &self,
            _: RequestSpec,
            _: watch::Receiver<bool>,
            _: crate::ModelProgress,
        ) -> anyhow::Result<Completion> {
            unreachable!()
        }
        async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
            unreachable!()
        }
        async fn execute(
            &self,
            _: PreparedTool,
            _: watch::Receiver<bool>,
        ) -> anyhow::Result<ToolOutcome> {
            unreachable!()
        }
    }
    #[test]
    fn independent_reads_pass_each_other_but_never_cross_a_mutation() {
        let (_dir, store, root, _) = crate::store::tests::fixture();
        let run = RunId::new();
        store
            .db
            .execute(
                "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
                params![run, root.thread],
            )
            .unwrap();
        let actor = Actor::new(store, Arc::new(Fixture)).unwrap();
        let ids = [
            OperationId::new(),
            OperationId::new(),
            OperationId::new(),
            OperationId::new(),
        ];
        for (i, (name, state)) in [
            ("read_file", "started"),
            ("read_file", "pending"),
            ("edit_file", "ready"),
            ("read_file", "pending"),
        ]
        .into_iter()
        .enumerate()
        {
            actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state,effect_write) VALUES(?1,?2,?3,?4,'tool',?1,'{}',?5,?6)",params![ids[i],run,root.thread,name,state,name=="edit_file"]).unwrap();
        }
        assert_eq!(actor.scheduled_operation(&run).unwrap().unwrap().id, ids[1]);
        actor
            .store
            .db
            .execute("UPDATE operations SET state='done' WHERE id=?1", [&ids[1]])
            .unwrap();
        assert_eq!(actor.scheduled_operation(&run).unwrap().unwrap().id, ids[0]);
        actor
            .store
            .db
            .execute("UPDATE operations SET state='done' WHERE id=?1", [&ids[0]])
            .unwrap();
        assert_eq!(actor.scheduled_operation(&run).unwrap().unwrap().id, ids[2]);
        let workspace: String = actor
            .store
            .db
            .query_row(
                "SELECT workspace FROM sessions WHERE id=?1",
                [root.session],
                |r| r.get(0),
            )
            .unwrap();
        assert!(actor.execution_blocked(&ids[3], &workspace, false).unwrap());
    }
}
