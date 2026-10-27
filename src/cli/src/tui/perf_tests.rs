use super::scrollback::ScrollbackRenderer;
use super::team::AgentTranscripts;
use super::test_support::test_app;
use super::view::{Cell, ToolStatus, ViewModel};
use super::App;
use bob_core::core::events::AgentEvent;
use bob_core::core::types::{ContentBlock, Message, Role, ToolOutput};
use ratatui::{backend::TestBackend, layout::Rect, Terminal};
use std::hint::black_box;
use std::time::{Duration, Instant};

fn tool_turns(turns: usize, agent: &str) -> Vec<AgentEvent> {
    let mut events = Vec::with_capacity(turns * 6);
    for index in 0..turns {
        let id = format!("{agent}-read-{index}");
        let input = serde_json::json!({"path": format!("src/module_{index}.rs")});
        events.extend([
            AgentEvent::UserPrompt {
                agent_id: agent.into(),
                text: format!("Inspect module {index}"),
            },
            AgentEvent::Message {
                agent_id: agent.into(),
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: id.clone(),
                        name: "read_file".into(),
                        input: input.clone(),
                    }],
                },
            },
            AgentEvent::ToolCall {
                agent_id: agent.into(),
                tool_use_id: id.clone(),
                name: "read_file".into(),
                input,
            },
            AgentEvent::ToolResult {
                agent_id: agent.into(),
                tool_use_id: id,
                output: ToolOutput::Read {
                    body: "    1 pub fn example() { println!(\"benchmark\"); }\n".repeat(40),
                    loader_context: None,
                },
                is_error: false,
            },
            AgentEvent::Message {
                agent_id: agent.into(),
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: format!("Module {index} inspected."),
                    }],
                },
            },
            AgentEvent::TurnEnd {
                agent_id: agent.into(),
                usage: Default::default(),
            },
        ]);
    }
    events
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn result_cells(cells: &[Cell]) -> usize {
    cells.iter().filter(|cell| matches!(cell,
        Cell::Tool { status: ToolStatus::Ok, output: Some(ToolOutput::Read { body, .. }), .. }
            if !body.is_empty()
    )).count()
}

fn replay(events: &[AgentEvent]) -> (ViewModel, AgentTranscripts) {
    let mut view = ViewModel::default();
    let mut teams = AgentTranscripts::new();
    for event in events {
        view.apply(black_box(event));
        teams.apply(event, None);
    }
    (view, teams)
}

fn draw(app: &mut App, terminal: &mut Terminal<TestBackend>) {
    terminal.draw(|frame| app.draw(frame)).unwrap();
    black_box(terminal.backend().buffer());
}

#[test]
#[ignore = "opt-in release replay baseline; no wall-clock threshold"]
fn benchmark_tool_replay() {
    for turns in [2000, 4000] {
        let mut events = tool_turns(turns, "root");
        events.extend(tool_turns(turns, "worker"));
        let mut samples = Vec::new();
        let mut counts = (0, 0);
        for _ in 0..3 {
            let start = Instant::now();
            let (view, teams) = replay(&events);
            samples.push(start.elapsed());
            let child = teams.get("worker").unwrap();
            assert_eq!(result_cells(&view.cells), turns);
            assert_eq!(result_cells(&child.cells), turns);
            counts = (view.cells.len(), child.cells.len());
            black_box((&view, &teams));
        }
        println!("replay turns_per_agent={turns} event_rows={} root_cells={} child_cells={} result_cells={} median3={:?}",
            events.len(), counts.0, counts.1, 2 * turns, median(samples));
    }
}

#[test]
#[ignore = "opt-in release event-bearing frame baseline; no wall-clock threshold"]
fn benchmark_event_frames() {
    const FRAMES: usize = 60;
    for turns in [2000, 4000] {
        let events = tool_turns(turns, "root");
        for (agent, anchored) in [
            ("root", false),
            ("worker", false),
            ("root", true),
            ("worker", true),
        ] {
            let delta = AgentEvent::TextDelta {
                agent_id: agent.into(),
                text: "streaming update ".into(),
            };
            let mut samples = Vec::new();
            for _ in 0..3 {
                let mut app = test_app();
                (app.view, app.teams) = replay(&events);
                let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
                draw(&mut app, &mut terminal);
                if anchored {
                    app.scrollback.scroll_up(usize::MAX);
                    draw(&mut app, &mut terminal);
                    assert!(!app.scrollback.at_bottom());
                }
                let before = app.view.cells.len();
                let start = Instant::now();
                for _ in 0..FRAMES {
                    app.apply_agent_event(black_box(&delta));
                    draw(&mut app, &mut terminal);
                }
                samples.push(start.elapsed());
                assert_eq!(app.view.cells.len(), before + usize::from(agent == "root"));
                assert_eq!(result_cells(&app.view.cells), turns);
                assert_eq!(app.scrollback.at_bottom(), !anchored);
            }
            let elapsed = median(samples);
            println!("frames turns={turns} root_cells={} delta_agent={agent} old_anchor={anchored} frames={FRAMES} median3={elapsed:?} per_frame={:?}",
                turns * 3, elapsed / FRAMES as u32);
        }
    }
}

#[test]
#[ignore = "opt-in release cold traversal and switching baseline; no wall-clock threshold"]
fn benchmark_cold_traversal() {
    for turns in [2000, 4000] {
        let mut events = tool_turns(turns, "root");
        events.extend(tool_turns(turns, "worker"));
        let (view, teams) = replay(&events);
        let mut cold = Vec::new();
        let mut up = Vec::new();
        let mut reverse = Vec::new();
        for _ in 0..3 {
            let mut renderer = ScrollbackRenderer::new();
            let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
            let render = |renderer: &mut ScrollbackRenderer,
                          terminal: &mut Terminal<TestBackend>| {
                terminal
                    .draw(|frame| {
                        renderer.render(
                            frame,
                            Rect::new(0, 0, 120, 30),
                            &view.cells,
                            1,
                            false,
                            0,
                            0,
                        )
                    })
                    .unwrap();
                black_box(terminal.backend().buffer());
            };
            let start = Instant::now();
            render(&mut renderer, &mut terminal);
            cold.push(start.elapsed());
            let bottom = terminal.backend().buffer().clone();
            let start = Instant::now();
            renderer.scroll_up(usize::MAX);
            render(&mut renderer, &mut terminal);
            up.push(start.elapsed());
            assert!(!renderer.at_bottom());
            assert!((0..30)
                .filter_map(|row| renderer.hit_test_offset(row))
                .any(|(index, _)| index == 0));
            let start = Instant::now();
            renderer.scroll_down(usize::MAX);
            render(&mut renderer, &mut terminal);
            reverse.push(start.elapsed());
            assert!(renderer.at_bottom());
            assert_eq!(terminal.backend().buffer(), &bottom);
        }
        let mut app = test_app();
        app.view = view;
        app.teams = teams;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        draw(&mut app, &mut terminal);
        let mut switches = Vec::new();
        for _ in 0..3 {
            let start = Instant::now();
            for _ in 0..6 {
                app.sidebar_open = true;
                app.set_focused_agent(Some("worker".into()));
                draw(&mut app, &mut terminal);
                app.set_focused_agent(None);
                app.sidebar_open = false;
                draw(&mut app, &mut terminal);
            }
            switches.push(start.elapsed());
        }
        println!("traversal turns={turns} root_cells={} cache_limit=256 median3 cold_bottom={:?} full_up={:?} reverse_down={:?} sidebar_and_agent_12_frames={:?}",
            app.view.cells.len(), median(cold), median(up), median(reverse), median(switches));
    }
}
