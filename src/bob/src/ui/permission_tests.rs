use super::*;
use crossterm::event::{KeyEventKind, MouseButton, MouseEvent, MouseEventKind};

fn grant_options() -> Vec<bob_kernel::ApprovalOption> {
    let mut options = approval::initial_options();
    options.insert(
        1,
        bob_kernel::ApprovalOption {
            label: "Always allow this command".into(),
            approve: true,
            grant: Some(bob_kernel::PermissionGrant {
                tool: "bash".into(),
                shell_command: None,
                path_prefix: None,
                command: Some("printf hello".into()),
            }),
        },
    );
    options
}

#[test]
fn permission_success_is_silent_and_errors_remain_visible() {
    for approve in [false, true] {
        let mut app = app(vec![]);
        refresh(&mut app, 10, vec![decision(1, "op")]);
        load_options(&mut app);
        app.pane_mut()
            .unwrap()
            .notify_current("Earlier useful error");
        let notices = app.pane().unwrap().notices.len();
        let (key, _) = app.take_decision(approve).unwrap();
        app.focus = Focus::PermissionInvalidated;
        app.reply(Reply::Decision {
            key: key.clone(),
            result: Ok(Receipt {
                detail: "permissions updated".into(),
                ..receipt(11)
            }),
        });
        assert_eq!(app.sequence_floor, 11);
        assert_eq!(app.focus, Focus::Composer);
        assert!(app.pane().unwrap().receipt.is_none());
        assert_eq!(app.pane().unwrap().notices.len(), notices);
        let text = screen(&render(&mut app, 100, 30));
        assert!(!text.contains("Decision recorded"));
        assert!(!text.contains("permissions updated"));
        assert!(!text.contains("Permission changed or closed"));
        assert!(text.contains("Earlier useful error"));
        app.reply(Reply::Decision {
            key,
            result: Err("storage unavailable".into()),
        });
        assert!(screen(&render(&mut app, 100, 30)).contains("Decision not confirmed"));
        assert!(app.pane().unwrap().notice.contains("storage unavailable"));
    }
}

#[test]
fn confirmed_prompt_closes_quietly_without_submitting_an_unsent_draft() {
    let mut app = app(vec![]);
    app.draft_mut().insert("/quit");
    refresh(&mut app, 10, vec![decision(1, "op")]);
    load_options(&mut app);
    let (captured, _) = app.take_decision(true).unwrap();
    app.focus = Focus::PermissionInvalidated;
    app.reply(Reply::Decision {
        key: captured,
        result: Ok(receipt(11)),
    });
    assert!(!screen(&render(&mut app, 100, 30)).contains("Permission changed or closed"));
    let mut jobs = JoinSet::new();
    assert!(!app.key(key_event_repeat(), &mut jobs));
    assert!(!app.key(key(KeyCode::Enter), &mut jobs));
    assert_eq!(app.draft_mut().text, "/quit");
    assert!(jobs.is_empty());
}

#[test]
fn permission_preempts_picker_and_unrelated_focus_without_losing_draft() {
    for focus in [Focus::Composer, Focus::Sidebar, Focus::Inspector] {
        let mut app = app(vec![]);
        app.draft_mut().insert("unsent draft");
        app.picker = Some(commands::Picker::theme("dark"));
        app.focus = focus;
        refresh(&mut app, 10, vec![decision(1, "op")]);
        load_options(&mut app);
        assert_eq!(app.focus, Focus::Permission);
        assert!(app.picker.is_none());
        app.focus = Focus::Sidebar;
        let text = screen(&render(&mut app, 100, 30));
        assert_eq!(app.focus, Focus::Permission);
        assert!(text.contains("1. Allow once"));
        assert!(!text.contains("Theme ("));
        assert_eq!(app.draft_mut().text, "unsent draft");
    }
}

#[test]
fn first_prompt_in_another_thread_does_not_inherit_closed_prompt_focus() {
    let mut app = app(vec![]);
    refresh(&mut app, 10, vec![decision(1, "op")]);
    app.focus = Focus::PermissionInvalidated;
    app.select(ThreadId("child".into()));
    assert_eq!(app.focus, Focus::Composer);
    let mut next = view(11, vec![]);
    next.decisions = vec![Decision {
        thread: ThreadId("child".into()),
        ..decision(1, "child-op")
    }];
    app.reply(Reply::View {
        thread: ThreadId("child".into()),
        generation: 0,
        result: Ok(next),
    });
    assert_eq!(app.focus, Focus::Permission);
    assert_eq!(
        app.pane()
            .unwrap()
            .permission
            .as_ref()
            .unwrap()
            .decision
            .operation
            .0,
        "child-op"
    );
}

#[test]
fn permission_loading_focuses_without_temporary_numbered_actions_or_draft_leaks() {
    let mut app = app(vec![]);
    app.draft_mut().insert("/quit");
    let revision = app.draft_mut().revision;
    refresh(&mut app, 10, vec![decision(1, "op")]);
    assert_eq!(app.focus, Focus::Permission);
    let mut jobs = JoinSet::new();
    for code in [
        KeyCode::Enter,
        KeyCode::Char('1'),
        KeyCode::Char('2'),
        KeyCode::Esc,
    ] {
        assert!(!app.key(key(code), &mut jobs));
        assert!(app.take_decision(true).is_none());
        assert!(app.take_decision(false).is_none());
    }
    app.paste("unwanted");
    let permission = app.pane().unwrap().permission.as_ref().unwrap();
    assert!(!permission.options_loaded);
    assert!(permission.options.is_empty());
    assert_eq!(permission.choice, None);
    assert_eq!(app.focus, Focus::Permission);
    assert_eq!(app.draft_mut().text, "/quit");
    assert_eq!(app.draft_mut().revision, revision);
    assert!(jobs.is_empty());

    install_options(&mut app, grant_options());
    let permission = app.pane().unwrap().permission.as_ref().unwrap();
    assert!(permission.options_loaded);
    assert_eq!(permission.choice, Some(0));
    assert_eq!(permission.options.len(), 3);
    assert!(permission.options[1].grant.is_some());
    assert!(!permission.pending);
    assert!(app.pane().unwrap().decided.is_none());
    assert_eq!(app.focus, Focus::Permission);
}

#[test]
fn identical_authoritative_options_preserve_selection_focus_and_details() {
    let mut app = app(vec![]);
    refresh(&mut app, 10, vec![decision(1, "op")]);
    install_options(&mut app, grant_options());
    let mut jobs = JoinSet::new();
    app.key(key(KeyCode::Down), &mut jobs);
    app.key(key(KeyCode::Char('i')), &mut jobs);
    for focus in [
        Focus::Permission,
        Focus::Inspector,
        Focus::PermissionInvalidated,
    ] {
        app.focus = focus;
        install_options(&mut app, grant_options());
        let permission = app.pane().unwrap().permission.as_ref().unwrap();
        assert_eq!(permission.choice, Some(1));
        assert!(permission.details);
        assert_eq!(app.focus, focus);
    }
    assert!(jobs.is_empty());
}

#[test]
fn changed_authoritative_option_snapshot_requires_explicit_rearming() {
    for change in 0..4 {
        let mut app = app(vec![]);
        app.draft_mut().insert("/quit");
        refresh(&mut app, 10, vec![decision(1, "op")]);
        install_options(&mut app, grant_options());
        let mut changed = grant_options();
        match change {
            0 => changed[1].label = "Different label".into(),
            1 => changed[1].approve = false,
            2 => changed[1].grant.as_mut().unwrap().command = Some("printf changed".into()),
            _ => changed.swap(1, 2),
        }
        install_options(&mut app, changed.clone());
        assert_eq!(app.focus, Focus::PermissionInvalidated);
        assert_eq!(
            app.pane().unwrap().permission.as_ref().unwrap().choice,
            None
        );
        let mut jobs = JoinSet::new();
        for code in [
            KeyCode::Enter,
            KeyCode::Char('1'),
            KeyCode::Char('2'),
            KeyCode::Esc,
        ] {
            assert!(!app.key(key(code), &mut jobs));
            assert_eq!(app.focus, Focus::PermissionInvalidated);
        }
        install_options(&mut app, changed);
        assert_eq!(app.focus, Focus::PermissionInvalidated);
        assert_eq!(
            app.pane().unwrap().permission.as_ref().unwrap().choice,
            None
        );
        app.paste("unwanted");
        assert_eq!(app.draft_mut().text, "/quit");
        assert!(jobs.is_empty());
        app.key(key(KeyCode::Tab), &mut jobs);
        assert_eq!(app.focus, Focus::Permission);
        assert_eq!(
            app.pane().unwrap().permission.as_ref().unwrap().choice,
            Some(0)
        );
    }
}

#[test]
fn mismatched_or_failed_options_never_leave_actions_enabled() {
    for mismatch in [false, true] {
        let mut app = app(vec![]);
        refresh(&mut app, 10, vec![decision(1, "op")]);
        load_options(&mut app);
        let key = DecisionKey::from(&app.pane().unwrap().permission.as_ref().unwrap().decision);
        app.reply(Reply::ApprovalOptions {
            key: key.clone(),
            result: if mismatch {
                Ok(bob_kernel::ApprovalOptions {
                    thread: key.thread,
                    operation: key.operation,
                    revision: 2,
                    options: grant_options(),
                })
            } else {
                Err("options unavailable".into())
            },
        });
        let permission = app.pane().unwrap().permission.as_ref().unwrap();
        assert!(!permission.options_loaded);
        assert!(permission.options.is_empty());
        assert!(app.take_option(0).is_none());
        assert_eq!(app.focus, Focus::PermissionInvalidated);
        assert!(app.pane().unwrap().force_refresh);
        load_options(&mut app);
        assert_eq!(app.focus, Focus::PermissionInvalidated);
        assert_eq!(
            app.pane().unwrap().permission.as_ref().unwrap().choice,
            None
        );
    }
}

#[test]
fn permission_is_modal_for_tab_mouse_composer_and_paste() {
    let mut app = app(vec![]);
    app.draft_mut().insert("saved draft");
    refresh(&mut app, 10, vec![decision(1, "op")]);
    load_options(&mut app);
    let mut jobs = JoinSet::new();
    app.key(key(KeyCode::Tab), &mut jobs);
    assert_eq!(app.focus, Focus::Permission);
    app.size = Rect::new(0, 0, 80, 24);
    app.hits.push(Hit {
        area: Rect::new(0, 20, 80, 4),
        action: HitAction::Composer,
    });
    app.mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 21,
            modifiers: KeyModifiers::NONE,
        },
        &mut jobs,
    );
    assert_eq!(app.focus, Focus::Permission);
    app.paste("ignored");
    assert_eq!(app.draft_mut().text, "saved draft");
    assert_eq!(app.draft_mut().cursor, "saved draft".len());
    assert!(jobs.is_empty());
}

#[test]
fn repeated_confirmation_keys_do_not_change_permission_selection() {
    let mut app = app(vec![]);
    refresh(&mut app, 10, vec![decision(1, "op")]);
    install_options(&mut app, grant_options());
    let mut jobs = JoinSet::new();
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        for code in [KeyCode::Enter, KeyCode::Char('2'), KeyCode::Esc] {
            let mut event = key(code);
            event.kind = kind;
            app.key(event, &mut jobs);
            assert_eq!(
                app.pane().unwrap().permission.as_ref().unwrap().choice,
                Some(0)
            );
            assert!(!app.pane().unwrap().permission.as_ref().unwrap().pending);
        }
    }
    assert!(jobs.is_empty());
}

#[test]
fn successful_own_decision_focuses_only_the_exact_previously_queued_successor() {
    for receipt_first in [false, true] {
        for revised in [false, true] {
            let mut app = app(vec![]);
            app.draft_mut().insert("/quit");
            refresh(
                &mut app,
                10,
                vec![decision(1, "first"), decision(2, "queued")],
            );
            load_options(&mut app);
            let (key, _) = app.take_decision(true).unwrap();
            app.focus = Focus::PermissionInvalidated;
            if receipt_first {
                app.reply(Reply::Decision {
                    key: key.clone(),
                    result: Ok(receipt(11)),
                });
                assert_eq!(app.focus, Focus::PermissionInvalidated);
            }
            refresh(
                &mut app,
                12,
                vec![decision(if revised { 3 } else { 2 }, "queued")],
            );
            load_options(&mut app);
            if !receipt_first {
                assert_eq!(app.focus, Focus::PermissionInvalidated);
                app.reply(Reply::Decision {
                    key,
                    result: Ok(receipt(11)),
                });
            }
            assert_eq!(
                app.focus,
                if revised {
                    Focus::PermissionInvalidated
                } else {
                    Focus::Permission
                }
            );
            let permission = app.pane().unwrap().permission.as_ref().unwrap();
            assert_eq!(permission.choice, if revised { None } else { Some(0) });
            assert!(!permission.pending);
            let mut jobs = JoinSet::new();
            let mut repeat = key_event_repeat();
            app.key(repeat, &mut jobs);
            repeat.code = KeyCode::Char('2');
            app.key(repeat, &mut jobs);
            assert!(jobs.is_empty());
            assert_eq!(app.draft_mut().text, "/quit");
        }
    }
}

fn key_event_repeat() -> KeyEvent {
    let mut event = key(KeyCode::Enter);
    event.kind = KeyEventKind::Repeat;
    event
}

#[test]
fn sequential_new_prompts_focus_after_own_success_in_either_reply_order() {
    for receipt_first in [false, true] {
        let mut app = app(vec![]);
        app.draft_mut().insert("/quit");
        refresh(&mut app, 10, vec![decision(1, "edit_file")]);
        load_options(&mut app);
        for (index, operation) in ["create_file", "bash"].into_iter().enumerate() {
            let sequence = 11 + index as i64 * 3;
            let (captured, _) = app.take_decision(true).unwrap();
            app.focus = Focus::PermissionInvalidated;
            if receipt_first {
                app.reply(Reply::Decision {
                    key: captured.clone(),
                    result: Ok(receipt(sequence)),
                });
            }
            refresh(&mut app, sequence, vec![]);
            let mut jobs = JoinSet::new();
            assert!(!app.key(key(KeyCode::Enter), &mut jobs));
            app.key(key(KeyCode::Char('1')), &mut jobs);
            app.paste("ignored between prompts");
            assert_eq!(app.draft_mut().text, "/quit");
            assert!(jobs.is_empty());
            refresh(&mut app, sequence + 1, vec![decision(1, operation)]);
            if !receipt_first {
                assert_eq!(app.focus, Focus::PermissionInvalidated);
                app.reply(Reply::Decision {
                    key: captured,
                    result: Ok(receipt(sequence)),
                });
            }
            assert_eq!(app.focus, Focus::Permission);
            assert!(app.take_decision(true).is_none());
            load_options(&mut app);
            assert_eq!(app.focus, Focus::Permission);
            let permission = app.pane().unwrap().permission.as_ref().unwrap();
            assert_eq!(permission.choice, Some(0));
            assert!(!permission.pending);
            assert!(!app.pane().unwrap().decision_followup);
            app.key(key_event_repeat(), &mut jobs);
            assert!(jobs.is_empty());
        }
        assert_eq!(app.draft_mut().text, "/quit");
    }
}

#[test]
fn sequential_same_operation_revision_and_external_replacement_stay_invalidated() {
    for same_operation in [false, true] {
        let mut app = app(vec![]);
        refresh(&mut app, 10, vec![decision(1, "first")]);
        load_options(&mut app);
        let (captured, _) = app.take_decision(true).unwrap();
        app.focus = Focus::PermissionInvalidated;
        app.reply(Reply::Decision {
            key: captured.clone(),
            result: Ok(receipt(11)),
        });
        refresh(
            &mut app,
            12,
            vec![decision(2, if same_operation { "first" } else { "next" })],
        );
        load_options(&mut app);
        if !same_operation {
            assert_eq!(app.focus, Focus::Permission);
            refresh(&mut app, 13, vec![decision(3, "external replacement")]);
            load_options(&mut app);
        }
        assert_eq!(app.focus, Focus::PermissionInvalidated);
        app.reply(Reply::Decision {
            key: captured,
            result: Ok(receipt(11)),
        });
        assert_eq!(app.focus, Focus::PermissionInvalidated);
        assert_eq!(
            app.pane().unwrap().permission.as_ref().unwrap().choice,
            None
        );
    }
}

#[test]
fn queued_options_change_before_receipt_keeps_the_invalidation_barrier() {
    let mut app = app(vec![]);
    refresh(
        &mut app,
        10,
        vec![decision(1, "first"), decision(2, "queued")],
    );
    load_options(&mut app);
    let (key, _) = app.take_decision(true).unwrap();
    app.focus = Focus::PermissionInvalidated;
    refresh(&mut app, 11, vec![decision(2, "queued")]);
    load_options(&mut app);
    install_options(&mut app, grant_options());
    app.reply(Reply::Decision {
        key,
        result: Ok(receipt(11)),
    });
    assert_eq!(app.focus, Focus::PermissionInvalidated);
    assert_eq!(
        app.pane().unwrap().permission.as_ref().unwrap().choice,
        None
    );
}
