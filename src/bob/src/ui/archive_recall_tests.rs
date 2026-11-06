use super::*;

fn search_match() -> Value {
    json!({
        "session_id":"archive-session", "thread_id":"root", "sequence":42,
        "title":"Kusto query discussion", "workspace":"/work/project", "updated_at":1234,
        "kind":"assistant", "source":"message.text", "snippet":"Use summarize for the Kusto query",
        "snippet_offset":0, "preview_only":false, "original_complete":true,
        "read_ref":{"view":"entry", "session_id":"archive-session", "thread_id":"root", "sequence":42}
    })
}

fn search_page(matches: Vec<Value>, complete: bool) -> Value {
    json!({
        "view":"search", "query":"Kusto query", "matches":matches,
        "next_cursor":null, "scan_complete":complete, "examined_entries":12,
        "examined_bytes":4096, "limitations":[], "initial_head":42
    })
}

fn recall_entry(name: &str, value: Value) -> Entry {
    let mut entry = entry(name, &value.to_string());
    entry.tool.as_mut().unwrap().input_preview = if name == "search_conversation" {
        json!({"query":"Kusto query"})
    } else {
        json!({"view":value["view"], "session_id":"archive-session", "thread_id":"root"})
    };
    entry
}

#[test]
fn archive_search_has_compact_header_matches_and_full_page_hit() {
    let entry = recall_entry(
        "search_conversation",
        search_page(vec![search_match()], true),
    );
    let source = entry.text.clone();
    let rendered = rows(&entry, false, 160);
    let shown = screen(&rendered);
    for expected in [
        "Search conversations Kusto query",
        "1 matches on this page",
        "scan complete",
        "Kusto query discussion",
        "/work/project",
        "#42",
        "Use summarize",
        "Examined 12 entries · 4096 bytes",
    ] {
        assert!(shown.contains(expected), "missing {expected}: {shown}");
    }
    assert!(!shown.contains("\"matches\""));
    assert_inspectable(&rendered, "search_conversation");
    assert_eq!(entry.text, source);
}

#[test]
fn archive_empty_search_distinguishes_complete_partial_and_unknown_coverage() {
    for complete in [true, false] {
        let entry = recall_entry("search_conversation", search_page(vec![], complete));
        let shown = screen(&rows(&entry, false, 140));
        if complete {
            assert!(shown.contains("No matches in available content · scan complete"));
        } else {
            assert!(shown.contains("No matches in examined content · partial scan"));
            assert!(!shown.contains("scan complete"));
            assert!(!shown.contains("No matches in available content"));
        }
    }
    let mut value = search_page(vec![], false);
    value.as_object_mut().unwrap().remove("scan_complete");
    let shown = screen(&rows(
        &recall_entry("search_conversation", value),
        false,
        140,
    ));
    assert!(shown.contains("coverage not reported"));
    assert!(!shown.contains("scan complete"));
}

#[test]
fn archive_partial_search_prioritizes_pagination_and_retention_limits() {
    let mut value = search_page(vec![], false);
    value["next_cursor"] = json!("next-page");
    value["limitations"] = json!([
        {"code":"source_missing", "count":2},
        {"code":"source_corrupt", "count":1},
        {"code":"source_oversized", "count":3},
        {"code":"preview_only", "count":4}
    ]);
    let rendered = rows(&recall_entry("search_conversation", value), false, 160);
    let shown = screen(&rendered);
    for expected in [
        "partial scan",
        "more pages available",
        "2 missing sources",
        "1 unreadable sources",
        "3 oversized sources skipped",
        "4 preview-only sources",
    ] {
        assert!(shown.contains(expected), "missing {expected}: {shown}");
    }
    assert!(has_color(&rendered, "Coverage:", Palette::WARN()));
    assert_inspectable(&rendered, "search_conversation");
}

#[test]
fn archive_read_views_show_excerpts_threads_and_source_warnings() {
    let pages = [
        json!({"view":"transcript", "session_id":"archive-session", "thread_id":"root",
            "entries":[{"sequence":42,"kind":"assistant","text":"Kusto excerpt", "source":"message.text",
                "preview_only":true,"original_complete":null,"text_truncated":true,"total_bytes":600}],
            "next_cursor":"page-two", "retention":"retained_entries", "scan_complete":false,
            "examined_entries":1,"limitations":[{"code":"preceding_context_limited","count":1}]}),
        json!({"view":"entry", "session_id":"archive-session", "thread_id":"root", "sequence":42,
            "source":"entries.text", "preview_only":true, "original_complete":null, "retention":"preview_only",
            "offset":0, "total_bytes":600, "next_offset":80, "page_truncated":true,"text":"Kusto excerpt", "limitations":[]}),
        json!({"view":"threads", "session_id":"archive-session", "thread_id":"root",
            "threads":[{"thread_id":"child-reader", "parent_thread_id":"root", "name":"Archive reader"}],
            "next_cursor":"child-reader", "scan_complete":false}),
    ];
    for value in pages {
        let view = value["view"].as_str().unwrap().to_owned();
        let entry = recall_entry("read_conversation", value);
        let rendered = rows(&entry, true, 160);
        let shown = screen(&rendered);
        assert!(shown.contains(&format!(
            "Read conversation {view} · archive-session · root"
        )));
        assert!(shown.contains("more pages available"));
        if view == "threads" {
            assert!(shown.contains("child-reader · Archive reader"));
        } else {
            assert!(shown.contains("Kusto excerpt"));
            assert!(shown.contains("Only a source preview is retained"));
        }
        if view == "transcript" {
            assert!(shown.contains("Entry excerpts shortened · read entry"));
            assert!(shown.contains("context windows limited by scan budget"));
            assert!(has_color(&rendered, "Coverage:", Palette::WARN()));
            assert!(shown.contains("partial scan"));
        }
        assert!(!shown.contains("\"view\""));
        assert_inspectable(&rendered, "read_conversation");
    }
}

#[test]
fn archive_ui_preview_never_implies_incomplete_source_or_no_matches() {
    for name in ["search_conversation", "read_conversation"] {
        let mut entry = entry(name, "{\"view\":");
        entry.is_preview = true;
        let rendered = rows(&entry, false, 100);
        let shown = screen(&rendered);
        assert!(shown.contains("Retained conversation page"));
        assert!(!shown.contains("incomplete"));
        assert!(!shown.contains("partial scan"));
        assert!(!shown.contains("No matches"));
        assert_inspectable(&rendered, name);
    }
    let mut entry = recall_entry("search_conversation", search_page(vec![], true));
    entry.is_preview = true;
    let rendered = rows(&entry, false, 140);
    assert!(!screen(&rendered).contains("more output"));
    assert!(!screen(&rendered).contains("incomplete"));
    assert_inspectable(&rendered, "search_conversation");
}

#[test]
fn archive_errors_remain_red_and_inspectable() {
    for name in ["search_conversation", "read_conversation"] {
        let mut entry = entry(name, "selected archive cannot be read: missing session");
        entry.kind = EntryKind::ToolError;
        entry.tool.as_mut().unwrap().state = OperationState::Failed;
        let rendered = rows(&entry, false, 120);
        assert!(screen(&rendered).contains("selected archive cannot be read: missing session"));
        assert!(has_color(&rendered[..1], "•", Palette::ERROR()));
        assert!(has_color(&rendered, "missing session", Palette::ERROR()));
        assert_inspectable(&rendered, name);
    }
}

#[test]
fn archive_long_results_remain_bounded_and_full_data_stays_inspectable() {
    let mut item = search_match();
    item["snippet"] = json!("Long Unicode 日本語 excerpt\n".repeat(1000));
    let entry = recall_entry("search_conversation", search_page(vec![item; 100], true));
    let original = entry.text.clone();
    for width in [40, 100] {
        let collapsed = rows(&entry, false, width);
        assert!(collapsed.len() <= PREVIEW + 5);
        assert!(screen(&collapsed).contains("more output"));
        let expanded = rows(&entry, true, width);
        assert!(expanded.len() <= archive_recall::EXPANDED_ROWS + 5);
        assert_inspectable(&expanded, "search_conversation");
        assert_eq!(entry.text, original);
    }
    let entry = recall_entry(
        "read_conversation",
        json!({
            "view":"entry", "session_id":"archive-session", "thread_id":"root", "sequence":42,
            "text":"retained long page\n".repeat(1000), "preview_only":false, "retention":"retained_text",
            "next_offset":null, "page_truncated":false, "limitations":[]
        }),
    );
    for expanded in [false, true] {
        let rendered = rows(&entry, expanded, 100);
        assert!(rendered.len() <= archive_recall::EXPANDED_ROWS + 5);
        assert_inspectable(&rendered, "read_conversation");
        assert!(!screen(&rendered).contains("source preview"));
    }
}

#[test]
fn archive_retrieval_is_visible_in_root_and_child_replay() {
    for root in [true, false] {
        let mut search = recall_entry(
            "search_conversation",
            search_page(vec![search_match()], true),
        );
        let mut read = recall_entry(
            "read_conversation",
            json!({"view":"entry", "session_id":"archive-session", "thread_id":"root", "text":"Retrieved discussion", "next_offset":null}),
        );
        read.sequence = 2;
        if !root {
            search.thread = ThreadId("child-reader".into());
            read.thread = search.thread.clone();
        }
        let mut pane = Pane::default();
        pane.view = Some(View {
            sequence: 2,
            entry_sequence: 2,
            entries: vec![search, read],
            threads: vec![],
            policies: vec![],
            decisions: vec![],
            operations: vec![],
            error: None,
        });
        let rendered = render::transcript(&pane, root, 160);
        let shown = screen(&rendered);
        assert_eq!(shown.matches("Search conversations").count(), 1);
        assert_eq!(shown.matches("Read conversation").count(), 1);
        assert!(shown.contains("Use summarize"));
        assert!(shown.contains("Retrieved discussion"));
        assert_inspectable(&rendered, "search_conversation");
        assert_inspectable(&rendered, "read_conversation");
    }
}

#[test]
fn archive_unicode_and_ansi_are_safe_at_narrow_terminal_widths() {
    let hostile = "\u{1b}]0;evil\u{7}\u{1b}[31m日本語 λ 👩‍💻 é\u{1b}[0m";
    let mut item = search_match();
    item["title"] = json!(hostile);
    item["snippet"] = json!(hostile);
    let mut search = recall_entry("search_conversation", search_page(vec![item], true));
    search.tool.as_mut().unwrap().input_preview["query"] = json!(hostile);
    let mut read = recall_entry(
        "read_conversation",
        json!({"view":"entry", "text":hostile, "next_offset":null}),
    );
    read.tool.as_mut().unwrap().input_preview["session_id"] = json!(hostile);
    for entry in [search, read] {
        for width in [12, 24, 40, 80] {
            let rendered = rows(&entry, true, width);
            let shown = screen(&rendered);
            assert!(!shown.contains('\u{1b}'));
            assert!(!shown.contains("evil"));
            let mut terminal =
                Terminal::new(TestBackend::new(width, rendered.len() as u16)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new(
                            rendered
                                .iter()
                                .map(|row| row.line.clone())
                                .collect::<Vec<_>>(),
                        ),
                        frame.area(),
                    );
                })
                .unwrap();
            assert!(terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| { !cell.symbol().chars().any(char::is_control) }));
            assert_inspectable(&rendered, &entry.tool.as_ref().unwrap().name);
        }
    }
}
