use crate::contracts::*;
use crate::model::*;
use crate::store::decode;
use anyhow::{ensure, Context};
use rusqlite::{params, Connection, OptionalExtension};

pub(crate) fn choices(prepared: &PreparedTool, ordinary: bool) -> Vec<ApprovalOption> {
    choices_with_target(prepared, ordinary, None)
}

pub(crate) fn choices_with_target(
    prepared: &PreparedTool,
    ordinary: bool,
    target: Option<&std::path::Path>,
) -> Vec<ApprovalOption> {
    let mut options = vec![ApprovalOption {
        label: "Allow once".into(),
        approve: true,
        grant: None,
    }];
    if ordinary {
        let input = &prepared.invocation.input;
        let name = &prepared.invocation.name;
        let grant = if name == "bash" {
            input["command"].as_str().map(|command| PermissionGrant {
                tool: name.clone(),
                shell_command: None,
                command: Some(command.to_owned()),
                path_prefix: None,
            })
        } else if target.is_some() || input["path"].is_string() {
            target
                .map(std::path::Path::to_path_buf)
                .or_else(|| {
                    input["path"].as_str().and_then(|path| {
                        crate::policy::grant_path(&prepared.invocation.workspace, path)
                    })
                })
                .and_then(|path| {
                    Some(PermissionGrant {
                        tool: name.clone(),
                        shell_command: None,
                        command: None,
                        path_prefix: Some(path.to_str()?.to_owned()),
                    })
                })
        } else {
            Some(PermissionGrant {
                tool: name.clone(),
                shell_command: None,
                command: None,
                path_prefix: None,
            })
        };
        if let Some(grant) = grant {
            let label = if let Some(command) = &grant.command {
                format!("Allow this exact command for this session: {command}")
            } else if let Some(path) = &grant.path_prefix {
                format!("Allow {} at {path} for this session", grant.tool)
            } else {
                format!("Allow {} for this session", grant.tool)
            };
            let path_scoped = grant.path_prefix.is_some();
            options.push(ApprovalOption {
                label,
                approve: true,
                grant: Some(grant),
            });
            if path_scoped {
                options.push(ApprovalOption {
                    label: format!("Allow {name} for this session"),
                    approve: true,
                    grant: Some(PermissionGrant {
                        tool: name.clone(),
                        shell_command: None,
                        command: None,
                        path_prefix: None,
                    }),
                });
            }
        }
    }
    options.push(ApprovalOption {
        label: "Deny".into(),
        approve: false,
        grant: None,
    });
    options
}

pub(crate) fn choices_with_shell(
    prepared: &PreparedTool,
    ordinary: bool,
    target: Option<&std::path::Path>,
    shell: Option<&ShellCommandGrant>,
) -> Vec<ApprovalOption> {
    let mut options = choices_with_target(prepared, ordinary, target);
    if ordinary && prepared.invocation.name == "bash" {
        if let Some(shell) = shell.filter(|shell| shell.well_formed()) {
            options.insert(
                options.len() - 1,
                ApprovalOption {
                    label: format!(
                        "Allow simple Bash {} commands with any literal arguments for this session",
                        shell.program
                    ),
                    approve: true,
                    grant: Some(PermissionGrant {
                        tool: "bash".into(),
                        path_prefix: None,
                        command: None,
                        shell_command: Some(shell.clone()),
                    }),
                },
            );
        }
    }
    options
}

pub(crate) fn options(
    db: &Connection,
    operation: &OperationId,
    revision: i64,
) -> anyhow::Result<ApprovalOptions> {
    let row: Option<(String, String)> = db.query_row(
        "SELECT o.thread,d.options FROM decisions d JOIN operations o ON o.id=d.operation JOIN runs r ON r.id=o.run JOIN threads t ON t.id=o.thread JOIN sessions s ON s.id=t.session WHERE o.id=?1 AND d.revision=?2 AND s.revision=?2 AND o.policy_revision=?2 AND d.state='pending' AND o.state='approval' AND r.state='active'",
        params![operation, revision], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?;
    let (thread, choices) = row.context("decision is stale or no longer pending")?;
    let options: Vec<ApprovalOption> = decode(choices)?;
    ensure!(
        !options.is_empty(),
        "decision has no prepared approval options"
    );
    Ok(ApprovalOptions {
        operation: operation.clone(),
        thread: ThreadId(thread),
        revision,
        options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{encode, tests::fixture};
    use serde_json::json;

    #[test]
    fn ordinary_file_approval_offers_distinct_path_and_tool_scopes() {
        let workspace = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let target = crate::policy::grant_path(&workspace, "file.rs").unwrap();
        let prepared = PreparedTool {
            invocation: Invocation {
                operation_id: OperationId::new(),
                run_id: RunId::new(),
                thread_id: ThreadId::new(),
                workspace,
                name: "edit_file".into(),
                input: json!({"path":"file.rs"}),
            },
            specification: serde_json::Value::Null,
            preview: "edit file".into(),
            presentation: None,
        };
        let options = choices_with_target(&prepared, true, Some(&target));
        assert_eq!(options.len(), 4);
        assert_eq!(options[0].label, "Allow once");
        assert_eq!(
            options[1].grant.as_ref().unwrap().path_prefix.as_deref(),
            target.to_str()
        );
        assert_eq!(options[2].label, "Allow edit_file for this session");
        assert!(options[2].grant.as_ref().unwrap().path_prefix.is_none());
        assert_eq!(options[3].label, "Deny");
        let mandatory = choices_with_target(&prepared, false, Some(&target));
        assert_eq!(mandatory.len(), 2);
        assert!(mandatory.iter().all(|option| option.grant.is_none()));
    }

    #[test]
    fn offered_grants_are_exact_and_mandatory_guards_cannot_be_granted() {
        for ordinary in [true, false] {
            let (_dir, mut store, receipt, models) = fixture();
            let operation = OperationId::new();
            let run = RunId::new();
            let command = format!("printf '%s' '{}'", "full command λ".repeat(500));
            let prepared = PreparedTool {
                invocation: Invocation {
                    operation_id: operation.clone(),
                    run_id: run.clone(),
                    thread_id: receipt.thread.clone(),
                    workspace: String::new(),
                    name: "bash".into(),
                    input: json!({"command": command}),
                },
                specification: json!({}),
                preview: "short preview".into(),
                presentation: None,
            };
            store
                .db
                .execute(
                    "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
                    params![run, receipt.thread],
                )
                .unwrap();
            store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state,prepared) VALUES(?1,?2,?3,'bash','tool','call','{}','approval',?4)",params![operation,run,receipt.thread,encode(&prepared).unwrap()]).unwrap();
            store.db.execute("INSERT INTO decisions(operation,revision,preview,state,options) VALUES(?1,0,'short preview','pending',?2)",params![operation,encode(&choices(&prepared,ordinary)).unwrap()]).unwrap();
            let offered = options(&store.db, &operation, 0).unwrap();
            assert!(options(&store.db, &operation, 1).is_err());
            let mut grant = PermissionGrant {
                tool: "bash".into(),
                shell_command: None,
                path_prefix: None,
                command: Some(command.clone()),
            };
            grant.command = Some("arbitrary injected command".into());
            assert!(store
                .command(
                    &Command::DecideWithGrant {
                        id: CommandId::new(),
                        operation: operation.clone(),
                        revision: 0,
                        approve: true,
                        grant: Some(grant)
                    },
                    &models
                )
                .is_err());
            assert!(options(&store.db, &operation, 0).is_ok());
            let granted = offered
                .options
                .iter()
                .find_map(|choice| choice.grant.clone());
            assert_eq!(granted.is_some(), ordinary);
            if let Some(grant) = &granted {
                assert_eq!(grant.command.as_deref(), Some(command.as_str()));
            }
            store
                .command(
                    &Command::DecideWithGrant {
                        id: CommandId::new(),
                        operation: operation.clone(),
                        revision: 0,
                        approve: true,
                        grant: granted,
                    },
                    &models,
                )
                .unwrap();
            assert!(options(&store.db, &operation, 0).is_err());
        }
    }
}
