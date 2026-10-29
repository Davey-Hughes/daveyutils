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
    let captured = read_styled(screen);
    let lines: Vec<&str> = captured.all.iter().map(|l| l.trim_end()).collect();
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

/// A CLI's input box, read off a pane captured with its styling
/// (`capture-pane -e`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBox {
    /// What is typed in it, whitespace squeezed. Text drawn dim is left out: a
    /// placeholder, or Claude's grey suggestion of what to say next, is not
    /// input.
    pub text: String,
    pub kind: BoxKind,
}

/// Which CLI drew the box, and what it says about its Vim mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxKind {
    /// Claude Code, with the label it draws under the box.
    Claude { mode: ClaudeMode },
    /// Codex, whose mode is on its status line instead (see [`vim_normal`]).
    Codex,
}

/// The label under a Claude Code box, Claude's only word on its Vim mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeMode {
    /// `-- INSERT --`
    Insert,
    /// `-- VISUAL --`
    Visual,
    /// No label: Normal mode, or a box with its Vim keys off.
    Unlabelled,
}

/// How many lines Codex may draw under its box. A live capture on 2026-10-28
/// had two, its status line and a line of key hints; the third is slack. The
/// lowest `›` with more than this under it is a sent message in the history,
/// with something else covering the box.
const CODEX_UNDER: usize = 3;

/// The pane's input box, if it shows one nudge knows how to read.
///
/// Claude's is the line starting `❯` that sits between two rules, the lowest
/// such. Its glyph also marks messages in the history and the selected agent
/// in the list under the box, but neither sits between rules. Codex's is the
/// last line starting `›`, with any lines under it up to a blank one.
pub fn input_box(screen: &str) -> Option<InputBox> {
    let captured = read_styled(screen);
    claude_box(&captured).or_else(|| codex_box(&captured))
}

/// [`InputBox::holds`] for a whole pane: a pane with no box holds nothing.
#[cfg(test)]
fn holds(screen: &str, msg: &str) -> bool {
    input_box(screen).is_some_and(|b| b.holds(msg))
}

impl InputBox {
    /// Whether `msg` is still sitting in this box: what the box holds ends
    /// with it. Compared with all whitespace taken out, since a long message
    /// wraps where the pane's width says.
    pub fn holds(&self, msg: &str) -> bool {
        let squeeze = |s: &str| s.split_whitespace().collect::<String>();
        let msg = squeeze(msg);
        !msg.is_empty() && squeeze(&self.text).ends_with(&msg)
    }
}

fn claude_box(captured: &Captured) -> Option<InputBox> {
    let rules: Vec<usize> = (0..captured.all.len())
        .filter(|&i| captured.all[i].trim_start().starts_with("──"))
        .collect();
    rules.windows(2).rev().find_map(|pair| {
        let (first, rule_under) = (pair[0] + 1, pair[1]);
        (first < rule_under && captured.all[first].starts_with('❯')).then(|| InputBox {
            text: box_text(&captured.solid[first..rule_under], '❯'),
            kind: BoxKind::Claude {
                mode: match captured.all.get(rule_under + 1) {
                    Some(footer) if footer.contains("-- INSERT --") => ClaudeMode::Insert,
                    Some(footer) if footer.contains("-- VISUAL --") => ClaudeMode::Visual,
                    _ => ClaudeMode::Unlabelled,
                },
            },
        })
    })
}

fn codex_box(captured: &Captured) -> Option<InputBox> {
    let first = captured.all.iter().rposition(|l| l.starts_with('›'))?;
    let end = captured.all[first..]
        .iter()
        .position(|l| l.trim().is_empty())
        .map_or(captured.all.len(), |n| first + n);
    let under = captured.all[end..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    if under > CODEX_UNDER {
        return None;
    }
    Some(InputBox {
        text: box_text(&captured.solid[first..end], '›'),
        kind: BoxKind::Codex,
    })
}

/// The box's lines as one string, its prompt glyph taken off.
fn box_text(lines: &[String], glyph: char) -> String {
    let joined = lines.join(" ");
    let text = joined.trim_start();
    text.strip_prefix(glyph)
        .unwrap_or(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A styled capture, line for line, read two ways: all of each line's text,
/// and only the text not drawn dim.
struct Captured {
    all: Vec<String>,
    solid: Vec<String>,
}

/// Take a capture's escapes out, keeping track of which text was dim.
///
/// tmux writes SGR sequences (`ESC [ … m`) into a styled capture, and they
/// are the only ones dimness lives in. Any other escape is skipped whole.
fn read_styled(screen: &str) -> Captured {
    let mut all = vec![String::new()];
    let mut solid = vec![String::new()];
    let mut dim = false;
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                all.push(String::new());
                solid.push(String::new());
            }
            '\x1b' => match chars.next() {
                // CSI: parameters, then a final byte in @..~.
                Some('[') => {
                    let mut params = String::new();
                    while let Some(p) = chars.next_if(|p| !('@'..='~').contains(p)) {
                        params.push(p);
                    }
                    if chars.next() == Some('m') {
                        dim = sgr_dim(&params, dim);
                    }
                }
                // OSC (a hyperlink, say): up to BEL or ESC \.
                Some(']') => {
                    while let Some(p) = chars.next() {
                        if p == '\x07' || (p == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            c => {
                all.last_mut().unwrap().push(c);
                if !dim {
                    solid.last_mut().unwrap().push(c);
                }
            }
        }
    }
    Captured { all, solid }
}

/// Whether text is dim after the SGR parameters `params`, given whether it was.
fn sgr_dim(params: &str, mut dim: bool) -> bool {
    let mut ps = params.split(';');
    while let Some(p) = ps.next() {
        match p {
            "" | "0" | "22" => dim = false,
            "2" => dim = true,
            // Extended colours carry numbers of their own, `38;5;n` and
            // `38;2;r;g;b`, and a 2 among them is a colour, not dim.
            "38" | "48" | "58" => match ps.next() {
                Some("5") => {
                    ps.next();
                }
                Some("2") => {
                    ps.nth(2);
                }
                _ => {}
            },
            _ => {}
        }
    }
    dim
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

    // ---- the input box ----

    /// The bottom of an idle Claude Code pane with its Vim keys on, as captured
    /// live on 2026-10-28 (the rules shortened). The prompt glyph is followed
    /// by a no-break space.
    const CLAUDE_INSERT: &str = "\
✻ Brewed for 27s · done 10:29 PM

────────────────────────────────────────
❯\u{a0}
────────────────────────────────────────
  -- INSERT -- ⏵⏵ auto mode on (shift+tab to cycle) · ← 2 agents
";

    /// Claude Code labels Insert and Visual mode under the box and leaves
    /// Normal mode unlabelled, the same as it looks with its Vim keys off.
    fn claude_normal() -> String {
        CLAUDE_INSERT.replace("-- INSERT -- ", "")
    }

    fn claude(mode: ClaudeMode) -> BoxKind {
        BoxKind::Claude { mode }
    }

    #[test]
    fn reads_a_claude_box_and_its_insert_label() {
        assert_eq!(
            input_box(CLAUDE_INSERT),
            Some(InputBox {
                text: String::new(),
                kind: claude(ClaudeMode::Insert),
            })
        );
        assert_eq!(
            input_box(&claude_normal()).unwrap().kind,
            claude(ClaudeMode::Unlabelled)
        );
        let visual = CLAUDE_INSERT.replace("-- INSERT --", "-- VISUAL --");
        assert_eq!(input_box(&visual).unwrap().kind, claude(ClaudeMode::Visual));
    }

    #[test]
    fn what_is_typed_in_the_box_is_its_text() {
        let pane = CLAUDE_INSERT.replace("❯\u{a0}", "❯\u{a0}please   continue");
        assert_eq!(input_box(&pane).unwrap().text, "please continue");
    }

    #[test]
    fn a_draft_over_several_lines_is_read_whole() {
        let pane = CLAUDE_INSERT.replace("❯\u{a0}", "❯\u{a0}one\n  two");
        assert_eq!(input_box(&pane).unwrap().text, "one two");
    }

    /// Claude's grey suggestion of what to say next, as captured live with
    /// `capture-pane -e` on 2026-10-28: drawn dim, and not input. Read as
    /// input, it is a nudge's own message still sitting unsent in the box.
    #[test]
    fn dim_text_in_the_box_is_not_input() {
        let pane = CLAUDE_INSERT.replace("❯\u{a0}", "\x1b[39m❯\u{a0}\x1b[2mplease continue\x1b[0m");
        assert_eq!(input_box(&pane).unwrap().text, "");
    }

    /// A 24-bit colour is `38;2;r;g;b`, and that 2 is part of the colour.
    /// Taken for "dim", it would hide text that is really there. The escapes
    /// are a live capture's.
    #[test]
    fn a_colour_whose_numbers_include_a_2_is_not_dim() {
        let pane = CLAUDE_INSERT.replace(
            "❯\u{a0}",
            "\x1b[38;2;130;139;184m❯\u{a0}\x1b[38;2;200;211;245myes\x1b[39m",
        );
        assert_eq!(input_box(&pane).unwrap().text, "yes");
    }

    /// A Claude pane running agents lists them under the box, and the selected
    /// one is marked with the same glyph as the prompt. As captured live.
    #[test]
    fn the_box_is_the_one_between_the_rules_not_a_glyph_below_them() {
        let pane = "\
────────────────────────────────────────
❯\u{a0}
────────────────────────────────────────
  -- INSERT -- ⏵⏵ auto mode on · 1 shell · ← 2 agents
  ● main
❯ ◯ general-purpose  Handing back ll-task-8 fix round
";
        assert_eq!(
            input_box(pane),
            Some(InputBox {
                text: String::new(),
                kind: claude(ClaudeMode::Insert),
            })
        );
    }

    /// A session named with `/rename` is drawn into the top rule, as captured
    /// live.
    #[test]
    fn a_name_on_the_top_rule_still_bounds_the_box() {
        let pane = CLAUDE_INSERT.replacen(
            "────────────────────────────────────────",
            "────────────────────────────── ra-scan ─",
            1,
        );
        assert_eq!(input_box(&pane).unwrap().kind, claude(ClaudeMode::Insert));
    }

    /// Claude draws a submitted message with the prompt glyph too. Only the
    /// one between the rules is the box.
    #[test]
    fn a_message_in_the_history_is_not_the_box() {
        let pane = format!("❯ please continue\n\n● On it.\n\n{CLAUDE_INSERT}");
        assert_eq!(input_box(&pane).unwrap().text, "");
    }

    #[test]
    fn reads_a_codex_box() {
        let pane = NORMAL.replace("Ask Codex to do anything", "please continue");
        assert_eq!(
            input_box(&pane),
            Some(InputBox {
                text: "please continue".into(),
                kind: BoxKind::Codex,
            })
        );
    }

    #[test]
    fn a_pane_with_no_box_has_none() {
        assert_eq!(input_box("$ ls\nnotes.md\n$ \n"), None);
    }

    /// The Vim check now reads the same styled capture as the box does.
    #[test]
    fn vim_normal_reads_a_styled_capture() {
        let pane = NORMAL.replace("Vim: Normal", "\x1b[2mVim: Normal\x1b[0m");
        assert!(vim_normal(&pane));
    }

    // ---- whether a message is still in the box ----

    fn claude_holding(typed: &str) -> String {
        CLAUDE_INSERT.replace("❯\u{a0}", &format!("❯\u{a0}{typed}"))
    }

    #[test]
    fn a_message_still_in_the_box_is_held() {
        assert!(holds(&claude_holding("please continue"), "please continue"));
        // Typed after a draft, it is the end of what the box holds.
        assert!(holds(
            &claude_holding("draft please continue"),
            "please continue"
        ));
    }

    #[test]
    fn a_submitted_message_is_not_held() {
        assert!(!holds(CLAUDE_INSERT, "please continue"));
        // Claude draws it in the history, with the same glyph, once sent.
        let sent = format!("❯ please continue\n\n{CLAUDE_INSERT}");
        assert!(!holds(&sent, "please continue"));
    }

    /// After a turn, Claude can suggest in grey the very message nudge sent.
    /// That is not the message waiting unsent.
    #[test]
    fn a_dim_suggestion_of_the_same_words_is_not_held() {
        let pane = claude_holding("\x1b[2mplease continue\x1b[0m");
        assert!(!holds(&pane, "please continue"));
    }

    /// A long message wraps in the box, and where it wraps is the pane's width.
    #[test]
    fn a_message_the_box_wrapped_is_still_held() {
        let pane = claude_holding("limits reset, please\n  continue the review");
        assert!(holds(&pane, "limits reset, please continue the review"));
    }

    #[test]
    fn a_codex_box_holding_the_message_holds_it() {
        let pane = NORMAL.replace("Ask Codex to do anything", "please continue");
        assert!(holds(&pane, "please continue"));
        assert!(!holds(NORMAL, "please continue"));
    }

    #[test]
    fn a_pane_with_no_box_holds_nothing() {
        assert!(!holds("$ please continue\n", "please continue"));
        assert!(!holds(&claude_holding("x"), ""));
    }

    /// Codex draws a sent message in its history with the box's glyph. When
    /// something else covers the bottom of the pane (an approval, a popup),
    /// the lowest glyph is that history line, and it is not the box.
    #[test]
    fn a_codex_history_line_above_other_output_is_not_the_box() {
        let pane = "\
› please continue

• Working (1s • esc to interrupt)

  Allow command?
  $ cargo test
  1. Yes
  2. No
";
        assert_eq!(input_box(pane), None);
        assert!(!holds(pane, "please continue"));
    }
}
