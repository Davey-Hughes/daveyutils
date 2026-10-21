//! The LLM step: a backend is any command that reads the prompt on stdin and prints the reply on
//! stdout. Presets cover claude, codex (gpt) and ollama; `--llm-cmd` covers anything else. claude
//! and codex print their reply as JSON, with what the call used.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::group;
use crate::text::{Line, timestamp};

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
    pub output: Output,
}

/// How a backend prints its reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The reply alone: ollama, and any `--llm-cmd`.
    Text,
    /// `claude -p --output-format json`: one object, the reply in `result`, with the call's tokens
    /// and its price.
    Claude,
    /// `codex exec --json`: an event a line, the reply in the last agent message, the call's
    /// tokens in `turn.completed`. No price.
    Codex,
}

/// The system prompt for presets that take one; the instructions themselves go on stdin.
const SYSTEM: &str = "You are a careful English proofreader. Follow the reply format exactly.";

impl Backend {
    /// `claude`, `gpt` or `ollama:<model>`.
    pub fn preset(name: &str) -> Result<Backend, String> {
        let (argv, output): (Vec<&str>, Output) = match name {
            // Without these flags a headless call loads the user's hooks, skills, CLAUDE.md and
            // memory: 31,648 tokens of context per call, measured, against 605 with them.
            "claude" => (
                vec![
                    "claude",
                    "-p",
                    "--model",
                    "sonnet",
                    "--output-format",
                    "json",
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
                Output::Claude,
            ),
            // `--disable hooks`: the user's Codex hooks (tmux session tracking) would otherwise
            // fire on every chunk.
            "gpt" => (
                vec![
                    "codex",
                    "exec",
                    "--disable",
                    "hooks",
                    "--sandbox",
                    "read-only",
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "--json",
                    "-",
                ],
                Output::Codex,
            ),
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
                    output: Output::Text,
                });
            }
        };
        Ok(Backend {
            label: name.to_string(),
            argv: argv.iter().map(ToString::to_string).collect(),
            chunk_lines: CHUNK_LINES,
            output,
        })
    }

    /// Any command, run through `sh -c`.
    pub fn shell(command: &str) -> Backend {
        Backend {
            label: format!("cmd:{command}"),
            argv: vec!["sh".into(), "-c".into(), command.into()],
            chunk_lines: CHUNK_LINES,
            output: Output::Text,
        }
    }

    /// The backend a run file records by its label.
    pub fn from_label(label: &str) -> Result<Backend, String> {
        match label.strip_prefix("cmd:") {
            Some(command) => Ok(Backend::shell(command)),
            None => Backend::preset(label),
        }
    }

    /// One call: the prompt in, the reply out, read as the backend prints it.
    pub fn call(&self, prompt: &str, timeout: Duration) -> Answer {
        let failed = match run(&self.argv, prompt, timeout) {
            Ok(stdout) => return self.output.read(&stdout),
            Err(failed) => failed,
        };
        let said = self.output.read_json(&failed.stdout);
        // The exit status and stderr, then the backend's reason when its JSON gives one: claude
        // out of its session says why there alone, with nothing on stderr.
        let reply = match said.as_ref().and_then(|s| s.reason.as_deref()) {
            Some(why) => format!(
                "{}; {} said: {why:.200}",
                failed.message,
                self.output.name()
            ),
            None => failed.message,
        };
        // Every call that started counts, with what its JSON says it used, if anything.
        let used = match (self.output, said.and_then(|s| s.usage)) {
            (Output::Text, _) => Used::Untracked,
            _ if !failed.started => Used::Untracked,
            (_, Some(usage)) => Used::Reported(usage),
            (_, None) => Used::Unreported,
        };
        Answer {
            reply: Err(reply),
            used,
        }
    }
}

/// What one call used, as its backend reported it. A run file holds it as
/// [`crate::store::LlmCall`] does.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    /// All of its input, read from a cache or not. (claude's own `input_tokens` leaves out the
    /// cached part; this is that plus both cache counts.)
    pub input_tokens: u64,
    /// With any the model spent thinking.
    pub output_tokens: u64,
    /// Of the input, how much was read from the backend's prompt cache, and how much written to
    /// it, when it says.
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    /// The call's price at the provider's API rates, when the backend says (claude does, codex
    /// does not). On a subscription that is not what is billed.
    pub cost_usd: Option<f64>,
}

/// What a call used, as far as can be told.
#[derive(Debug, Clone, PartialEq)]
pub enum Used {
    Reported(Usage),
    /// It started, and its backend said nothing of what it used: killed at the timeout, printing
    /// nothing or something other than its JSON, or JSON with no usage in it. It may have cost.
    Unreported,
    /// Nothing to count: a backend that never reports (ollama, `--llm-cmd`), or a call that never
    /// started.
    Untracked,
}

/// A call's reply, read from what the backend printed.
#[derive(Debug, PartialEq)]
pub struct Answer {
    /// The reply for [`parse`], or why there is none.
    pub reply: Result<String, String>,
    pub used: Used,
}

/// What a backend's JSON says of a call.
struct Said {
    /// The reply; with none, the error for a backend that exited 0.
    reply: Result<String, String>,
    /// Why it failed, in its own words, when it says.
    reason: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ClaudeReply {
    #[serde(default)]
    is_error: bool,
    result: Option<String>,
    subtype: Option<String>,
    total_cost_usd: Option<f64>,
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)] // claude's names
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

/// The events of `codex exec --json` that are read; the rest are passed over.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum CodexEvent {
    #[serde(rename = "item.completed")]
    ItemCompleted { item: CodexItem },
    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: CodexUsage },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: CodexError },
    /// Not always the end, so no reason for one: codex says so when it reconnects, too.
    #[serde(rename = "error")]
    Error(CodexError),
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct CodexItem {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)] // codex's names
struct CodexUsage {
    /// Cached input included, as OpenAI counts it.
    #[serde(default)]
    input_tokens: u64,
    cached_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    /// Reasoning included.
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
struct CodexError {
    message: String,
}

impl Output {
    /// The reply and what the call used, from what the backend printed.
    pub fn read(self, stdout: &str) -> Answer {
        let what = match self {
            Output::Text => {
                return Answer {
                    reply: Ok(stdout.to_string()),
                    used: Used::Untracked,
                };
            }
            Output::Claude => "claude's JSON",
            Output::Codex => "codex's events",
        };
        self.read_json(stdout).map_or_else(
            || {
                let first = stdout.trim().lines().next().unwrap_or("(empty)");
                Answer {
                    reply: Err(format!("reply is not {what}: {first:.80}")),
                    used: Used::Unreported,
                }
            },
            |said| Answer {
                reply: said.reply,
                used: said.usage.map_or(Used::Unreported, Used::Reported),
            },
        )
    }

    /// The backend, as an error names it.
    fn name(self) -> &'static str {
        match self {
            Output::Text => "the command",
            Output::Claude => "claude",
            Output::Codex => "codex",
        }
    }

    /// As [`Output::read`], for a backend that prints JSON; `None` for text, or output that is
    /// not this backend's JSON at all.
    fn read_json(self, stdout: &str) -> Option<Said> {
        match self {
            Output::Text => None,
            Output::Claude => read_claude(stdout),
            Output::Codex => read_codex(stdout),
        }
    }
}

/// Whether `cost` can be a price: finite, and not below 0. In claude's reply one that is not is
/// taken as no price at all; in a run file it makes the record unreadable.
pub fn is_price(cost: f64) -> bool {
    cost.is_finite() && cost >= 0.0
}

fn read_claude(stdout: &str) -> Option<Said> {
    let r: ClaudeReply = serde_json::from_str(stdout).ok()?;
    let says = |why: &String| !why.trim().is_empty();
    let usage = (r.usage.is_some() || r.total_cost_usd.is_some()).then(|| {
        let u = r.usage.as_ref();
        let read = u.and_then(|u| u.cache_read_input_tokens);
        let written = u.and_then(|u| u.cache_creation_input_tokens);
        Usage {
            input_tokens: u
                .map_or(0, |u| u.input_tokens)
                .saturating_add(read.unwrap_or(0))
                .saturating_add(written.unwrap_or(0)),
            output_tokens: u.map_or(0, |u| u.output_tokens),
            cache_read_tokens: read,
            cache_write_tokens: written,
            cost_usd: r.total_cost_usd.filter(|&c| is_price(c)),
        }
    });
    let (reply, reason) = if r.is_error {
        // Its result, else its subtype, which says `success` even of some errors.
        let subtype = r.subtype.filter(|s| says(s) && s != "success");
        let reason = r.result.filter(says).or(subtype);
        let reply = match &reason {
            Some(why) => Err(format!("claude reported an error: {why:.200}")),
            None => Err("claude reported an error".into()),
        };
        (reply, reason)
    } else {
        // As `claude -p` prints it without --output-format.
        let reply = r.result.map(|result| format!("{result}\n"));
        (
            reply.ok_or_else(|| "claude's reply has no result".into()),
            None,
        )
    };
    Some(Said {
        reply,
        reason,
        usage,
    })
}

/// `None` when no line is an event.
fn read_codex(stdout: &str) -> Option<Said> {
    let mut seen = false;
    let mut message = None;
    let mut failed = None;
    let mut error = None;
    let mut usage: Option<Usage> = None;
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<CodexEvent>(line) else {
            continue;
        };
        seen = true;
        match event {
            CodexEvent::ItemCompleted { item } if item.kind == "agent_message" => {
                message = item.text;
            }
            CodexEvent::TurnCompleted { usage: u } => {
                let sum = usage.get_or_insert_with(Usage::default);
                sum.input_tokens = sum.input_tokens.saturating_add(u.input_tokens);
                sum.output_tokens = sum.output_tokens.saturating_add(u.output_tokens);
                let add = |a: Option<u64>, b: Option<u64>| match (a, b) {
                    (None, None) => None,
                    _ => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
                };
                sum.cache_read_tokens = add(sum.cache_read_tokens, u.cached_input_tokens);
                sum.cache_write_tokens = add(sum.cache_write_tokens, u.cache_write_input_tokens);
            }
            CodexEvent::TurnFailed { error: e } => failed = Some(e.message),
            CodexEvent::Error(e) => error = Some(e.message),
            CodexEvent::ItemCompleted { .. } | CodexEvent::Other => {}
        }
    }
    // Its turn's failure, else its last error.
    let reason = failed.or(error);
    let reply = match (message, &reason) {
        // As `codex exec` prints its final message without --json.
        (Some(text), _) => Ok(format!("{text}\n")),
        (None, Some(why)) => Err(format!("codex: {why:.200}")),
        (None, None) => Err("codex gave no message".into()),
    };
    seen.then_some(Said {
        reply,
        reason,
        usage,
    })
}

/// What a run's LLM calls used, summed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spent {
    pub calls: usize,
    /// Of them, those whose backend said nothing of what they used: the sums leave them out.
    pub unreported: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The calls that reported a price, and its sum.
    pub priced: usize,
    pub cost_usd: f64,
}

impl Spent {
    /// One call, with what it used if its backend said.
    pub fn add(&mut self, usage: Option<&Usage>) {
        self.calls += 1;
        let Some(u) = usage else {
            self.unreported += 1;
            return;
        };
        self.input_tokens = self.input_tokens.saturating_add(u.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(u.output_tokens);
        if let Some(cost) = u.cost_usd {
            self.priced += 1;
            self.cost_usd += cost;
        }
    }

    /// `98 calls`, `1.23M in / 31.2k out tokens`, and `$3.21 at API prices` when a call had a
    /// price. Calls that reported nothing are said (`98 calls (2 reported no usage)`), and any
    /// call without a price makes the price a floor (`at least $3.21`). Nothing with no calls.
    pub fn parts(&self) -> Vec<String> {
        self.calls_part()
            .into_iter()
            .chain(self.tokens_part())
            .chain(self.price_part())
            .collect()
    }

    /// `98 calls`, `98 calls (2 reported no usage)`, `98 calls (none reported usage)`; `None` with
    /// no calls.
    pub fn calls_part(&self) -> Option<String> {
        let calls = match self.calls {
            0 => return None,
            1 => "1 call".to_string(),
            n => format!("{n} calls"),
        };
        Some(match self.unreported {
            0 => calls,
            n if n == self.calls => format!("{calls} (none reported usage)"),
            n => format!("{calls} ({n} reported no usage)"),
        })
    }

    /// `1.23M in / 31.2k out tokens`; `None` when no call reported.
    pub fn tokens_part(&self) -> Option<String> {
        (self.calls > self.unreported).then(|| {
            format!(
                "{} in / {} out tokens",
                tokens(self.input_tokens),
                tokens(self.output_tokens)
            )
        })
    }

    /// `$3.21 at API prices`, `at least` that when any call has no price (it reported none, or
    /// nothing at all); `None` when no call has one.
    pub fn price_part(&self) -> Option<String> {
        if self.priced == 0 {
            return None;
        }
        let floor = if self.priced < self.calls {
            "at least "
        } else {
            ""
        };
        Some(format!("{floor}{} at API prices", dollars(self.cost_usd)))
    }

    /// `LLM: ` and the [parts](Spent::parts) joined by `sep`; `None` when no call was recorded (a
    /// run from before calls were, or a backend that reports nothing: ollama, `--llm-cmd`).
    pub fn line(&self, sep: &str) -> Option<String> {
        let parts = self.parts();
        (!parts.is_empty()).then(|| format!("LLM: {}", parts.join(sep)))
    }
}

/// A token count to three figures: `980`, `31.2k`, `1.23M`.
fn tokens(n: u64) -> String {
    // A count that rounds to 1000 of a unit is shown in the next.
    let (scale, unit): (u128, &str) = match n {
        0..1_000 => return n.to_string(),
        1_000..999_500 => (1_000, "k"),
        999_500..999_500_000 => (1_000_000, "M"),
        _ => (1_000_000_000, "G"),
    };
    let n = u128::from(n);
    // Decimals for three figures, judged on the value as it will round.
    let rounded = |decimals: u32| {
        let ten = 10u128.pow(decimals);
        (n * ten + scale / 2) / scale
    };
    let hundredths = rounded(2);
    if hundredths < 1_000 {
        return format!("{}.{:02}{unit}", hundredths / 100, hundredths % 100);
    }
    let tenths = rounded(1);
    if tenths < 1_000 {
        return format!("{}.{}{unit}", tenths / 10, tenths % 10);
    }
    format!("{}{unit}", rounded(0))
}

/// Dollars to the cent, or to two figures under ten cents, judged as they round: `$3.21`,
/// `$0.10`, `$0.013`, `$0.0025`; to the millionth at most, so `<$0.000001` below it.
fn dollars(x: f64) -> String {
    if x <= 0.0 {
        return format!("${x:.2}");
    }
    let mut shown = String::new();
    for decimals in 2..=6 {
        shown = format!("{x:.decimals$}");
        // Its figures: what is left after the leading zeros and the point.
        if shown.trim_start_matches(['0', '.']).len() >= 2 {
            break;
        }
    }
    if shown.trim_start_matches(['0', '.']).is_empty() {
        return "<$0.000001".into();
    }
    format!("${shown}")
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
    if let Some(rest) = s.strip_prefix('[')
        && let Some(end) = rest.find(']')
        && rest[..end]
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b':' || b == b'.')
    {
        s = rest[end + 1..].trim_start();
    }
    let style = format!("({})", line.style);
    s.strip_prefix(style.as_str()).map_or(s, str::trim_start)
}

/// After the command exits, how long its output may take to end. Something it left running in the
/// background and holding stdout gets this long, then its process group is killed: the reply is
/// already complete by then.
const GRACE: Duration = Duration::from_secs(2);

/// Why a command gave no reply, and what it printed on stdout all the same.
#[derive(Debug)]
struct Failed {
    message: String,
    stdout: String,
    /// It started, so it may have cost.
    started: bool,
}

impl Failed {
    /// Nothing on stdout: it never `started`, or was killed.
    fn with_nothing(message: String, started: bool) -> Failed {
        Failed {
            message,
            stdout: String::new(),
            started,
        }
    }
}

/// Run `argv` with `input` on stdin, returning stdout; an error names the exit status or the
/// timeout, plus the last line of stderr, and keeps what the command printed on stdout.
///
/// The command runs in its own process group, so a timeout kills everything it started (a wrapper
/// script's children, or what dash forks for `sh -c`), not just the direct child. That takes it out
/// of the terminal's foreground group, where Ctrl-C would reach it, so when SIGINT, SIGTERM or
/// SIGHUP ends subcheck, [`group`] kills every group still running.
fn run(argv: &[String], input: &str, timeout: Duration) -> Result<String, Failed> {
    let Some((program, rest)) = argv.split_first() else {
        return Err(Failed::with_nothing("empty command".into(), false));
    };
    let (mut child, _running) = group::spawn(
        Command::new(program)
            .args(rest)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .map_err(|e| Failed::with_nothing(format!("cannot run {program}: {e}"), false))?;
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
        if let Some(status) = child
            .try_wait()
            .map_err(|e| Failed::with_nothing(e.to_string(), true))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            group::kill(&child);
            let _ = child.wait();
            // The pipe threads finish on their own once the group is gone; not joined, in case
            // something left the group and still holds a pipe.
            let message = format!("{program} timed out after {}s", timeout.as_secs());
            return Err(Failed::with_nothing(message, true));
        }
        thread::sleep(Duration::from_millis(50));
    };
    let grace_end = Instant::now() + GRACE;
    while !(stdout.is_finished() && stderr.is_finished()) && Instant::now() < grace_end {
        thread::sleep(Duration::from_millis(20));
    }
    if !(stdout.is_finished() && stderr.is_finished()) {
        group::kill(&child);
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
        // With nothing on stderr, no `: ` before nothing.
        let said = if last_err.is_empty() {
            String::new()
        } else {
            format!(": {last_err:.200}")
        };
        Err(Failed {
            message: format!("{program} exited with {status}{said}"),
            stdout: out,
            started: true,
        })
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
        assert!(c.argv.windows(2).any(|w| w == ["--output-format", "json"]));
        assert_eq!(c.output, Output::Claude);
        let g = Backend::preset("gpt").unwrap();
        assert_eq!(&g.argv[..4], ["codex", "exec", "--disable", "hooks"]);
        assert_eq!(&g.argv[g.argv.len() - 2..], ["--json", "-"]);
        assert_eq!(g.output, Output::Codex);
        let o = Backend::preset("ollama:gemma4:12b-it-qat").unwrap();
        assert_eq!(o.output, Output::Text);
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
        assert_eq!(Backend::shell("my-llm --x").output, Output::Text);
    }

    /// A real reply of `claude -p --output-format json` (2.1.304), with fields it does not read
    /// trimmed.
    const CLAUDE: &str = include_str!("../tests/fixtures/llm/claude.json");
    /// A real reply of `codex exec --json` (0.163.0), whole.
    const CODEX: &str = include_str!("../tests/fixtures/llm/codex.jsonl");

    #[test]
    fn claudes_json_gives_the_reply_as_plain_claude_prints_it_and_what_the_call_used() {
        let a = Output::Claude.read(CLAUDE);
        // `claude -p` without --output-format prints the result and a newline.
        assert_eq!(a.reply.as_deref(), Ok("NONE\n"));
        assert_eq!(
            a.used,
            Used::Reported(Usage {
                // 2 uncached, 622 written to the cache, 0 read from it.
                input_tokens: 624,
                output_tokens: 5,
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(622),
                cost_usd: Some(0.002_542),
            })
        );
    }

    #[test]
    fn a_claude_price_that_is_no_price_is_none_and_the_tokens_are_kept() {
        let negative = CLAUDE.replace(r#""total_cost_usd":0.002542"#, r#""total_cost_usd":-0.5"#);
        let used = Output::Claude.read(&negative).used;
        let u = reported(&used).unwrap();
        assert_eq!((u.cost_usd, u.output_tokens), (None, 5));
        // JSON cannot write the rest, but a price must be a finite one.
        for cost in [f64::NAN, f64::INFINITY, -0.01] {
            assert!(!is_price(cost), "{cost}");
        }
        assert!(is_price(0.0) && is_price(0.002_542));
    }

    #[test]
    fn a_claude_error_fails_the_call_and_keeps_what_it_used() {
        let failed = CLAUDE
            .replace(r#""is_error":false"#, r#""is_error":true"#)
            .replace(
                r#""result":"NONE""#,
                r#""result":"API Error: 529 overloaded""#,
            );
        let a = Output::Claude.read(&failed);
        assert_eq!(
            a.reply,
            Err("claude reported an error: API Error: 529 overloaded".into())
        );
        assert_eq!(reported(&a.used).map(|u| u.output_tokens), Some(5));
        // A blank result and a subtype of `success` say nothing.
        let a = Output::Claude.read(r#"{"is_error":true,"result":"","subtype":"success"}"#);
        assert_eq!(a.reply, Err("claude reported an error".into()));
        let a = Output::Claude.read("Not logged in\n");
        assert_eq!(
            a.reply,
            Err("reply is not claude's JSON: Not logged in".into())
        );
        assert_eq!(a.used, Used::Unreported);
    }

    #[test]
    fn codexs_events_give_the_last_message_and_the_turns_tokens_without_a_price() {
        let a = Output::Codex.read(CODEX);
        assert_eq!(a.reply.as_deref(), Ok("NONE\n"));
        assert_eq!(
            a.used,
            Used::Reported(Usage {
                // codex counts cached input in its input, as this record does.
                input_tokens: 17_413,
                output_tokens: 59,
                cache_read_tokens: Some(12_288),
                cache_write_tokens: Some(0),
                cost_usd: None,
            })
        );
        // An earlier message gives way to the last; a line that is no event is passed over.
        let two = CODEX.replace(
            r#"{"type":"turn.completed""#,
            "not json\n{\"type\":\"item.completed\",\"item\":{\"id\":\"item_1\",\"type\":\"agent_message\",\"text\":\"L1\\tfixed\\twhy\"}}\n{\"type\":\"turn.completed\"",
        );
        assert_eq!(
            Output::Codex.read(&two).reply.as_deref(),
            Ok("L1\tfixed\twhy\n")
        );
    }

    #[test]
    fn codex_with_no_message_fails_the_call_with_its_error() {
        let failed = "{\"type\":\"turn.started\"}\n{\"type\":\"error\",\"message\":\"stream disconnected\"}\n{\"type\":\"turn.failed\",\"error\":{\"message\":\"usage limit reached\"}}\n";
        let a = Output::Codex.read(failed);
        assert_eq!(a.reply, Err("codex: usage limit reached".into()));
        assert_eq!(a.used, Used::Unreported);
        let a = Output::Codex.read("{\"type\":\"turn.started\"}\n");
        assert_eq!(a.reply, Err("codex gave no message".into()));
        // Exiting 0 with no reply: its last error, alone.
        let a = Output::Codex.read("{\"type\":\"error\",\"message\":\"stream disconnected\"}\n");
        assert_eq!(a.reply, Err("codex: stream disconnected".into()));
        let a = Output::Codex.read("");
        assert_eq!(a.reply, Err("reply is not codex's events: (empty)".into()));
    }

    /// A JSON backend that prints `stdout`, says `stderr` (when there is any), and exits 1.
    fn failing(output: Output, stdout: &str, stderr: &str) -> Answer {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::write(&out, stdout).unwrap();
        let say = if stderr.is_empty() {
            String::new()
        } else {
            format!("echo '{stderr}' >&2; ")
        };
        let cmd = format!("cat '{}'; {say}exit 1", out.display());
        Backend {
            output,
            ..Backend::shell(&cmd)
        }
        .call("", TIMEOUT)
    }

    #[test]
    fn a_failed_call_says_its_exit_status_and_stderr_and_then_the_backends_reason() {
        let status = "sh exited with exit status: 1: Error: not logged in";
        let codex = |events: &str| failing(Output::Codex, events, "Error: not logged in").reply;
        // No reason in its JSON: the exit status and stderr alone.
        assert_eq!(
            codex(r#"{"type":"thread.started","thread_id":"t"}"#),
            Err(status.to_string())
        );
        // codex's turn.failed, else its last error, after them.
        let failed = r#"{"type":"turn.failed","error":{"message":"usage limit reached"}}"#;
        let error = |m: &str| format!(r#"{{"type":"error","message":"{m}"}}"#);
        assert_eq!(
            codex(failed),
            Err(format!("{status}; codex said: usage limit reached"))
        );
        assert_eq!(
            codex(&format!("{}\n{failed}", error("stream disconnected"))),
            Err(format!("{status}; codex said: usage limit reached"))
        );
        assert_eq!(
            codex(&format!(
                "{}\n{}",
                error("stream disconnected"),
                error("retrying")
            )),
            Err(format!("{status}; codex said: retrying"))
        );
        // claude's non-blank result, else its non-blank subtype unless that is "success".
        let claude = |json: &str| failing(Output::Claude, json, "Error: not logged in").reply;
        let claude_error = |result: &str, subtype: &str| {
            claude(&format!(
                r#"{{"type":"result","is_error":true,"result":"{result}","subtype":"{subtype}"}}"#
            ))
        };
        assert_eq!(
            claude_error("API Error: 529 overloaded", "success"),
            Err(format!("{status}; claude said: API Error: 529 overloaded"))
        );
        assert_eq!(
            claude_error(" ", "error_max_turns"),
            Err(format!("{status}; claude said: error_max_turns"))
        );
        assert_eq!(claude_error("", "success"), Err(status.to_string()));
        // A reply, not an error: no reason, and what the call used is kept.
        let a = failing(Output::Claude, CLAUDE, "Error: not logged in");
        assert_eq!(a.reply, Err(status.to_string()));
        assert_eq!(reported(&a.used).map(|u| u.output_tokens), Some(5));
    }

    #[test]
    fn claude_out_of_its_session_says_why_after_its_exit_status() {
        // Its reason is in its JSON alone, nothing on stderr; what it used is kept.
        let limit = CLAUDE
            .replace(r#""is_error":false"#, r#""is_error":true"#)
            .replace(
                r#""result":"NONE""#,
                r#""result":"You've hit your session limit""#,
            );
        let a = failing(Output::Claude, &limit, "");
        assert_eq!(
            a.reply,
            Err("sh exited with exit status: 1; claude said: You've hit your session limit".into())
        );
        assert_eq!(reported(&a.used).map(|u| u.output_tokens), Some(5));
        // Text on stdout is never taken for a reason.
        let a = Backend::shell("echo NONE; exit 1").call("", TIMEOUT);
        assert_eq!(a.reply, Err("sh exited with exit status: 1".into()));
    }

    #[test]
    fn plain_text_is_the_reply_and_says_nothing_of_usage() {
        let a = Output::Text.read("L1\tfixed\twhy\n");
        assert_eq!(
            a,
            Answer {
                reply: Ok("L1\tfixed\twhy\n".into()),
                used: Used::Untracked
            }
        );
    }

    #[test]
    fn a_call_reads_its_reply_as_the_backend_prints_it() {
        let claude = Backend {
            output: Output::Claude,
            ..Backend::shell("cat")
        };
        let a = claude.call(CLAUDE, TIMEOUT);
        assert_eq!(a.reply.as_deref(), Ok("NONE\n"));
        assert!(reported(&a.used).is_some());
        let a = Backend::shell("exit 3").call("", TIMEOUT);
        assert!(a.reply.unwrap_err().contains("exit status: 3"));
        assert_eq!(a.used, Used::Untracked);
    }

    #[test]
    fn every_call_a_json_backend_started_is_counted_with_numbers_or_without() {
        let claude = |cmd: &str| Backend {
            output: Output::Claude,
            ..Backend::shell(cmd)
        };
        // Killed at the timeout.
        let a = claude("sleep 5; echo late").call("", Duration::from_millis(200));
        assert!(a.reply.unwrap_err().contains("timed out"));
        assert_eq!(a.used, Used::Unreported);
        // Not its JSON, exiting 0 or not; nothing on stdout; its JSON with no usage in it.
        for cmd in [
            "echo hello",
            "echo hello; exit 1",
            "echo 'Error: not logged in' >&2; exit 1",
            r#"echo '{"type":"result","is_error":true,"result":"no"}'; exit 1"#,
        ] {
            assert_eq!(
                claude(cmd).call("", TIMEOUT).used,
                Used::Unreported,
                "{cmd}"
            );
        }
        let codex = Backend {
            output: Output::Codex,
            ..Backend::shell(r#"echo '{"type":"thread.started","thread_id":"t"}'; exit 1"#)
        };
        assert_eq!(codex.call("", TIMEOUT).used, Used::Unreported);
        // It never started: nothing to count.
        let gone = Backend {
            argv: vec!["/nonexistent/claude".into()],
            ..claude("")
        };
        let a = gone.call("", TIMEOUT);
        assert!(a.reply.unwrap_err().contains("cannot run"));
        assert_eq!(a.used, Used::Untracked);
        // A backend that never reports: nothing to count, whatever happened.
        for (cmd, timeout) in [
            ("sleep 5; echo late", Duration::from_millis(200)),
            ("exit 1", TIMEOUT),
        ] {
            assert_eq!(
                Backend::shell(cmd).call("", timeout).used,
                Used::Untracked,
                "{cmd}"
            );
        }
    }

    fn reported(used: &Used) -> Option<&Usage> {
        match used {
            Used::Reported(u) => Some(u),
            Used::Unreported | Used::Untracked => None,
        }
    }

    fn usage(input: u64, output: u64, cost: Option<f64>) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cost_usd: cost,
            ..Usage::default()
        }
    }

    #[test]
    fn spent_sums_the_calls_and_their_price_a_floor_when_a_call_has_none() {
        let mut s = Spent::default();
        assert_eq!(s.line(", "), None, "no calls recorded");
        s.add(Some(&usage(1_200_000, 30_000, Some(3.0))));
        s.add(Some(&usage(34_567, 1_234, Some(0.21))));
        assert_eq!(
            s.line(", ").as_deref(),
            Some("LLM: 2 calls, 1.23M in / 31.2k out tokens, $3.21 at API prices")
        );
        assert_eq!(
            s.parts(),
            [
                "2 calls",
                "1.23M in / 31.2k out tokens",
                "$3.21 at API prices"
            ]
        );
        // A call with no price: the sum is short of it, and says so.
        s.add(Some(&usage(10, 1, None)));
        assert_eq!(
            s.line(" · ").as_deref(),
            Some("LLM: 3 calls · 1.23M in / 31.2k out tokens · at least $3.21 at API prices")
        );
        // No call with a price (codex): none.
        let mut codex = Spent::default();
        codex.add(Some(&usage(1500, 40, None)));
        assert_eq!(
            codex.line(", ").as_deref(),
            Some("LLM: 1 call, 1.50k in / 40 out tokens")
        );
        let mut one = Spent::default();
        one.add(Some(&usage(624, 5, Some(0.002_542))));
        assert_eq!(
            one.line(", ").as_deref(),
            Some("LLM: 1 call, 624 in / 5 out tokens, $0.0025 at API prices")
        );
    }

    #[test]
    fn calls_that_reported_nothing_are_counted_and_make_the_price_a_floor() {
        let mut s = Spent::default();
        s.add(Some(&usage(1203, 40, Some(0.0126))));
        s.add(None);
        assert_eq!(
            s.line(", ").as_deref(),
            Some(
                "LLM: 2 calls (1 reported no usage), 1.20k in / 40 out tokens, \
                 at least $0.013 at API prices"
            )
        );
        // None reported: no sums to show.
        let mut none = Spent::default();
        for _ in 0..200 {
            none.add(None);
        }
        assert_eq!(
            none.line(", ").as_deref(),
            Some("LLM: 200 calls (none reported usage)")
        );
    }

    #[test]
    fn token_counts_read_to_three_figures() {
        let cases = [
            (0, "0"),
            (999, "999"),
            (1_000, "1.00k"),
            (1_234, "1.23k"),
            (9_994, "9.99k"),
            (9_995, "10.0k"),
            (31_249, "31.2k"),
            (99_950, "100k"),
            (999_499, "999k"),
            (999_500, "1.00M"),
            (1_234_567, "1.23M"),
            (12_345_678_901, "12.3G"),
            (u64::MAX, "18446744074G"),
        ];
        for (n, shown) in cases {
            assert_eq!(tokens(n), shown, "{n}");
        }
    }

    #[test]
    fn dollars_keep_two_figures_under_ten_cents() {
        let cases = [
            (3.214, "$3.21"),
            (0.1, "$0.10"),
            (0.0126, "$0.013"),
            (0.002_542, "$0.0025"),
            (0.0, "$0.00"),
            // Two figures as rounded, not before.
            (0.0999, "$0.10"),
            (0.009_99, "$0.010"),
            // Under a millionth of a dollar is not free.
            (0.000_003, "$0.000003"),
            (0.000_000_5, "<$0.000001"),
        ];
        for (x, shown) in cases {
            assert_eq!(dollars(x), shown, "{x}");
        }
    }

    #[test]
    fn a_backend_comes_back_from_the_label_a_run_records() {
        for label in [
            "claude",
            "gpt",
            "ollama:gemma4:12b-it-qat",
            "cmd:my-llm --x",
        ] {
            let b = Backend::from_label(label).unwrap();
            assert_eq!(b.label, label);
        }
        assert_eq!(
            Backend::from_label("cmd:my-llm --x").unwrap().argv,
            Backend::shell("my-llm --x").argv
        );
        assert!(Backend::from_label("gemini").is_err());
    }

    #[test]
    fn run_passes_stdin_through() {
        let argv = Backend::shell("tr a-z A-Z").argv;
        assert_eq!(run(&argv, "hello\n", TIMEOUT).unwrap(), "HELLO\n");
    }

    #[test]
    fn run_reports_exit_status_and_stderr() {
        let argv = Backend::shell("echo boom >&2; exit 3").argv;
        let err = run(&argv, "", TIMEOUT).unwrap_err().message;
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
        let err = run(&argv, "", Duration::from_millis(200))
            .unwrap_err()
            .message;
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
        let err = run(&Backend::shell(&cmd).argv, "", Duration::from_millis(300))
            .unwrap_err()
            .message;
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
