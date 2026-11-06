use super::*;

impl Actor {
    fn command_session(&self, command: &Command) -> anyhow::Result<Option<SessionId>> {
        let db = &self.store.db;
        let thread = match command {
            Command::CreateSession { .. } | Command::CreateSessionConfigured { .. } => {
                return Ok(None)
            }
            Command::SetPolicy { session, .. } | Command::SetPermissions { session, .. } => {
                return Ok(Some(session.clone()))
            }
            Command::Submit { thread, .. }
            | Command::StopThread { thread, .. }
            | Command::Resume { thread, .. }
            | Command::UpdateProfile { thread, .. }
            | Command::ClearContext { thread, .. }
            | Command::Compact { thread, .. }
            | Command::Steer { thread, .. }
            | Command::Background { thread, .. } => thread.clone(),
            Command::RecallSubmission { submission, .. } => ThreadId(db.query_row(
                "SELECT thread FROM submissions WHERE id=?1",
                [submission],
                |r| r.get(0),
            )?),
            Command::StopJob { job, .. } => {
                ThreadId(db.query_row("SELECT thread FROM jobs WHERE id=?1", [job], |r| r.get(0))?)
            }
            Command::Stop { run, .. } => {
                ThreadId(db.query_row("SELECT thread FROM runs WHERE id=?1", [run], |r| r.get(0))?)
            }
            Command::Decide { operation, .. }
            | Command::DecideWithGrant { operation, .. }
            | Command::AnswerQuestion { operation, .. }
            | Command::DecidePlan { operation, .. }
            | Command::ResolveUnknown { operation, .. } => ThreadId(db.query_row(
                "SELECT thread FROM operations WHERE id=?1",
                [operation],
                |r| r.get(0),
            )?),
        };
        Ok(Some(thread_session(db, &thread)?))
    }

    pub(super) fn accept_command(&mut self, command: &Command) -> anyhow::Result<Receipt> {
        if matches!(
            command,
            Command::CreateSession { .. } | Command::CreateSessionConfigured { .. }
        ) {
            let existing: Option<String> = self
                .store
                .db
                .query_row(
                    "SELECT receipt FROM commands WHERE id=?1",
                    [command.id()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(raw) = existing {
                let receipt: Receipt = decode(raw)?;
                if self.selected_session.as_ref() != Some(&receipt.session) {
                    self.claim_target(&receipt.session)?;
                }
                self.store.ensure_owner()?;
                return self.store_command(command);
            }
            self.check_switch(None)?;
            let session = SessionId::new();
            let path = self
                .store
                .path
                .as_deref()
                .context("runtime store has no filesystem identity")?;
            let claim = crate::SessionClaim::acquire_initialized(path, session.clone())?;
            let receipt = self.store.command_with_session(
                command,
                &self.models,
                self.options.auto_titles,
                Some(&session),
            )?;
            if receipt.session == session {
                self.store.adopt_claim(claim, false)?;
                self.selected_session = Some(session);
                self.published_session_sequence = -1;
            } else {
                drop(claim);
                self.claim_target(&receipt.session)?;
            }
            return Ok(receipt);
        }
        let selected = self.selected_session.as_ref().context(
            "runtime has no claimed session; activate a session before issuing commands",
        )?;
        ensure!(
            self.command_session(command)?.as_ref() == Some(selected),
            "command targets an inactive session"
        );
        self.store.ensure_owner()?;
        self.store_command(command)
    }

    fn check_switch(&self, target: Option<&SessionId>) -> anyhow::Result<()> {
        ensure!(
            self.workers.is_empty() && self.title_worker.is_none(),
            "session switch requires all execution workers to settle"
        );
        if let Some(selected) = &self.selected_session {
            self.store.ensure_owner()?;
            let busy: bool = self.store.db.query_row("SELECT EXISTS(SELECT 1 FROM runs r JOIN threads t ON t.id=r.thread WHERE t.session=?1 AND r.state IN ('active','stopping')) OR EXISTS(SELECT 1 FROM jobs WHERE session=?1 AND state IN ('queued','running','stopping')) OR EXISTS(SELECT 1 FROM submissions s JOIN threads t ON t.id=s.thread WHERE t.session=?1 AND t.held=0 AND s.state='queued')", [selected], |r| r.get(0))?;
            ensure!(
                !busy || (self.paused && target == Some(selected)),
                "session switch requires the selected session to be quiescent"
            );
        }
        Ok(())
    }

    fn claim_target(&mut self, session: &SessionId) -> anyhow::Result<()> {
        if self.selected_session.as_ref() == Some(session) {
            return self.store.ensure_owner();
        }
        self.check_switch(Some(session))?;
        let path = self
            .store
            .path
            .as_deref()
            .context("runtime store has no filesystem identity")?;
        let claim = crate::SessionClaim::acquire_initialized(path, session.clone())?;
        let snapshot = crate::sessions::session(&self.store.db, session)?;
        if let Some(selected) = self.selected_session.as_ref() {
            let current = crate::sessions::session(&self.store.db, selected)?;
            ensure!(
                current.workspace == snapshot.workspace,
                "switching workspaces requires reopening with workspace-bound services"
            );
        }
        self.store.adopt_claim(claim, true)?;
        self.selected_session = Some(session.clone());
        self.published_session_sequence = -1;
        Ok(())
    }

    fn store_command(&mut self, command: &Command) -> anyhow::Result<Receipt> {
        if self.options.auto_titles {
            self.store.command_with_titles(command, &self.models, true)
        } else {
            self.store.command(command, &self.models)
        }
    }

    pub(super) fn activate_session(
        &mut self,
        session: SessionId,
        bootstrap: Vec<Command>,
    ) -> anyhow::Result<Vec<Receipt>> {
        self.check_switch(Some(&session))?;
        self.paused = true;
        ensure!(
            self.error.is_none(),
            "kernel is halted after storage/runtime failure"
        );
        ensure!(bootstrap.len() <= 128, "bootstrap exceeds 128 commands");
        let snapshot = crate::sessions::session(&self.store.db, &session)?;
        if let Some(selected) = &self.selected_session {
            let current = crate::sessions::session(&self.store.db, selected)?;
            ensure!(
                current.workspace == snapshot.workspace,
                "switching workspaces requires reopening with workspace-bound services"
            );
        }
        for command in &bootstrap {
            ensure!(
                self.command_session(command)?.as_ref() == Some(&session),
                "bootstrap command must target the selected session"
            );
        }
        self.claim_target(&session)?;
        let mut receipts = Vec::with_capacity(bootstrap.len());
        for command in bootstrap {
            receipts.push(self.store_command(&command)?);
        }
        let snapshot = crate::sessions::session(&self.store.db, &session)?;
        validate_profile(&snapshot.profile, &self.models)?;
        self.selected_session = Some(session);
        self.cancel_titles(false)?;
        self.run_cursor = 0;
        self.paused = false;
        Ok(receipts)
    }
}
