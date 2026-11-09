//! Desktop notifications sent through the terminal. Kitty gets OSC 99 (with
//! `o=unfocused`, so the terminal itself suppresses them while focused);
//! other terminals get the widely supported OSC 9.

use bob_kernel::{ThreadActivity, ThreadState, ThreadSummary};
use std::io::Write;

/// What the root thread is doing, reduced to the transitions worth a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Working,
    NeedsInput,
    Finished,
    Failed,
}

pub(super) fn phase(thread: &ThreadSummary) -> Phase {
    if thread.needs_approval || thread.activity == Some(ThreadActivity::Approval) {
        return Phase::NeedsInput;
    }
    match thread.state {
        ThreadState::Active | ThreadState::Stopping => Phase::Working,
        ThreadState::Failed | ThreadState::Interrupted => Phase::Failed,
        _ => Phase::Finished,
    }
}

/// The message for a phase change, or `None` when it is not worth interrupting for.
pub(super) fn message(previous: Phase, current: Phase) -> Option<&'static str> {
    match (previous, current) {
        (Phase::Working, Phase::Finished) => Some("Finished"),
        (Phase::Working, Phase::Failed) => Some("Stopped with an error"),
        (Phase::Working, Phase::NeedsInput) => Some("Needs your input"),
        _ => None,
    }
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(200)
        .collect()
}

pub(super) fn sequence(kitty: bool, id: u64, title: &str, body: &str) -> String {
    let (title, body) = (clean(title), clean(body));
    if kitty {
        format!(
            "\x1b]99;i=bob-{id}:d=0:o=unfocused;{title}\x1b\\\x1b]99;i=bob-{id}:p=body;{body}\x1b\\"
        )
    } else {
        format!("\x1b]9;{title}: {body}\x07")
    }
}

pub(super) fn is_kitty() -> bool {
    std::env::var("TERM").is_ok_and(|term| term == "xterm-kitty")
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
}

pub(super) fn send(output: &mut impl Write, kitty: bool, id: u64, title: &str, body: &str) {
    let _ = output.write_all(sequence(kitty, id, title, body).as_bytes());
    let _ = output.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_end_of_work_or_a_question_notifies() {
        use Phase::*;
        assert_eq!(message(Working, Finished), Some("Finished"));
        assert_eq!(message(Working, NeedsInput), Some("Needs your input"));
        assert_eq!(message(Working, Failed), Some("Stopped with an error"));
        for (previous, current) in [
            (Finished, Working),
            (Working, Working),
            (Finished, Finished),
            (NeedsInput, Working),
            (NeedsInput, Finished),
        ] {
            assert_eq!(
                message(previous, current),
                None,
                "{previous:?} -> {current:?}"
            );
        }
    }

    #[test]
    fn sequences_strip_control_characters_and_use_the_right_protocol() {
        let kitty = sequence(true, 7, "bob", "done\x1b]evil\x07 now");
        assert_eq!(
            kitty,
            "\x1b]99;i=bob-7:d=0:o=unfocused;bob\x1b\\\x1b]99;i=bob-7:p=body;done]evil now\x1b\\"
        );
        assert_eq!(sequence(false, 7, "bob", "done"), "\x1b]9;bob: done\x07");
    }
}
