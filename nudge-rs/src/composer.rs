//! Read the state of the CLI's input box off a captured pane, so a nudge is
//! typed as text and not as commands.

/// How many lines up from the last line of the pane the status line may sit.
///
/// Codex draws it under the composer, with a blank line between them and at
/// most a line of key hints beneath: it is the last line or the one above.
/// Looking no higher is the point. The words "Vim: Normal" further up are
/// conversation -- a transcript quoting them, or a draft that ends in them --
/// and not the state of the input box. Blank lines count toward the distance,
/// since the one above the status line is what keeps the composer out of it.
const STATUS_LINES: usize = 2;

/// The modes Codex's status line names when its Vim keys are on.
const VIM_MODES: [&str; 3] = ["Normal", "Insert", "Replace"];

/// Whether the pane's composer is waiting in Vim Normal mode.
///
/// In Normal mode what is typed is read as commands: seen live, "please
/// continue" pasted, moved the cursor, and from its first "a" on was inserted,
/// leaving "se continue". The lowest line that ends in a mode is the status
/// line, and it alone decides.
pub fn vim_normal(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let end = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |i| i + 1);
    lines[end.saturating_sub(STATUS_LINES)..end]
        .iter()
        .rev()
        .find_map(|line| {
            VIM_MODES.iter().find(|mode| {
                line.strip_suffix(*mode)
                    .and_then(|rest| rest.strip_suffix("Vim: "))
                    .is_some_and(|before| before.is_empty() || before.ends_with(' '))
            })
        })
        .is_some_and(|mode| *mode == "Normal")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bottom of a parked Codex pane from a live 80-column session on
    /// 2026-10-12, after Esc had left its composer in Normal mode. Verbatim but
    /// for the path on the status line, which is abridged.
    const NORMAL: &str = "\
› please continue

• Working (3s • esc to interrupt)

› Ask Codex to do anything

  GPT-6.1-Sol xhigh · ~/src                                        Vim: Normal
  ← for agents · ? for shortcuts
";

    #[test]
    fn reads_vim_normal_off_the_captured_codex_status_line() {
        assert!(vim_normal(NORMAL));
    }

    /// With text in the composer Codex drops the hints, and the status line is
    /// the last thing on screen.
    #[test]
    fn reads_it_when_the_status_line_is_the_last_line() {
        let pane = "› draft\n\n  GPT-6.1-Sol xhigh · ~/src                   Vim: Normal\n\n";
        assert!(vim_normal(pane));
    }

    /// The same pane as it normally waits, and the other mode Codex names.
    /// Typing lands as text in both.
    #[test]
    fn insert_and_replace_are_not_normal() {
        assert!(!vim_normal(&NORMAL.replace("Vim: Normal", "Vim: Insert")));
        assert!(!vim_normal(&NORMAL.replace("Vim: Normal", "Vim: Replace")));
    }

    /// A CLI with no Vim keys says nothing about them at all.
    #[test]
    fn a_pane_that_names_no_mode_is_not_normal() {
        assert!(!vim_normal("❯ \n\n  ? for shortcuts\n"));
        assert!(!vim_normal(""));
    }

    /// The words alone prove nothing. A pane can be *about* Vim Normal mode --
    /// a transcript discussing this very check is -- and typing an "i" into it
    /// would put a stray letter in front of the nudge.
    #[test]
    fn the_words_higher_up_the_pane_are_conversation_not_state() {
        let pane = "\
● The status line says                                             Vim: Normal
● so nudge presses i first.
● Done.

❯

  ? for shortcuts
";
        assert!(!vim_normal(pane));
    }

    /// Nor do they count in a draft, when the status line under it says
    /// otherwise. The lowest line that names a mode is the status line.
    #[test]
    fn a_draft_that_ends_in_the_words_does_not_outvote_the_status_line() {
        let pane = "\
› the footer said Vim: Normal

  GPT-6.1-Sol xhigh · ~/src                                        Vim: Insert
";
        assert!(!vim_normal(pane));
    }

    /// With no hints under it, the status line is the last line and the draft
    /// is two lines up. If that status line names no mode at all, nothing but
    /// distance keeps the draft's words from being read as one.
    #[test]
    fn a_draft_above_a_status_line_that_names_no_mode_is_not_state() {
        let pane = "› note to self: Vim: Normal\n\n  GPT-6.1-Sol xhigh · ~/src\n";
        assert!(!vim_normal(pane));
    }

    /// A short footer leaves the transcript's last line close to the bottom.
    /// Counting only lines with text on them reached it; the blank lines a
    /// CLI draws around its input box are part of the distance.
    #[test]
    fn a_transcript_line_just_above_a_short_footer_is_still_conversation() {
        let pane = "\
● The status line then said                                        Vim: Normal

❯

  ? for shortcuts
";
        assert!(!vim_normal(pane));
    }

    /// And on the status line they must be its last word, where Codex puts
    /// them, not something a path or a branch name happens to contain.
    #[test]
    fn the_words_must_end_the_status_line() {
        let pane = "› \n\n  GPT-6.1-Sol · ~/notes/Vim: Normal mode.md       Vim: Insert\n";
        assert!(!vim_normal(pane));
    }
}
