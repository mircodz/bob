use super::*;

impl Actor {
    pub(super) fn authorize_control(
        &mut self,
        op: &Op,
        workspace: &str,
        revision: i64,
        decision: RuleDecision,
    ) -> anyhow::Result<bool> {
        if (op.state == "waiting" && matches!(op.name.as_str(), "ask_user" | "exit_plan"))
            || decision == RuleDecision::Allow
        {
            return Ok(true);
        }
        let approved: bool = self.store.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM decisions WHERE operation=?1 AND revision=?2 AND state='approved')",
            params![op.id, revision], |r| r.get(0),
        )?;
        if matches!(op.state.as_str(), "ready" | "waiting") && op.revision == revision && approved {
            return Ok(true);
        }
        let prepared = PreparedTool {
            invocation: Invocation {
                operation_id: op.id.clone(),
                run_id: op.run.clone(),
                thread_id: op.thread.clone(),
                workspace: workspace.into(),
                name: op.name.clone(),
                input: op.input.clone(),
            },
            specification: Value::Null,
            preview: format!("{} {}", op.name, preview(&encode(&op.input)?)),
            presentation: None,
        };
        let options = crate::approvals::choices(&prepared, decision == RuleDecision::Ask);
        let tx = self
            .store
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("UPDATE operations SET state='approval',prepared=?2,policy_revision=?3,profile_revision=(SELECT profile_revision FROM threads WHERE id=operations.thread) WHERE id=?1", params![op.id, encode(&prepared)?, revision])?;
        tx.execute("INSERT INTO decisions(operation,revision,preview,state,options) VALUES(?1,?2,?3,'pending',?4) ON CONFLICT(operation) DO UPDATE SET revision=excluded.revision,preview=excluded.preview,state='pending',options=excluded.options", params![op.id, revision, prepared.preview, encode(&options)?])?;
        change(&tx, Some(&op.thread), "authorization_changed", &op.id.0)?;
        tx.commit()?;
        Ok(false)
    }

    pub(super) fn prepared_authorization(
        &self,
        profile: &Profile,
        policy: &Policy,
        permissions: &PermissionPolicy,
        prepared: &PreparedTool,
        caps: &ToolCapabilities,
    ) -> anyhow::Result<(
        RuleDecision,
        Vec<PreparedAuthorizationRequirement>,
        Option<std::path::PathBuf>,
    )> {
        let invocation = &prepared.invocation;
        let target = self.executor.prepared_authorization_target(prepared)?;
        if let Some(path) = &target {
            let text = path
                .to_str()
                .context("primary authorization target is not UTF-8")?;
            ensure!(
                text.len() <= 32767
                    && path.is_absolute()
                    && crate::policy::grant_path("", text).as_ref() == Some(path),
                "primary authorization target must be an absolute normalized filesystem path"
            );
        }
        let shell = self.executor.prepared_shell_command(prepared)?;
        let confirmed = crate::policy::prepared_shell_permissions(
            permissions,
            crate::policy::ToolRequest {
                name: &invocation.name,
                input: &invocation.input,
                workspace: &invocation.workspace,
            },
            shell.as_ref(),
        )?;
        let permissions = confirmed.as_ref().unwrap_or(permissions);
        let fallback = self.executor.prepared_policy(prepared, permissions)?;
        let mut decision = crate::policy::evaluate_with_target(
            policy,
            permissions,
            profile,
            crate::policy::ToolRequest {
                name: &invocation.name,
                input: &invocation.input,
                workspace: &invocation.workspace,
            },
            caps,
            fallback,
            target.as_deref(),
        );
        let mut requirements = self
            .executor
            .prepared_authorization_requirements(prepared)?;
        ensure!(
            !requirements.iter().any(|requirement| matches!(
                requirement,
                PreparedAuthorizationRequirement::ToolPath { .. }
            )),
            "primary tool path must be supplied through prepared_authorization_target"
        );
        if let Some(path) = &target {
            requirements.push(PreparedAuthorizationRequirement::ToolPath { path: path.clone() });
        }
        ensure!(
            requirements.len() <= 256 && encode(&requirements)?.len() <= 64 * 1024,
            "prepared authorization targets exceed bounded retention"
        );
        for requirement in &requirements {
            match requirement {
                PreparedAuthorizationRequirement::ToolPath { .. } => {}
                PreparedAuthorizationRequirement::EditFile { path } => {
                    let text = path.to_str().context("authorization target is not UTF-8")?;
                    let target=crate::policy::grant_path(&invocation.workspace,text).context("authorization target is not workspace-anchored or contains parent traversal")?;
                    let external = !target.starts_with(&invocation.workspace);
                    let caps = ToolCapabilities {
                        mutates_workspace: true,
                        external_access: external,
                        requires_approval: external,
                        ..Default::default()
                    };
                    let required = crate::policy::evaluate_in_workspace(
                        policy,
                        permissions,
                        profile,
                        "edit_file",
                        &json!({"path":target}),
                        &caps,
                        &invocation.workspace,
                    );
                    decision = merge_decision(decision, required);
                }
            }
        }
        Ok((decision, requirements, target))
    }
}
fn merge_decision(a: RuleDecision, b: RuleDecision) -> RuleDecision {
    use RuleDecision::*;
    match (a, b) {
        (Deny, _) | (_, Deny) => Deny,
        (RequireApproval, _) | (_, RequireApproval) => RequireApproval,
        (Ask, _) | (_, Ask) => Ask,
        _ => Allow,
    }
}
