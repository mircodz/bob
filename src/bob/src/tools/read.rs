use super::*;
use std::ops::Range;

#[cfg(test)]
#[path = "read_presentation_tests.rs"]
mod read_presentation_tests;

pub(super) struct Page {
    pub body: String,
    pub visible: Vec<Range<usize>>,
    returned: Option<(usize, usize, usize)>,
    next_line: Option<usize>,
    first_clipped: Option<(usize, usize)>,
    clipped_lines: usize,
}

impl Page {
    pub(super) fn presentation(
        &self,
        report: &crate::project_instructions::InstructionReport,
    ) -> bob_kernel::ToolResultPresentation {
        let summary = match self.returned {
            Some((first, last, column)) => {
                let mut summary = if first == last {
                    format!("line {first}")
                } else {
                    format!("lines {first}–{last}")
                };
                if column > 1 {
                    summary.push_str(&format!(" · starting column {column}"));
                }
                summary
            }
            None => "No lines returned".into(),
        };
        let mut warnings = Vec::new();
        if self.returned.is_none() {
            warnings.push("[!] Offset is beyond end of file.".into());
        }
        if let Some(line) = self.next_line {
            warnings.push(format!(
                "[!] Output paged; use offset={line} and limit to read further."
            ));
        }
        if let Some((line, column)) = self.first_clipped {
            warnings.push(format!(
                "[!] {} line(s) truncated; first continuation: offset={line}, column_offset={column}.{}",
                self.clipped_lines,
                if self.clipped_lines > 1 {
                    " Inspect output for remaining line cursors."
                } else {
                    ""
                }
            ));
        }
        if !report.failures.is_empty() {
            let failure = report.failures[0]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let detail: String = failure.chars().take(200).collect();
            warnings.push(format!(
                "[!] Project conventions: {} loading failure(s). {detail}{}",
                report.failures.len(),
                if detail.len() < failure.len() || report.failures.len() > 1 {
                    " … Inspect output for full diagnostics."
                } else {
                    ""
                }
            ));
        }
        if report.cancelled {
            warnings.push("[!] Convention loading cancelled.".into());
        }
        if report.truncated {
            warnings.push("[!] Applicable conventions may be incomplete.".into());
        }
        bob_kernel::ToolResultPresentation {
            summary: Some(summary),
            body_preview: Some(warnings.join("\n")),
            exit_code: None,
        }
    }
}

pub(super) fn optional_integer<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<usize>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value
        .as_u64()
        .and_then(|number| usize::try_from(number).ok())
        .or_else(|| {
            let number = value.as_f64()?;
            (number >= 0.0 && number < 2_f64.powi(usize::BITS as i32) && number.fract() == 0.0)
                .then_some(number as usize)
        })
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("expected a nonnegative integer"))
}

pub(super) fn page(text: &str, input: &ReadInput) -> Result<Page> {
    let offset = input.offset.unwrap_or(1);
    let limit = input.limit.unwrap_or(2000).min(2000);
    let column_offset = input.column_offset.unwrap_or(1);
    ensure!(
        offset > 0 && limit > 0 && column_offset > 0,
        "offset, limit, and column_offset must be positive integers"
    );
    let mut page = Page {
        body: String::new(),
        visible: Vec::new(),
        returned: None,
        next_line: None,
        first_clipped: None,
        clipped_lines: 0,
    };
    let lines = text
        .split_inclusive('\n')
        .chain(text.is_empty().then_some(""));
    let mut byte_offset = 0;
    let mut shown = 0;
    for (index, line) in lines.enumerate() {
        let end = byte_offset + line.len();
        if index < offset - 1 {
            byte_offset = end;
            continue;
        }
        if shown == limit {
            page.next_line = Some(index + 1);
            page.body.push_str(&format!(
                "\n\n... [output truncated; use offset={} and limit to read further]",
                index + 1
            ));
            break;
        }
        let display = line.strip_suffix('\n').unwrap_or(line);
        let column = if index == offset - 1 {
            column_offset
        } else {
            1
        };
        let start = display
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(display.len()))
            .nth(column - 1)
            .context("column_offset is beyond the end of the line")?;
        let stop = start
            + display[start..]
                .char_indices()
                .nth(2000)
                .map_or(display.len() - start, |(i, _)| i);
        let clipped = stop < display.len();
        let continuation = if clipped {
            format!(
                "... [line truncated; use offset={} and column_offset={} to continue]",
                index + 1,
                column + 2000
            )
        } else {
            String::new()
        };
        let rendered = format!(
            "{:>6}\t{}{}",
            index + 1,
            &display[start..stop],
            continuation
        );
        if page.body.len() + rendered.len() + 1 > 256 * 1024 {
            page.next_line = Some(index + 1);
            page.body.push_str(&format!(
                "\n\n... [output truncated; use offset={} and limit to read further]",
                index + 1
            ));
            break;
        }
        if !page.body.is_empty() {
            page.body.push('\n');
        }
        page.body.push_str(&rendered);
        let returned = page.returned.get_or_insert((index + 1, index + 1, column));
        returned.1 = index + 1;
        if clipped {
            page.first_clipped.get_or_insert((index + 1, column + 2000));
            page.clipped_lines += 1;
        }
        page.visible
            .push(byte_offset + start..if clipped { byte_offset + stop } else { end });
        byte_offset = end;
        shown += 1;
    }
    if shown == 0 {
        page.body.push_str("(offset is beyond end of file)");
    }
    Ok(page)
}
