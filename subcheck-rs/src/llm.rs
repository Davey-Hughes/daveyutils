//! The LLM step: a backend is any command that reads the prompt on stdin and prints the reply on
//! stdout. Presets cover claude, codex (gpt) and ollama; `--llm-cmd` covers anything else.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::text::{timestamp, Line};

/// Stored in every run file, so findings can be traced to the prompt that produced them.
pub const PROMPT_VERSION: u32 = 1;

/// One chunk's run limit; a chunk that takes longer is killed, retried once, then failed.
pub const TIMEOUT: Duration = Duration::from_mins(10);

/// Lines per chunk unless `--chunk-lines` says otherwise. Ollama defaults to a 4k context on
/// cards under 24 GiB and truncates silently, and 150 lines (about 2.5k tokens with the
/// instructions) fit it with room for the reply.
pub const CHUNK_LINES: usize = 400;
pub const CHUNK_LINES_OLLAMA: usize = 150;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    /// What the run file records: `claude`, `gpt`, `ollama:<model>` or `cmd:<command>`.
    pub label: String,
    pub argv: Vec<String>,
    pub chunk_lines: usize,
}

/// The system prompt for presets that take one; the instructions themselves go on stdin.
const SYSTEM: &str = "You are a careful English proofreader. Follow the reply format exactly.";

impl Backend {
    /// `claude`, `gpt` or `ollama:<model>`.
    pub fn preset(name: &str) -> Result<Backend, String> {
        let argv: Vec<&str> = match name {
            // Without these flags a headless call loads the user's hooks, skills, CLAUDE.md and
            // memory: 31,648 tokens of context per call, measured, against 605 with them.
            "claude" => vec![
                "claude",
                "-p",
                "--model",
                "sonnet",
                "--tools",
                "",
                "--setting-sources",
                "",
                "--disable-slash-commands",
                "--strict-mcp-config",
                "--no-session-persistence",
                "--system-prompt",
                SYSTEM,
            ],
            // `--disable hooks`: the user's Codex hooks (tmux session tracking) would otherwise
            // fire on every chunk. codex prints only the final message on stdout.
            "gpt" => vec![
                "codex",
                "exec",
                "--disable",
                "hooks",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--ephemeral",
                "-",
            ],
            _ => {
                let Some(model) = name.strip_prefix("ollama:").filter(|m| !m.is_empty()) else {
                    return Err(format!(
                        "unknown --llm {name:?}: use claude, gpt, ollama:<model>, or --llm-cmd"
                    ));
                };
                // Thinking off: gemma4 spent its whole budget thinking and replied with nothing.
                // No word wrap: it would break reply lines at the terminal width.
                let argv = ["ollama", "run", "--think=false", "--nowordwrap", model];
                return Ok(Backend {
                    label: name.to_string(),
                    argv: argv.iter().map(ToString::to_string).collect(),
                    chunk_lines: CHUNK_LINES_OLLAMA,
                });
            }
        };
        Ok(Backend {
            label: name.to_string(),
            argv: argv.iter().map(ToString::to_string).collect(),
            chunk_lines: CHUNK_LINES,
        })
    }

    /// Any command, run through `sh -c`.
    pub fn shell(command: &str) -> Backend {
        Backend {
            label: format!("cmd:{command}"),
            argv: vec!["sh".into(), "-c".into(), command.into()],
            chunk_lines: CHUNK_LINES,
        }
    }
}

const INSTRUCTIONS: &str = "\
You are proofreading English subtitles for spelling mistakes.

Each line below is one subtitle: an ID, its start time, its style in parentheses, then the text.

Report only:
- misspelled words and typos (\"befor\" for \"before\", \"teh\" for \"the\");
- a real word used in place of the one meant (\"trough\" where \"through\" was meant, \"your\" for \"you're\");
- accidentally doubled or missing words.

Do not report:
- names, honorifics (-san, -chan, -senpai, Sensei) or any other Japanese, romanized or not;
- deliberate stutters, cut-off words and sounds (\"S-Such\", \"Mm-hmm\", \"Whoa\");
- slang, dialect, informal grammar, style, punctuation or capitalization;
- British versus American spelling;
- anything you are not sure is a mistake.

Some lines come from karaoke effects and may be fragments; skip any you cannot read as English.

Reply with one line per mistake and nothing else, fields separated by a TAB:
L<ID><TAB><the whole line, corrected><TAB><a few words on what was wrong>
If there are no mistakes, reply with the single word NONE.

Lines:
";

const CLOSING: &str = "\n(End of lines. Reply only in the format above, or NONE.)\n";

/// The prompt for one chunk. Line IDs are the chunk's own, `L1` to `Ln` in order.
pub fn prompt(lines: &[&Line]) -> String {
    let mut out = String::from(INSTRUCTIONS);
    for (i, line) in lines.iter().enumerate() {
        let _ = writeln!(
            out,
            "L{} [{}] ({}) {}",
            i + 1,
            timestamp(line.start_ms()),
            line.style,
            line.text
        );
    }
    out.push_str(CLOSING);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// Index into the chunk's lines.
    pub index: usize,
    pub corrected: String,
    pub reason: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reply {
    pub suggestions: Vec<Suggestion>,
    /// Finding lines naming an ID the chunk does not have.
    pub unknown_ids: usize,
    /// Findings whose correction equals the original.
    pub no_ops: usize,
}

/// Read a reply leniently: `L<ID>` first, then the corrected line and the reason separated by a
/// TAB. A model that echoes the line's `[time] (style)` prefix gets it stripped (gemma4 does).
/// A reply with neither a finding line nor `NONE` is an error: that is what a backend that only
/// "thought", or printed something else entirely, looks like.
pub fn parse(reply: &str, lines: &[&Line]) -> Result<Reply, String> {
    let mut out = Reply::default();
    let mut saw_none = false;
    let mut saw_finding = false;
    for raw in reply.lines() {
        let raw = raw.trim();
        if is_none(raw) {
            saw_none = true;
            continue;
        }
        let Some(rest) = raw.strip_prefix('L') else {
            continue;
        };
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let Ok(id) = rest[..digits].parse::<usize>() else {
            continue;
        };
        saw_finding = true;
        let Some(line) = id.checked_sub(1).and_then(|i| lines.get(i)) else {
            out.unknown_ids += 1;
            continue;
        };
        let rest = rest[digits..].trim_start_matches([':', ' ', '\t']);
        let mut fields = rest.splitn(2, '\t');
        let corrected = strip_echo(fields.next().unwrap_or_default().trim(), line);
        let reason = fields.next().unwrap_or_default().trim().replace('\t', " ");
        if corrected.is_empty() || corrected == line.text {
            out.no_ops += 1;
            continue;
        }
        out.suggestions.push(Suggestion {
            index: id - 1,
            corrected: corrected.to_string(),
            reason,
        });
    }
    if !saw_finding && !saw_none {
        let first = reply.trim().lines().next().unwrap_or("(empty)");
        return Err(format!("reply has no findings and no NONE: {first:.80}"));
    }
    Ok(out)
}

/// Whether a reply line says "no mistakes": the word NONE alone, or dressed in punctuation or
/// markdown ("NONE.", "**NONE**", "None - no mistakes found."), as small models write it. A word
/// that merely starts with it ("Nonetheless") is not NONE.
fn is_none(line: &str) -> bool {
    let word = line.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    word.get(..4)
        .is_some_and(|w| w.eq_ignore_ascii_case("none"))
        && !word[4..].starts_with(|c: char| c.is_ascii_alphanumeric())
}

/// The corrected text without an echoed `[0:02:45.06] (OP en) ` prefix.
fn strip_echo<'a>(s: &'a str, line: &Line) -> &'a str {
    let mut s = s;
    if let Some(rest) = s.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            if rest[..end]
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b':' || b == b'.')
            {
                s = rest[end + 1..].trim_start();
            }
        }
    }
    let style = format!("({})", line.style);
    s.strip_prefix(style.as_str()).map_or(s, str::trim_start)
}

/// After the command exits, how long its output may take to end. Something it left running in the
/// background and holding stdout gets this long, then its process group is killed: the reply is
/// already complete by then.
const GRACE: Duration = Duration::from_secs(2);

/// Run `argv` with `input` on stdin, returning stdout; an error names the exit status or the
/// timeout, plus the last line of stderr.
///
/// The command runs in its own process group, so a timeout kills everything it started (a wrapper
/// script's children, or what dash forks for `sh -c`), not just the direct child. That takes it out
/// of the terminal's foreground group, where Ctrl-C would reach it, so [`group`] forwards SIGINT,
/// SIGTERM and SIGHUP to every group still running.
pub fn run(argv: &[String], input: &str, timeout: Duration) -> Result<String, String> {
    let Some((program, rest)) = argv.split_first() else {
        return Err("empty command".into());
    };
    let mut child = Command::new(program)
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    let pgid = i32::try_from(child.id()).unwrap_or(0);
    let _running = group::register(pgid);
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let input = input.to_string();
    // Written from its own thread: a child that replies before reading all its input would
    // otherwise deadlock against a full pipe.
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let reader = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let stdout = reader(Box::new(child.stdout.take().expect("stdout was piped")));
    let stderr = reader(Box::new(child.stderr.take().expect("stderr was piped")));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            group::kill(pgid);
            let _ = child.wait();
            // The pipe threads finish on their own once the group is gone; not joined, in case
            // something left the group and still holds a pipe.
            return Err(format!("{program} timed out after {}s", timeout.as_secs()));
        }
        thread::sleep(Duration::from_millis(50));
    };
    let grace_end = Instant::now() + GRACE;
    while !(stdout.is_finished() && stderr.is_finished()) && Instant::now() < grace_end {
        thread::sleep(Duration::from_millis(20));
    }
    if !(stdout.is_finished() && stderr.is_finished()) {
        group::kill(pgid);
    }
    let _ = writer.join();
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    let last_err = err
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    if status.success() {
        Ok(out)
    } else {
        Err(format!("{program} exited with {status}: {last_err:.200}"))
    }
}

/// The process groups of LLM commands still running, so a signal that ends subcheck ends them too.
mod group {
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::Once;

    /// More than `-j` will ever want; a command that finds no free slot just is not forwarded to.
    const SLOTS: usize = 64;
    static RUNNING: [AtomicI32; SLOTS] = [const { AtomicI32::new(0) }; SLOTS];
    static INSTALL: Once = Once::new();

    /// A registered group; dropping it frees the slot.
    pub struct Registered(Option<usize>);

    impl Drop for Registered {
        fn drop(&mut self) {
            if let Some(i) = self.0 {
                RUNNING[i].store(0, Ordering::SeqCst);
            }
        }
    }

    pub fn register(pgid: i32) -> Registered {
        INSTALL.call_once(install);
        if pgid <= 0 {
            return Registered(None);
        }
        Registered(RUNNING.iter().position(|slot| {
            slot.compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        }))
    }

    pub fn kill(pgid: i32) {
        if pgid > 0 {
            // SAFETY: kill only reads its integer arguments.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }

    /// Kills every running group, then dies of the signal as if there were no handler. Only
    /// async-signal-safe calls: atomic loads, kill, signal, raise.
    extern "C" fn on_signal(sig: libc::c_int) {
        for slot in &RUNNING {
            let pgid = slot.load(Ordering::SeqCst);
            if pgid > 0 {
                // SAFETY: as in `kill`.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
        // SAFETY: restoring the default action and re-raising are async-signal-safe.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    fn install() {
        let handler = on_signal as extern "C" fn(libc::c_int);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: `on_signal` makes only async-signal-safe calls.
            unsafe {
                libc::signal(sig, handler as libc::sighandler_t);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Occurrence;

    fn line(style: &str, text: &str, start_ms: i64) -> Line {
        Line {
            style: style.into(),
            text: text.into(),
            occurrences: vec![Occurrence {
                track: 0,
                start_ms,
                end_ms: start_ms + 1000,
            }],
        }
    }

    #[test]
    fn prompt_numbers_lines_from_one_with_time_and_style() {
        let a = line("K1Norma", "befor it came", 779_080);
        let b = line("OP en", "gets trough", 165_060);
        let p = prompt(&[&a, &b]);
        assert!(p.starts_with("You are proofreading English subtitles"));
        assert!(p.contains(
            "\nL1 [0:12:59.08] (K1Norma) befor it came\nL2 [0:02:45.06] (OP en) gets trough\n"
        ));
        assert!(p.ends_with("(End of lines. Reply only in the format above, or NONE.)\n"));
    }

    #[test]
    fn parse_reads_tab_separated_findings() {
        let a = line("D", "befor it came", 0);
        let b = line("D", "fine", 0);
        let reply = "L1\tbefore it came\t\"befor\" misspelled\n";
        let r = parse(reply, &[&a, &b]).unwrap();
        assert_eq!(
            r.suggestions,
            [Suggestion {
                index: 0,
                corrected: "before it came".into(),
                reason: "\"befor\" misspelled".into()
            }]
        );
    }

    #[test]
    fn parse_strips_an_echoed_time_and_style() {
        let a = line("OP en", "gets trough to", 0);
        let reply = "L1 [0:02:45.06] (OP en) gets through to\tthrough for trough\n\n";
        let r = parse(reply, &[&a]).unwrap();
        assert_eq!(r.suggestions[0].corrected, "gets through to");
        assert_eq!(r.suggestions[0].reason, "through for trough");
    }

    #[test]
    fn parse_counts_unknown_ids_and_no_ops() {
        let a = line("D", "fine", 0);
        let reply = "L1\tfine\tnothing\nL9\tghost\tnot a line\n";
        let r = parse(reply, &[&a]).unwrap();
        assert_eq!(r.suggestions, [] as [Suggestion; 0]);
        assert_eq!((r.unknown_ids, r.no_ops), (1, 1));
    }

    #[test]
    fn none_is_a_clean_reply_and_chatter_alone_is_an_error() {
        let a = line("D", "fine", 0);
        assert_eq!(parse("NONE\n", &[&a]).unwrap(), Reply::default());
        assert!(parse("\n\n", &[&a]).is_err());
        assert!(parse("Here are my thoughts about the lines.", &[&a]).is_err());
    }

    #[test]
    fn none_survives_punctuation_and_markdown() {
        let a = line("D", "fine", 0);
        for reply in [
            "NONE.",
            "**NONE**",
            "`NONE`",
            "None - no mistakes found.",
            "  none\n",
        ] {
            assert_eq!(parse(reply, &[&a]), Ok(Reply::default()), "{reply:?}");
        }
        // A word that merely starts with "none" is not NONE.
        assert!(parse("Nonetheless, all fine.", &[&a]).is_err());
    }

    #[test]
    fn presets() {
        let c = Backend::preset("claude").unwrap();
        assert_eq!(&c.argv[..4], ["claude", "-p", "--model", "sonnet"]);
        assert!(c.argv.contains(&"--setting-sources".to_string()));
        let g = Backend::preset("gpt").unwrap();
        assert_eq!(&g.argv[..4], ["codex", "exec", "--disable", "hooks"]);
        let o = Backend::preset("ollama:gemma4:12b-it-qat").unwrap();
        assert_eq!(
            o.argv,
            [
                "ollama",
                "run",
                "--think=false",
                "--nowordwrap",
                "gemma4:12b-it-qat"
            ]
        );
        assert_eq!(o.chunk_lines, CHUNK_LINES_OLLAMA);
        assert!(Backend::preset("ollama:").is_err());
        assert!(Backend::preset("gemini").is_err());
        assert_eq!(
            Backend::shell("my-llm --x").argv,
            ["sh", "-c", "my-llm --x"]
        );
    }

    #[test]
    fn run_passes_stdin_through() {
        let argv = Backend::shell("tr a-z A-Z").argv;
        assert_eq!(run(&argv, "hello\n", TIMEOUT).unwrap(), "HELLO\n");
    }

    #[test]
    fn run_reports_exit_status_and_stderr() {
        let argv = Backend::shell("echo boom >&2; exit 3").argv;
        let err = run(&argv, "", TIMEOUT).unwrap_err();
        assert!(
            err.contains("exit status: 3") && err.contains("boom"),
            "{err}"
        );
    }

    #[test]
    fn run_kills_a_command_that_overruns() {
        // Two commands, so every shell forks `sleep` instead of exec'ing it: the grandchild
        // outlives the killed shell and keeps stdout open. dash forks even a lone `sleep 5`,
        // which is how CI (Debian's /bin/sh) caught this while bash on the desktop did not.
        let argv = Backend::shell("sleep 5; echo late").argv;
        let started = Instant::now();
        let err = run(&argv, "", Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn run_does_not_wait_for_a_background_process_holding_stdout() {
        // The shell exits at once, but `sleep` inherited stdout and keeps it open.
        let argv = Backend::shell("sleep 30 & echo done").argv;
        let started = Instant::now();
        assert_eq!(run(&argv, "", TIMEOUT).unwrap(), "done\n");
        let took = started.elapsed();
        assert!(took < Duration::from_secs(10), "{took:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn run_kills_the_whole_process_group_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let cmd = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let err = run(&Backend::shell(&cmd).argv, "", Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        thread::sleep(Duration::from_millis(200));
        // Gone, or a zombie nobody has reaped yet (a container's PID 1 may not): either way no
        // longer running.
        let stat =
            std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())).unwrap_or_default();
        let state = stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next();
        assert!(
            matches!(state, None | Some("Z" | "X")),
            "sleep still running: {stat}"
        );
    }

    #[test]
    fn run_survives_a_command_that_ignores_its_input() {
        // 1 MB on stdin to a command that never reads it must not deadlock.
        let argv = Backend::shell("echo done").argv;
        let big = "x".repeat(1 << 20);
        assert_eq!(run(&argv, &big, TIMEOUT).unwrap(), "done\n");
    }
}
