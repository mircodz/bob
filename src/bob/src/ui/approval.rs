use super::style::*;
use bob_kernel::{ApprovalOption, Decision, ToolBody};
use ratatui::{
    style::Style,
    text::{Line, Span},
};

pub(super) struct Cache {
    pub width: u16,
    pub details: bool,
    pub theme: u64,
    pub rows: Vec<Line<'static>>,
}

#[cfg(test)]
pub(super) fn initial_options() -> Vec<ApprovalOption> {
    vec![
        ApprovalOption {
            label: "Allow once".into(),
            approve: true,
            grant: None,
        },
        ApprovalOption {
            label: "Deny".into(),
            approve: false,
            grant: None,
        },
    ]
}

pub(super) fn same_options(left: &[ApprovalOption], right: &[ApprovalOption]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.label == right.label && left.approve == right.approve && left.grant == right.grant
        })
}

pub(super) fn option_line(option: &ApprovalOption, index: usize, selected: bool) -> Line<'static> {
    let color = if !option.approve {
        Palette::ERROR()
    } else if option.grant.is_some() {
        Palette::OK()
    } else {
        Palette::TEXT()
    };
    Line::from(vec![
        Span::styled(
            format!(" {} ", if selected { "❯" } else { " " }),
            text().fg(Palette::WARN()),
        ),
        Span::styled(format!("{}. ", index + 1), text().fg(Palette::DIM())),
        Span::styled(
            safe(&option.label),
            if selected {
                bold().fg(color)
            } else {
                text().fg(color)
            },
        ),
    ])
}

pub(super) fn title(decision: &Decision) -> String {
    let action = decision.presentation.as_ref().map_or_else(
        || decision.name.replace('_', " "),
        |presentation| presentation.title.clone(),
    );
    format!("Allow {}?", safe(&action))
}

pub(super) fn rows(decision: &Decision, details: bool, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(presentation) = &decision.presentation {
        if let Some(target) = &presentation.target {
            literal(&mut lines, target, "", bold(), width);
        }
        match &presentation.body {
            ToolBody::Text { text: body } => {
                for source in safe(body).split('\n') {
                    lines.extend(
                        prose_wrap(&Line::styled(source.to_owned(), text()), width)
                            .into_iter()
                            .map(|(_, line)| line),
                    );
                }
            }
            ToolBody::Command { command } => literal(&mut lines, command, "$ ", text(), width),
            ToolBody::Edit {
                old_string,
                new_string,
            } => {
                literal(
                    &mut lines,
                    old_string,
                    "− ",
                    text().fg(Palette::ERROR()),
                    width,
                );
                literal(
                    &mut lines,
                    new_string,
                    "+ ",
                    text().fg(Palette::OK()),
                    width,
                );
            }
            ToolBody::Create { content } => {
                literal(&mut lines, content, "+ ", text().fg(Palette::OK()), width)
            }
        }
        if details {
            lines.push(Line::default());
            lines.push(Line::styled(
                "Preparation details",
                bold().fg(Palette::DIM()),
            ));
            for detail in &presentation.details {
                literal(&mut lines, detail, "", text().fg(Palette::DIM()), width);
            }
            literal(
                &mut lines,
                &decision.preview,
                "",
                text().fg(Palette::DIM()),
                width,
            );
        }
    } else {
        literal(&mut lines, &decision.preview, "", text(), width);
    }
    if details {
        literal(
            &mut lines,
            &format!(
                "Operation {} · revision {}",
                decision.operation, decision.revision
            ),
            "",
            text().fg(Palette::DIM()),
            width,
        );
    }
    lines
}

fn literal(lines: &mut Vec<Line<'static>>, source: &str, prefix: &str, style: Style, width: usize) {
    let indent = Line::raw(prefix).width().min(width.saturating_sub(1));
    for source in safe(source).split('\n') {
        for (index, (_, mut line)) in wrap(
            &Line::styled(source.to_owned(), style),
            width.saturating_sub(indent).max(1),
        )
        .into_iter()
        .enumerate()
        {
            line.spans.insert(
                0,
                Span::styled(
                    if index == 0 {
                        prefix.to_owned()
                    } else {
                        " ".repeat(indent)
                    },
                    style,
                ),
            );
            lines.push(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn numbered_options_match_production_styles_without_selection_background() {
        let mut options = initial_options();
        options.insert(
            1,
            ApprovalOption {
                label: "Always allow write_file".into(),
                approve: true,
                grant: Some(bob_kernel::PermissionGrant {
                    tool: "write_file".into(),
                    shell_command: None,
                    path_prefix: None,
                    command: None,
                }),
            },
        );
        for (index, (option, color)) in options
            .iter()
            .zip([Palette::TEXT(), Palette::OK(), Palette::ERROR()])
            .enumerate()
        {
            let line = option_line(option, index, true);
            assert_eq!(line.spans[0].content, " ❯ ");
            assert_eq!(line.spans[0].style.fg, Some(Palette::WARN()));
            assert_eq!(line.spans[1].content, format!("{}. ", index + 1));
            assert_eq!(line.spans[1].style.fg, Some(Palette::DIM()));
            assert_eq!(line.spans[2].style.fg, Some(color));
            assert!(line.spans[2].style.add_modifier.contains(Modifier::BOLD));
            assert!(line.spans.iter().all(|span| span.style.bg.is_none()));
            let idle = option_line(option, index, false);
            assert_eq!(idle.spans[0].content, "   ");
            assert!(!idle.spans[2].style.add_modifier.contains(Modifier::BOLD));
        }
    }
}
