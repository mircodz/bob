use super::*;

fn check_cancel(cancel: Option<&watch::Receiver<bool>>) -> Result<()> {
    ensure!(
        !cancel.is_some_and(cancellation_requested),
        "write plan cancelled"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedWrites {
    root: PathBuf,
    root_identity: Value,
    files: Vec<PreparedWrite>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedWrite {
    path: PathBuf,
    snapshot: Snapshot,
    before: String,
    after: String,
    staged: PathBuf,
    backup: PathBuf,
}

impl PreparedWrites {
    pub(crate) const MAX_FILES: usize = 256;
    pub(crate) const MAX_BYTES: usize = 32 * 1024 * 1024;

    pub(crate) fn check_budget(count: usize, bytes: usize) -> Result<()> {
        ensure!(
            count <= Self::MAX_FILES,
            "write plan exceeds {} files",
            Self::MAX_FILES
        );
        ensure!(
            bytes <= Self::MAX_BYTES,
            "write plan exceeds {} aggregate bytes",
            Self::MAX_BYTES
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn prepare(root: &Path, changes: Vec<(PathBuf, String, String)>) -> Result<Self> {
        Self::prepare_cancellable(root, changes, None)
    }

    pub(crate) fn prepare_cancellable(
        root: &Path,
        changes: Vec<(PathBuf, String, String)>,
        cancel: Option<&watch::Receiver<bool>>,
    ) -> Result<Self> {
        let mut bytes = 0usize;
        for (_, before, after) in &changes {
            bytes = bytes
                .saturating_add(before.len())
                .saturating_add(after.len());
            Self::check_budget(changes.len(), bytes)?;
        }
        check_cancel(cancel)?;
        let root = root_path(root.to_str().context("non-Unicode workspace")?)?;
        let root_identity = path_identity(&root)?;
        ensure!(!changes.is_empty(), "write plan is empty");
        let mut files: Vec<PreparedWrite> = Vec::new();
        for (requested, before, after) in changes {
            check_cancel(cancel)?;
            ensure!(
                after.len() <= READ_LIMIT,
                "replacement exceeds {READ_LIMIT} bytes"
            );
            let path = workspace_file(&root, requested.to_str().context("non-Unicode target")?)?;
            ensure!(
                !files.iter().any(|file| file.path == path),
                "duplicate write target"
            );
            let (current, snapshot) = read_snapshot(&path)?;
            ensure!(
                current == before,
                "write target changed before preparation: {}",
                path.display()
            );
            let parent = path.parent().context("missing parent")?;
            let token = uuid::Uuid::new_v4();
            let staged = parent.join(format!(".bob-{token}.staged"));
            let backup = parent.join(format!(".bob-{token}.backup"));
            files.push(PreparedWrite {
                path,
                snapshot,
                before,
                after,
                staged,
                backup,
            });
        }
        let plan = Self {
            root,
            root_identity,
            files,
        };
        plan.validate(cancel)?;
        Ok(plan)
    }

    fn validate_file(&self, file: &PreparedWrite) -> Result<()> {
        ensure!(
            path_identity(&self.root)? == self.root_identity,
            "write plan root changed"
        );
        ensure!(
            workspace_file(
                &self.root,
                file.path.to_str().context("non-Unicode target")?
            )? == file.path,
            "write plan target changed"
        );
        ensure!(
            file.staged.parent() == file.path.parent()
                && file.backup.parent() == file.path.parent()
                && file.staged != file.backup
                && file.staged != file.path
                && file.backup != file.path,
            "invalid recovery paths"
        );
        let (current, snapshot) = read_snapshot(&file.path)?;
        ensure!(
            current == file.before && snapshot == file.snapshot,
            "write plan target changed: {}",
            file.path.display()
        );
        ensure!(
            file.after.len() <= READ_LIMIT,
            "write plan result exceeds file limit"
        );
        Ok(())
    }

    pub(crate) fn validate_structure(&self) -> Result<()> {
        ensure!(!self.files.is_empty(), "write plan is empty");
        let mut bytes = 0usize;
        for (index, file) in self.files.iter().enumerate() {
            bytes = bytes
                .saturating_add(file.before.len())
                .saturating_add(file.after.len());
            Self::check_budget(self.files.len(), bytes)?;
            ensure!(
                file.before.len() <= READ_LIMIT && file.after.len() <= READ_LIMIT,
                "write plan exceeds file limit"
            );
            ensure!(
                !self.files[..index]
                    .iter()
                    .any(|prior| prior.path == file.path),
                "duplicate write target"
            );
        }
        Ok(())
    }

    fn validate(&self, cancel: Option<&watch::Receiver<bool>>) -> Result<()> {
        check_cancel(cancel)?;
        self.validate_structure()?;
        for file in &self.files {
            check_cancel(cancel)?;
            self.validate_file(file)?;
        }
        check_cancel(cancel)
    }

    pub(crate) fn targets(&self) -> Vec<PathBuf> {
        self.files.iter().map(|file| file.path.clone()).collect()
    }

    pub(crate) fn validate_workspace_binding(&self, workspace: &Path) -> Result<()> {
        self.validate_structure()?;
        ensure!(
            workspace == self.root,
            "write plan belongs to another workspace"
        );
        ensure!(
            self.files
                .iter()
                .all(|file| file.path.is_absolute() && file.path.starts_with(workspace)),
            "write plan target is outside workspace"
        );
        Ok(())
    }

    fn validate_workspace_identity(&self, workspace: &Path) -> Result<()> {
        ensure!(
            root_path(workspace.to_str().context("non-Unicode workspace")?)? == self.root,
            "write plan belongs to another workspace"
        );
        Ok(())
    }

    pub(crate) fn preview(&self) -> String {
        let mut output = String::new();
        for file in &self.files {
            output.push_str(&format!(
                "{}\nBefore sha256: {}\nAfter sha256: {}\n",
                file.path.display(),
                file.snapshot.sha256,
                digest(file.after.as_bytes())
            ));
            if output.len() < 256 * 1024 {
                output.push_str(&presentation::diff_preview(
                    &file.before,
                    &file.after,
                    (256 * 1024 - output.len()).min(32 * 1024),
                ));
            } else {
                output.push_str(
                    "[Diff omitted from preview; exact content retained in prepared action]\n",
                );
            }
        }
        output
    }

    pub(crate) fn execute(&self, cancel: &watch::Receiver<bool>) -> Result<ToolOutcome> {
        check_cancel(Some(cancel))?;
        self.validate_structure()?;
        self.validate_workspace_identity(&self.root)?;
        self.validate(Some(cancel))?;
        let mut completed = Vec::new();
        for file in &self.files {
            if cancellation_requested(cancel) {
                return Ok(ToolOutcome {
                    text: format!(
                        "Cancelled; published files: {}. Remaining files unchanged.",
                        completed.join(", ")
                    ),
                    is_error: true,
                    interrupted: false,
                    presentation: None,
                });
            }
            let result = guarded_replace(
                &file.path,
                &file.staged,
                &file.backup,
                (&file.before, &file.snapshot),
                &file.after,
                || {
                    check_cancel(Some(cancel))?;
                    self.validate_file(file)
                },
                || cancellation_requested(cancel),
            );
            match result {
                Ok(outcome) if !outcome.is_error => completed.push(file.path.display().to_string()),
                Ok(mut outcome) => {
                    outcome.text = format!(
                        "{}\nPreviously published files: {}. No rollback attempted.",
                        outcome.text,
                        completed.join(", ")
                    );
                    return Ok(outcome);
                }
                Err(error) => {
                    return Ok(ToolOutcome {
                        text: format!(
                            "{error:#}\nPreviously published files: {}. No rollback attempted.",
                            completed.join(", ")
                        ),
                        is_error: true,
                        interrupted: false,
                        presentation: None,
                    })
                }
            }
        }
        Ok(ToolOutcome {
            text: format!(
                "Edited {} files:\n{}",
                completed.len(),
                completed.join("\n")
            ),
            is_error: false,
            interrupted: false,
            presentation: Some(bob_kernel::ToolResultPresentation {
                summary: Some(format!("Edited {} files", completed.len())),
                body_preview: Some(completed.join("\n")),
                exit_code: None,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_file(path: PathBuf, before: String, after: String) -> PreparedWrite {
        PreparedWrite {
            staged: path.with_extension("staged"),
            backup: path.with_extension("backup"),
            path,
            snapshot: Snapshot {
                identity: Value::Null,
                modified: Value::Null,
                ancestors: Value::Null,
                len: 0,
                sha256: String::new(),
            },
            before,
            after,
        }
    }

    #[test]
    fn preparation_and_deserialized_validation_enforce_plan_budgets_before_io() {
        let root = std::env::temp_dir().join(format!("bob-missing-{}", uuid::Uuid::new_v4()));
        let changes = (0..=PreparedWrites::MAX_FILES)
            .map(|i| (root.join(i.to_string()), String::new(), String::new()))
            .collect();
        assert!(PreparedWrites::prepare(&root, changes)
            .unwrap_err()
            .to_string()
            .contains("files"));
        let files = (0..=PreparedWrites::MAX_FILES)
            .map(|i| synthetic_file(root.join(i.to_string()), String::new(), String::new()))
            .collect();
        let mut plan = PreparedWrites {
            root: root.clone(),
            root_identity: Value::Null,
            files,
        };
        let serialized = serde_json::to_value(&plan).unwrap();
        let decoded: PreparedWrites = serde_json::from_value(serialized).unwrap();
        assert!(decoded
            .validate(None)
            .unwrap_err()
            .to_string()
            .contains("files"));
        plan.files = (0..3)
            .map(|i| {
                synthetic_file(
                    root.join(i.to_string()),
                    "a".repeat(12 * 1024 * 1024),
                    String::new(),
                )
            })
            .collect();
        assert!(plan
            .validate(None)
            .unwrap_err()
            .to_string()
            .contains("aggregate bytes"));
        let changes = plan
            .files
            .into_iter()
            .map(|file| (file.path, file.before, file.after))
            .collect();
        assert!(PreparedWrites::prepare(&root, changes)
            .unwrap_err()
            .to_string()
            .contains("aggregate bytes"));
        assert!(!root.exists());
    }

    #[test]
    fn cancellation_is_checked_before_snapshot_validation_not_only_publication() {
        let root = std::env::temp_dir().join(format!("bob-missing-{}", uuid::Uuid::new_v4()));
        let plan = PreparedWrites {
            root: root.clone(),
            root_identity: Value::Null,
            files: vec![synthetic_file(
                root.join("missing.rs"),
                "old".into(),
                "new".into(),
            )],
        };
        let (sender, cancel) = watch::channel(true);
        assert!(plan
            .validate(Some(&cancel))
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(
            PreparedWrites::prepare_cancellable(&root, Vec::new(), Some(&cancel))
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        drop(sender);
        assert!(plan
            .validate(Some(&cancel))
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(!root.exists());
    }
}
