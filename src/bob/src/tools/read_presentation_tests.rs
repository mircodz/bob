use super::*;
use crate::project_instructions::InstructionReport;

fn read(text: &str, input: Value) -> Page {
    page(text, &serde_json::from_value(input).unwrap()).unwrap()
}

#[test]
fn returned_ranges_and_columns_describe_only_emitted_lines() {
    let page = read(
        "first\n日本語\nlast\n",
        json!({"path":"source", "offset":2, "limit":20, "column_offset":2}),
    );
    assert_eq!(page.body, "     2\t本語\n     3\tlast");
    assert_eq!(page.visible, vec![9..16, 16..21]);
    let presentation = page.presentation(&InstructionReport::default());
    assert_eq!(
        presentation.summary.as_deref(),
        Some("lines 2–3 · starting column 2")
    );
    assert_eq!(presentation.body_preview.as_deref(), Some(""));

    let past_end = read("first\n", json!({"path":"source", "offset":8}));
    assert!(past_end.visible.is_empty());
    assert_eq!(past_end.body, "(offset is beyond end of file)");
    let presentation = past_end.presentation(&InstructionReport::default());
    assert_eq!(presentation.summary.as_deref(), Some("No lines returned"));
    assert!(presentation
        .body_preview
        .unwrap()
        .contains("beyond end of file"));
}

#[test]
fn paging_and_clipped_lines_leave_read_evidence_and_text_unchanged() {
    let source = format!("{}\nnext\nlast\n", "界".repeat(2001));
    let page = read(&source, json!({"path":"source", "limit":2}));
    assert_eq!(page.visible, vec![0..6000, 6004..6009]);
    assert_eq!(page.body, format!(
        "     1\t{}... [line truncated; use offset=1 and column_offset=2001 to continue]\n     2\tnext\n\n... [output truncated; use offset=3 and limit to read further]",
        "界".repeat(2000)
    ));
    let presentation = page.presentation(&InstructionReport::default());
    assert_eq!(presentation.summary.as_deref(), Some("lines 1–2"));
    let warnings = presentation.body_preview.unwrap();
    assert!(warnings.contains("offset=3"));
    assert!(warnings.contains("offset=1, column_offset=2001"));
    assert!(warnings.contains("1 line(s) truncated"));
}

#[test]
fn byte_capped_pages_report_actual_last_line_not_requested_limit() {
    let source = format!("{}\n", "界".repeat(2000)).repeat(100);
    let page = read(&source, json!({"path":"source", "limit":100}));
    let returned = page.visible.len();
    assert!(returned > 0 && returned < 100);
    let presentation = page.presentation(&InstructionReport::default());
    assert_eq!(presentation.summary, Some(format!("lines 1–{returned}")));
    assert!(presentation
        .body_preview
        .unwrap()
        .contains(&format!("offset={}", returned + 1)));
    assert_eq!(page.clipped_lines, 0);
    assert_eq!(page.visible.last().unwrap().end, returned * 6001);
}

#[test]
fn all_warning_categories_fit_the_persisted_byte_budget() {
    let source = format!("{}\n", "界".repeat(2001)).repeat(100);
    let page = read(&source, json!({"path":"source"}));
    let report = InstructionReport {
        failures: vec!["𐀀".repeat(3000); 100],
        truncated: true,
        cancelled: true,
        ..Default::default()
    };
    let presentation = page.presentation(&report);
    assert!(presentation.summary.unwrap().len() < 2048);
    let warnings = presentation.body_preview.unwrap();
    assert!(warnings.len() < 2048, "{} bytes", warnings.len());
    for marker in [
        "Output paged",
        "line(s) truncated",
        "100 loading failure(s)",
        "loading cancelled",
        "conventions may be incomplete",
        "Inspect output for full diagnostics",
    ] {
        assert!(warnings.contains(marker), "missing {marker}: {warnings}");
    }
    assert_eq!(page.first_clipped, Some((1, 2001)));
    assert_eq!(page.clipped_lines, page.visible.len());
}
