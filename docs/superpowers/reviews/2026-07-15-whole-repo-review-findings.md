# Whole-repo review — confirmed findings (2026-07-15)

Multi-agent review: 43 findings raised across 7 dimensions, **40 confirmed** after independent adversarial verification, 3 refuted.

Each finding below survived a skeptic whose job was to refute it.

## CRITICAL (3)

### C1. `nudge-rs/src/ipc/server.rs`:24 — concurrency
**`serve()` unconditionally unlinks the socket before binding, and nothing else enforces a daemon singleton — so a second daemon silently steals the socket and two schedulers run concurrently against the same queue.json, double-firing jobs and clobbering each other's state.**

*Failure scenario:* No flock/pidfile/singleton check exists anywhere (grep for flock|lockfile|pidfile|singleton across src/ and tests/ returns nothing), and `let _ = std::fs::remove_file(socket)` discards the one signal that would have caught it (a live listener would otherwise make bind() fail with EADDRINUSE). Two ways in: (a) `app.rs:66 ensure_daemon` is TOCTOU — two concurrent `nudge -p a` / `nudge -p b` invocations both Ping, both get ECONNREFUSED, both spawn `--daemon`; (b) deterministically via the accept() bug below — the IPC thread dies, the daemon process stays alive, the next `nudge` Ping gets ECONNREFUSED and spawns daemon #2. Either way daemon B unlinks A's socket and binds its own. Both processes did `Queue::load(queue.json)` into *separate* in-memory `Queue`s and both run the scheduler loop. Job 1 is due -> A fires it AND B fires it: the user's messages are typed into the tmux pane twice. Worse, both call `Queue::save`, which writes the same fixed sibling temp path `queue.json.tmp` (queue.rs:88) and renames over queue.json — so A's save silently erases every job B accepted over the socket (and vice versa), and interleaved create/truncate/write on the shared temp name can publish a truncated file that makes the next `Queue::load` fail with InvalidData.

*Suggested fix:* Acquire an exclusive advisory lock (flock on a `nudge.lock` in the state dir) at daemon start and exit if held. Only unlink the socket after the lock is won (that proves the prior socket is stale). Make the temp file unique (e.g. include the pid) so concurrent writers can never share `queue.json.tmp`.


### C2. `nudge-rs/src/app.rs`:62 — data-loss
**Nothing enforces a single daemon: `ensure_daemon` races, and `serve()` unconditionally unlinks the socket, so a second daemon silently steals it while the first keeps running — and both rewrite the whole queue file, erasing each other's jobs.**

*Failure scenario:* There is no pidfile/flock/singleton guard anywhere (grep for flock/pidfile/single_instance finds nothing). Two paths produce two live daemons:

(a) Install-over-adhoc (deterministic, no race needed): user runs `nudge -p bot:0.1`, which auto-starts daemon A via ensure_daemon (app.rs:66-73). Later the user runs `nudge --install-daemon`; register::install runs `systemctl --user enable --now nudged.service`, starting daemon B. B's serve() hits `let _ = std::fs::remove_file(socket)` (ipc/server.rs:24), unlinks A's socket, and binds its own. Nothing ever kills A — it holds the old unlinked socket and keeps running its scheduler loop.

(b) Concurrent invocations: two `nudge -p ...` run at once, both Ping-fail at app.rs:63, both spawn `--daemon` (app.rs:67-73), second unlinks + rebinds.

Data loss then follows from queue.rs: both daemons did `Queue::load` into an independent in-memory `State`, and `save()` (queue.rs:84-95) serializes the *entire* state and renames over queue.json. Concretely: A and B both loaded {job1, job2}. Client schedules job3 → reaches B (B owns the socket) → B writes {1,2,3}. Job1 comes due → A fires it and calls apply_outcome → `q.remove(1)` → A writes its stale in-memory {2}. **Job3 is silently erased from disk and never fires**, and job1 may fire twice (both A and B have it). `nudge --list` shows the surviving set with no indication anything was dropped.

*Suggested fix:* Make the daemon a singleton. Either (1) in `serve()`, before unlinking, attempt `UnixStream::connect(socket)` — if it succeeds a daemon is already live, so exit instead of stealing the socket; only unlink on ECONNREFUSED/ENOENT (genuinely stale); and/or (2) hold an exclusive `flock` on a pidfile (or on queue.json) for the daemon's lifetime, exiting if the lock is held. Additionally have `ensure_daemon` take the same lock to close the spawn race, and have `install` detect/stop an already-running ad-hoc daemon before enabling the unit.


### C3. `scripts/batch_img2pdf`:78 — data-loss
**`--clean` runs `rm -rf "$d"` on the whole folder tree after building a PDF from only the folder's top-level images, silently destroying images in nested subfolders that were never converted.**

*Failure scenario:* MAINDIR contains `book/` laid out as `book/cover.jpg`, `book/chapter1/p1.jpg`, `book/chapter1/p2.jpg`, `book/chapter2/p1.jpg` (a very common comic/scan layout, and exactly what `unar` produces from a zip with a mixed root). The image loop at line 72 uses `find "$d" -maxdepth 1 -type f`, so `images=(book/cover.jpg)` only. That is non-empty, so the `continue` guard at line 73 does not fire. `img2pdf` succeeds on the single cover and exits 0, so line 78 fires and `rm -rf book/` deletes chapter1/ and chapter2/ — 3 source images that are in no PDF and have no other copy. The summary prints `done: 1 pdfs, 0 failed`, so the run looks like a clean success. The dir loop at line 83 is also `-maxdepth 1`, so those subfolders are never visited on any later pass either. Verified the find semantics against a real fixture tree; `--clean` has zero test coverage.

*Suggested fix:* Only `rm -rf "$d"` when the folder holds nothing but the images that went into the PDF. Cheapest correct guard: before cleaning, assert `find "$d" -mindepth 1 -type d -print -quit` is empty (no subdirs) and that the count of `find "$d" -type f` equals `${#images[@]}`; otherwise print a WARN that the folder was kept because it contains unconverted nested content. Alternatively recurse the image collection to match the delete's depth (`find "$d" -type f`) so the PDF actually covers everything `rm -rf` removes.


## IMPORTANT (21)

### I1. `nudge-rs/src/ipc/server.rs`:41 — error-handling
**Any `accept()` error — including the transient ECONNABORTED/EMFILE — permanently kills the IPC thread while the daemon process keeps running, leaving a live daemon with no control plane.**

*Failure scenario:* `accept()` fails transiently with ECONNABORTED (client vanished during handshake) or EMFILE/ENFILE (process/system fd limit hit). Both are conventionally retried; here the `Err(e)` arm does `return Err(e)`, `serve` returns, the spawned thread in daemon.rs:40-44 logs and exits. The scheduler loop is on a different thread and keeps firing jobs forever. The listener is dropped, so the socket file still exists but connect() now gives ECONNREFUSED: `nudge --list`, `--cancel <id>` and `--edit` all fail permanently, and the user can never cancel a pending nudge. Because the daemon was auto-started with `.stderr(Stdio::null())` (app.rs:72), the `tracing::error!("accept failed, stopping")` goes to /dev/null — the user gets no clue. This then feeds the critical finding above: the next `nudge` schedule Pings, gets ECONNREFUSED, and spawns a second daemon on top of the first.

*Suggested fix:* Log and `continue` on transient accept errors (ECONNABORTED, EINTR, EMFILE/ENFILE — with a short backoff for the fd-exhaustion cases) instead of returning. If `serve` ever does exit, it must take the whole daemon process down (or restart the thread) rather than leaving a headless daemon that still fires jobs.


### I2. `nudge-rs/src/scheduler.rs`:61 — correctness
**`apply_outcome`'s catch-all arm deletes the job when injection *fails*, so a single transient tmux error destroys a job the user explicitly asked to retry forever.**

*Failure scenario:* User runs the README's own advertised invocation `nudge -p bot:0.1 -a -r -1` (auto_retry=true, retries_left=-1, "retry forever"). At fire time `run_injection` returns Err because `tmux send-keys` transiently failed — the tmux server was restarting, the pane was momentarily unavailable, or the pane got renamed. The match arm at line 53 only handles `Ok(InjectOutcome::Sent(_))`, so `Err(_)` falls through to `_ => queue.remove(job.id)` and the job is deleted permanently. The user asked for infinite retries and instead gets zero: the nudge silently vanishes after one transient blip. The only trace is `tracing::warn!("job {} failed")` at daemon.rs:77, which is discarded because the auto-started daemon runs with `.stderr(Stdio::null())` (app.rs:72). Note this is distinct from the intentional `SkippedVerify` -> remove behavior, which is correct and tested.

*Suggested fix:* Add an explicit `Err(_)` arm: when `job.auto_retry && job.retries_left != 0`, reschedule at `retry_at` (decrementing as in the Sent path) instead of removing; only remove on Err once retries are exhausted. The existing tests never cover the Err path — add one.


### I3. `nudge-rs/src/ipc/server.rs`:33 — availability
**The accept loop handles connections serially and no socket anywhere in src/ sets a read timeout, so one client that connects without sending a newline wedges the daemon's entire IPC surface forever.**

*Failure scenario:* `serve`'s loop calls `handle_conn` inline (not on a worker thread), and `handle_conn` -> `read_msg` -> `read_line` (ipc/mod.rs:44) blocks until a newline or EOF. Grepping src/ for set_read_timeout|set_write_timeout|timeout|nonblocking returns nothing — no timeout is ever set on the accepted stream. So `nc -U $XDG_RUNTIME_DIR/nudge.sock` (or any local process that connects and writes a partial line like `abc` with no newline, then idles) blocks the loop indefinitely; control never returns to `accept()`. Every subsequent `nudge --list`, `--cancel`, and schedule hangs forever too — `client::request` (ipc/client.rs) has no timeout either, so the CLI hangs rather than erroring. The scheduler thread keeps firing jobs the user now has no way to cancel. Permanent until the daemon is killed. (A client killed *after* connecting is harmless: the fd closes and read_line returns 0 -> EOF, so this needs a client that stays alive holding the connection open.)

*Suggested fix:* Call `stream.set_read_timeout(Some(Duration::from_secs(5)))` and `set_write_timeout` in `handle_conn` before reading, and/or hand each connection to a short-lived worker thread so a stuck peer can't block `accept()`. Give `client::request` a connect/read timeout so the CLI fails fast instead of hanging.


### I4. `nudge-rs/src/queue.rs`:46 — correctness
**`Queue::add` mutates in-memory state before persisting and never rolls back on save failure, so the CLI reports the schedule was rejected while the daemon still fires the job.**

*Failure scenario:* `add` does `next_id += 1; jobs.push(...)` and only *then* `self.save()?`. The state dir is full (ENOSPC) or read-only, so `save()` fails and `add` returns Err. `handle_request` (ipc/mod.rs:56-59) turns that into `Response::Error(e)`, and `app.rs:103` prints `nudge: daemon rejected the job: No space left on device` and exits 1. The user reasonably concludes nothing is scheduled — but the job is sitting in the daemon's in-memory `state.jobs` and the scheduler loop will happily fire it into the pane at fire_at. `remove` and `reschedule` (lines 51-80) have the mirror-image bug: they mutate memory, fail to persist, and daemon.rs:88 only logs a warning (discarded to /dev/null for the auto-started daemon). The in-memory drop means the fired job won't re-fire in *this* process, but queue.json on disk still contains it, so a daemon restart within the 6h grace window reloads the already-fired job and fires it a second time.

*Suggested fix:* Build the candidate state, persist it, and only commit to `self.state` on success — or snapshot and roll back the mutation when `save()` returns Err — so an Err response always means "nothing changed".


### I5. `nudge-rs/src/detect.rs`:48 — correctness
**detect_reset picks the OLDEST banner in the pane, not the newest, because Regex::find returns the leftmost match and pane text is chronological top-to-bottom — yielding a silently wrong reset time.**

*Failure scenario:* Pane text (verified by running detect_reset against the built lib, now=10:00):

(1) Two duration banners — a stale one still on screen above the current one:
  "quota reached. Resets in 45m\n... hours of work ...\nquota reached. Resets in 3h"
  duration_re.find() matches the FIRST "quota reached", find_duration_token scans from there and grabs "45m".
  detect_reset => 10:48. Correct answer is 13:03 (the newest banner + 3m padding). The nudge fires ~2h early into a still-active limit.

(2) Cross-shape: the duration branch runs first unconditionally, regardless of where each banner sits in the pane:
  "quota reached. Resets in 45m\n<later>\ncurrent session resets 3:00pm"
  detect_reset => 10:48, never reaching the clock branch. Correct answer is 15:03.

The module comments show the author reasoned about scrollback contamination and scoped the token scan to `&clean[m.end()..]` — but the scoping only guards against junk ABOVE the banner. It does not answer "which banner", so a superseded banner still visible on screen wins over the live one.

*Suggested fix:* Select the LAST banner match rather than the first (`find_iter(&clean).last()`), and choose between the clock and duration shapes by whichever banner matches later in the text rather than by hardcoded branch order.


### I6. `nudge-rs/src/detect.rs`:29 — robustness
**User-supplied NUDGE_CLOCK_PATTERN / NUDGE_DURATION_PATTERN are interpolated into a regex and compiled with .expect(), so an invalid pattern panics instead of erroring — and the expect message misattributes the fault to a "built-in" regex.**

*Failure scenario:* `build_re` does `format!("(?i)(?:{base}|{e})")` where `e` is raw env input, then `Regex::new(&pattern).expect("valid built-in banner regex")`. Nothing validates `e`.

Verified: `detect_reset("current session resets 3:00pm", &now, Some("codex ("), None)` panics at src/detect.rs:29 with:
  `valid built-in banner regex: Syntax(... (?i)(?:(?:session limit|current session).*resets|codex () ... error: unclosed group)`

Two user-facing paths:
- `NUDGE_CLOCK_PATTERN='codex (' nudge -p bot:0.1` -> app::fire_time calls detect_reset -> CLI dies with a Rust panic + backtrace instead of "invalid NUDGE_CLOCK_PATTERN".
- The same typo'd env var in the daemon's environment: daemon::run passes clock_ext/dur_ext into run_injection for every `--verify` job. The first such job panics the scheduler loop, which runs on the daemon's main thread — the whole daemon dies and every pending job stops firing.

Any regex metacharacter a user reasonably types (`(`, `*`, `a[b` all confirmed to Err) triggers it; these read as plain text to someone writing a banner phrase.

*Suggested fix:* Compile the extension pattern once at startup and surface a clean error (`anyhow::bail!("invalid NUDGE_CLOCK_PATTERN: {e}")`), or fall back to the built-in pattern with a tracing::warn!. At minimum, replace `.expect()` with a Result so a bad env var cannot kill the daemon.


### I7. `nudge-rs/src/config.rs`:44 — correctness
**resolve() applies the retries override AFTER the auto_retry override, so `--retries N` silently re-enables auto-retry even when the user explicitly passed `--no-auto-retry`.**

*Failure scenario:* In resolve(), `overrides.auto_retry` is applied at line 38-40, then lines 44-47 run:
    if let Some(v) = overrides.retries {
        out.retries = v;
        out.auto_retry = true; // setting a retry count implies auto-retry
    }
The unconditional `= true` clobbers the explicit `false` that `--no-auto-retry` just wrote.

Verified against the built lib with NUDGE_* env cleared:
  `nudge -p x --retries 5 --no-auto-retry` => auto_retry=true, retries=5   (expected auto_retry=false)
  `nudge -p x --no-auto-retry`             => auto_retry=false, retries=2   (correct)

Consequence via app::build_spec line 38 (`retries_left: if opts.auto_retry { opts.retries } else { 0 }`): the job is persisted with auto_retry=true / retries_left=5, and scheduler::apply_outcome reschedules it 5 more times. The user asked for no retries and gets 5. --no-auto-retry is documented as "Disable auto-retry" (cli.rs:37) and this is the only spelling that disables it, so the flag silently does nothing whenever -r is also present. The same bug hits `nudge --edit ID --retries 5 --no-auto-retry` through app::merge_edit, which shares resolve().

*Suggested fix:* Only imply auto-retry when it was not explicitly overridden — e.g. set `out.retries = v;` unconditionally but guard the implication with `if overrides.auto_retry.is_none() { out.auto_retry = true; }`, or apply the auto_retry override last so an explicit flag always wins.


### I8. `nudge-rs/src/app.rs`:162 — correctness
**`nudge --edit <id> --auto-retry` silently produces a job with auto_retry=true but retries_left=0, which never retries.**

*Failure scenario:* `merge_edit` seeds its base from the job itself: `retries: job.retries_left` (app.rs:162). A job scheduled without auto-retry has retries_left == 0, because build_spec sets `retries_left: if opts.auto_retry { opts.retries } else { 0 }` (app.rs:38).

Now: `nudge --edit 5 --auto-retry` (no `-r`). overrides.retries is None, so config::resolve only flips `auto_retry = true` and leaves `retries = 0` (config.rs:38-47). merge_edit line 197 then computes `retries_left: if opts.auto_retry { opts.retries } else { 0 }` → 0. The replacement job is stored with auto_retry=true, retries_left=0.

When it fires, apply_outcome (scheduler.rs:53) guards on `job.auto_retry && job.retries_left != 0` → the 0 fails the guard → falls to the `_` arm → `queue.remove(job.id)`. The job is deleted and never retries. The CLI prints the cheerful `nudge: edited job 5 -> 6` and the user believes auto-retry is armed. A fresh `nudge -p x --auto-retry` gives retries_left=2 (the NUDGE_RETRIES default from cli.rs:96-99), so the same flag behaves inconsistently between schedule and edit. Workaround `--edit 5 -r 2` works, which hides the bug.

*Suggested fix:* In merge_edit, when the resolved auto_retry is being turned on but the job had no retry budget (job.retries_left == 0) and `--retries` was not passed, fall back to the default retry count rather than 0 — e.g. seed `base.retries` from the env/`NUDGE_RETRIES` default when `job.retries_left == 0`, or treat `tri(cli.auto_retry, cli.no_auto_retry) == Some(true)` with `cli.retries == None` as implying the default count.


### I9. `nudge-rs/src/app.rs`:143 — misleading-ux
**`--list`, `--cancel` and `--edit` never call `ensure_daemon`, so with the daemon down they fail with a raw errno — and a persisted job cannot be cancelled even though it will still fire.**

*Failure scenario:* `ensure_daemon` is called from exactly one place, `schedule` (app.rs:96); `list` (132), `cancel` (143) and `edit` (206) go straight to `client::request(&socket(), ...)`.

Jobs are persisted in queue.json (queue.rs:84-95) and survive a daemon exit/reboot. So: user schedules a job for 3pm, reboots (or the ad-hoc daemon dies — nothing restarts it unless `--install-daemon` was run). The socket file is gone but queue.json still holds the job. `nudge --list` → `UnixStream::connect` returns ENOENT → main.rs:7 prints `nudge: No such file or directory (os error 2)`. Same for `nudge --cancel 3`.

This is not just cosmetic: the user cannot cancel the job. It is still on disk, and as soon as anything starts the daemon again (e.g. the next `nudge -p ...`, which does auto-start it), `plan()` (scheduler.rs:24-43) sees it as due-within-grace (the daemon is invoked with a 6h grace at lib.rs:33) and **fires the injection the user tried to cancel**. The only escape is hand-editing queue.json.

*Suggested fix:* Call `ensure_daemon(&paths.socket)` at the top of list/cancel/edit as `schedule` does, so they auto-start the daemon and operate on the real persisted queue. At minimum, map a connect failure to a clear message ("nudge: daemon is not running") instead of surfacing the bare io::Error.


### I10. `nudge-rs/src/app.rs`:225 — correctness
**`edit`'s Schedule-then-Cancel is not atomic: if the Cancel leg fails or the process dies in the window, a duplicate job is left behind and the error message never mentions the replacement that was created.**

*Failure scenario:* edit deliberately schedules the replacement before cancelling the original (app.rs:220-231) so a Schedule failure can't lose the job — good direction, but the reverse hazard is unhandled and the IPC protocol has no atomic replace op (Request is only Schedule/List/Cancel/Ping, ipc/mod.rs:16-21).

Concretely: `nudge --edit 5 -m "now + 2 hours"`. Schedule succeeds → job 6 exists. Then line 225's `client::request(&socket(), &Request::Cancel(id))?` fails at the socket level — the daemon was killed/restarted in that window, or the socket was stolen by a second daemon (see the ensure_daemon finding). The `?` propagates, main.rs prints e.g. `nudge: Broken pipe (os error 32)`, exit 1. The user reads that as "the edit failed" — but jobs 5 AND 6 are both live in the queue, so **the message is injected twice**, once at the old time and once at the new one. The same duplicate results if the user Ctrl-Cs between the Schedule and the Cancel.

The `other =>` arm at line 230 gets this right (it names new_id in the error); the `?` on the same line does not.

*Suggested fix:* Add an atomic `Request::Replace { id, spec }` (or `Reschedule`) that the daemon applies under the queue lock, so edit is one round-trip. Short of that, catch the transport error on the Cancel leg and report it like line 230 does — naming new_id and telling the user job `id` is still pending — rather than letting `?` surface a bare errno.


### I11. `nudge-rs/src/cli.rs`:159 — test-flakiness
**Two unit tests in the same test binary race on the process-global NUDGE_NOTIFY env var, making the suite intermittently fail; the concurrent set_var/var is also undefined behavior.**

*Failure scenario:* cargo runs `no_flags_override_env_defaults` (line 136) and `no_notify_beats_a_bare_notify_env` (line 157) on parallel threads in one process. Test A does remove_var("NUDGE_NOTIFY") then resolve_options() then assert!(!t.notify). Test B does set_var("NUDGE_NOTIFY", "1"). If B's set_var lands between A's remove_var (line 140) and A's resolve_options (line 144), A reads NUDGE_NOTIFY=1 with no CLI override, resolve() returns notify=true, and A's assert!(!t.notify) fails. Verified empirically: replicating both test bodies against the real resolve_options across 200,000 concurrent interleavings produced 6,167 failures (~3%). The window is only a few instructions wide, so 500 sequential `cargo test --lib` runs did not reproduce it -- it will surface as a rare unexplained CI flake instead. Separately, std::env::set_var concurrent with std::env::var in another thread is documented UB (glibc can realloc the environ array under the reader); this is exactly why Rust 2024 made set_var unsafe, and this crate only compiles today because it is on edition 2021.

*Suggested fix:* Stop mutating process-global env in tests. resolve_options is already a thin wrapper over the pure config::resolve(&Toggles, &FlagOverrides), which cli.rs's own tests could call directly with an explicit Toggles -- config.rs's tests already do exactly this and are race-free. Alternatively, extract an env-reading seam (e.g. resolve_options_from(cli, &dyn Fn(&str) -> Option<String>)) and pass a fake map. If the env must be touched, serialize the tests behind a shared Mutex and set/restore inside the guard.


### I12. `scripts/mkvpropedit_set_name`:18 — correctness
**`find -iname '*.mkv'` matches files case-insensitively but `${base%.mkv}` strips the extension case-sensitively, so uppercase-extension files get the extension baked into the title tag.**

*Failure scenario:* A file `Movie.MKV` (uppercase extension — common from Windows rippers and older MakeMKV builds) is matched by `find "$dir" -iname '*.mkv'` at line 60. In `derive_title`, `base="Movie.MKV"` and `${base%.mkv}` does not match, so base stays `Movie.MKV`. The script then runs `mkvpropedit "$f" -e info --set "title=Movie.MKV"`, writing the filename *including the extension* into the MKV title tag. mkvpropedit exits 0, so it counts as `ok` and the summary reports success — the corruption is silent and is written into the media file. Verified: `base=Movie.MKV; ${base%.mkv}` -> `Movie.MKV`. With `-d`/`-f` set, the stray `.MKV` also lands in the cut field, e.g. `Show - S01 - Pilot.MKV -f 3` yields `Pilot.MKV`. Tests only ever use lowercase `.mkv`, so this passes today.

*Suggested fix:* Strip the extension case-insensitively, e.g. `shopt -s nocasematch` around the strip, or `base="${base%.[Mm][Kk][Vv]}"`, or use bash's `${base%.*}` (extension-agnostic, matching what `bisect_img`'s `out_prefix` already does). Add a `derive_title 'Movie.MKV'` case to tests/test_mkvpropedit_set_name.sh.


### I13. `scripts/bisect_img`:27 — correctness
**`out_prefix` yields a `._`-prefixed name for images in the default directory, so `bisect_img` with no arguments writes its output as hidden dotfiles.**

*Failure scenario:* `bisect_img` defaults to `dir="."` and `outdir="."` (line 41), the most common invocation. `find . -iname '*.jpg'` emits `./foo.jpg`. `out_prefix './foo.jpg'` computes `basename "$(dirname './foo.jpg')"` = `basename '.'` = `.`, producing the prefix `._foo`. The crop at line 72 therefore writes `./._foo-0.jpg`, and the renames produce `./._foo-1.jpg` and `./._foo-2.jpg` — hidden files, invisible to `ls`, to most file managers, and to a plain `*.jpg` glob. The script prints `bisect ./foo.jpg` and reports `done: N bisected, 0 failed`, so the user is told it worked but cannot find the halves. Verified directly. Tests only exercise `out_prefix '/a/sub/photo.jpg'` and `'b/x.jpeg'`, both of which have a real parent directory, so the default path is untested.

*Suggested fix:* Resolve the parent before taking its basename so `.` becomes a real name, e.g. `parent=$(cd "$(dirname "$1")" && basename "$PWD")`, or fall back to the invoking dir's name when `dirname` yields `.`. Add an `out_prefix './foo.jpg'` case to tests/test_bisect_img.sh.


### I14. `scripts/mkvpropedit_set_name`:39 — error-handling
**`-d` or `-f` as the final argument makes `shift 2` fail under `set -e`, aborting with exit 1 and no error message at all.**

*Failure scenario:* Running `mkvpropedit_set_name -d` (or `... -f`, or `mkvpropedit_set_name /media -d` where the delimiter was forgotten). `${2:-}` suppresses the `set -u` error and leaves `delim=""`, then `shift 2` runs with `$#` == 1 and returns non-zero. It is the last command of the case branch inside the `while` body — not a condition context — so `set -euo pipefail` (line 34) exits immediately with status 1, printing nothing on stdout or stderr. The user sees a silent failure and no usage text, and the `-d requires -f` validation at line 46 is never reached. Verified with a reduced repro of the exact parse loop. Sibling scripts get this right: `video_pcm_to_flac` uses `${2:?"$1 requires an argument"}` and `bisect_img` uses an explicit `[[ $# -ge 2 ]] || die`.

*Suggested fix:* Match the sibling scripts: `-d) [[ $# -ge 2 ]] || die "-d requires an argument"; delim="$2"; shift 2 ;;` and the same for `-f`. Also tighten line 46 to reject the mirror case — `-f` without `-d` is currently accepted and then silently ignored by `derive_title`'s `[[ -n "$delim" && -n "$field" ]]` guard, so `mkvpropedit_set_name -f 3 /media` retitles every file with the full basename instead of erroring.


### I15. `scripts/nudge`:1031 — correctness
**A value-taking CLI flag with its value omitted (-p/--pane, -m/--time, -i/--input) makes the argument-parsing loop spin forever at 100% CPU, because bash's `shift 2` with $#==1 is a silent failing no-op that never advances the loop.**

*Failure scenario:* `nudge -p` (a trivial typo — forgetting the pane) leaves $#==1, $1=="-p". The `-p|--pane` branch runs `TARGET_PANE="$2"` (empty) then `shift 2`, which fails silently and shifts nothing, so `while [[ $# -gt 0 ]]` re-enters the same branch forever. Verified against the extracted prelude: `-p`, `-m`, `-i`, `--pane`, `--time` and `--input` all hang (killed by `timeout 3`) and pin a core at 98% CPU. `-i` is worse: `MESSAGES+=("$2")` appends an empty element on every iteration — a measured 100,000 elements in 100k iterations — so `nudge -p bot:0.1 -i` grows the array without bound until the OOM killer intervenes. `-w` and `-r` escape only by accident: their value-validation regexes reject the empty string and `exit 1` before the loop can spin. This is the exact hazard the script already guards in its own payload walkers — `job_summary` (490-497), `job_detail` (509-518) and `load_job` (592-602) all use `shift 2 || return 1`, with comments spelling out the failing-no-op behaviour, and tests/test_jobs.sh:141-163 (F2) runs them under `timeout` to prove it — but the CLI parser those walkers mirror was never fixed.

*Suggested fix:* Guard each value-taking branch the way the payload walkers already are, e.g. `-p|--pane) [ $# -ge 2 ] || { echo "Error: $1 requires a value." >&2; exit 1; }; TARGET_PANE="$2"; SET_PANE=true; shift 2 ;;` for -p/-m/-i (and for -w/-r, so they fail on the missing value rather than on the regex). Worth mirroring test_jobs.sh's F2 `run_guarded`/`timeout` pattern over the CLI parser so the Rust port inherits the guard.


### I16. `tests/test_jobs_e2e.sh`:44 — test-harness
**The "environment can't queue an 'at' job" self-skip cannot distinguish an unusable `at` from a regressed `nudge`, so a scheduling regression silently disables the entire e2e file and the suite still reports PASSED.**

*Failure scenario:* The file decides whether to skip by running `nudge -p ... -m 23:59 ...` and grepping its stdout for `Job ID: N` (line 39/42). Any regression in `at_pipe`, `at_schedule_epoch`, `finalize_schedule`'s success message, the `-q $AT_QUEUE` handling, or the `-p`/`-m`/`-i`/`-n` parse branches makes that grep come back empty — whereupon line 44-47 prints `SKIP: environment can't queue an 'at' job (no id returned)` and `exit 0`. run.sh's `bash "$t" || rc=1` sees 0, prints `=== suite PASSED ===`, and all ~20 checks in the file (--list-plain, list_jobs, F5 non-TTY fallback, --preview-job, F1 queue-membership guard, F4 out-of-queue note, --edit, F3 false-"Done!" guard, --cancel guards) vanish without a single FAIL line. CI goes green while nudge cannot schedule anything at all. The risk is highest on macOS, which is precisely the platform this file exists to cover — its own header calls it "the only tests that exercise the actual `atq` output format ... what verifies the parsing on macOS/BSD".

*Suggested fix:* Decide skippability by probing `at` directly rather than through nudge, e.g. `if ! probe=$(echo true | at -q w now + 2 hours 2>&1 | grep -oE 'job [0-9]+'); then echo '  SKIP: at cannot queue here'; exit 0; fi` (then atrm the probe). Once at is known to work, an empty `$ID` from `schedule` is a real regression: report it via `check` and let the file fail.


### I17. `tests/test_jobs_e2e.sh`:32 — data-loss
**purge() deletes every `at` job in queues w, v and u — not just the ones the test created — so running the documented-as-safe `bash tests/run.sh` silently destroys the user's own scheduled jobs in those queues.**

*Failure scenario:* `TEST_QUEUES="w v u"` and `purge()` runs `atrm $(atq -q "$qq" | awk '{print $1}')` for each, with no filter on which jobs the test actually staged. purge is invoked at startup (line 36) and from the EXIT trap (line 35). `at` supports queues a-z as a plain user choice, so a user who ran `at -q w -t 202607160900 <<< 'backup.sh'` loses that job the next time anyone runs the suite — no prompt, no output (`2>/dev/null`), no record of what was removed. The header comment only reasons about not touching the nudge queue 'n' ("we never touch the user's real nudge queue"); it never establishes that w/v/u are unowned. This bit during this review: running `bash tests/run.sh` to check the suite purged queues w/v/u on the maintainer's machine.

*Suggested fix:* Track the ids the test stages (ID, F1ID, F4ID, NEW, plus any from F3) in an array and atrm only those. Failing that, make purge refuse rather than destroy: at startup, if `atq -q w|v|u` is non-empty, `echo '  SKIP: queues w/v/u are not empty'; exit 0`. Also worth correcting the header comment so the isolation claim matches what the code does.


### I18. `tests/test_video_pcm_to_flac.sh`:25 — test-harness
**The bash<4.3 skip branch runs `finish` then `exit 0`, discarding finish's status, so on macOS's system bash 3.2 a genuine failure in the four checks above it is reported as FAIL on stdout yet the file exits 0 and run.sh counts the suite as PASSED.**

*Failure scenario:* assert.sh's `finish` signals failure only through its return value (`[ "$FAIL" -eq 0 ]`, line 21). Lines 24-25 call `finish` and then `exit 0`, which throws that value away. On a macOS host whose `bash` is Apple's 3.2 (no homebrew bash on PATH — the exact host the skip exists for), the branch is taken. If `select_streams` or `output_path` regressed, checks 1-4 print `FAIL:` and the tally reads `== 3 passed, 1 failed ==`, but the file exits 0, so run.sh's `bash "$t" || rc=1` never trips and it prints `=== suite PASSED ===`. Reproduced verbatim against tests/assert.sh with the condition forced true: a failing check plus this branch yields file exit code 0. Every other test file ends in a bare `finish` and propagates correctly; this is the only place the contract is broken. CI hides it because `brew install bash` puts 5.x ahead of /bin on the runner PATH, so the branch is never taken there.

*Suggested fix:* Use a bare `exit` (which exits with the status of the last command, i.e. finish) or `finish; exit $?` — dropping the `exit 0`. Cheap hardening for the whole harness: make `finish` itself the terminator, e.g. have it `exit` rather than `return`, so no caller can drop the status.


### I19. `scripts/nudge`:1194 — correctness
**--verify tests for a limit banner anywhere in the captured pane with no notion of recency, so the very banner that motivated the nudge re-triggers the gate and nudge injects into a session the user already resumed — the exact outcome the flag advertises it prevents.**

*Failure scenario:* The headless path captures the pane (`tmux capture-pane -pJt`, which returns only the ~40 visible rows) and passes the whole thing to `has_limit_banner`, which greps for `(session limit|current session).*resets` positionally-blind. Scenario: at 23:00 Claude prints `⏸ session limit reached · resets 3:00am`; the user runs `nudge -p bot:0.1 -v`, scheduling for 03:03. At 03:01 the user resumes manually and Claude replies briefly. At 03:03 the pane still shows the 03:00 banner a few lines up, so has_limit_banner returns 0 and nudge injects "please continue" into the live session. Verified with the pure helpers: for a pane of [banner, "> continue", "● Sure — resuming now...", "● All 40 tests pass."], `has_limit_banner` returns 0 (--verify injects) while `pane_after_marker` on the same text correctly sees only the post-resume lines. The script already knows this hazard — pane_after_marker's comment (373-377) exists because "the stale banner that triggered this nudge (sitting above our input)" would otherwise be re-detected — but only the auto-retry re-scan (1220-1229) uses that scoping; --verify does not. tests/test_e2e_tmux.sh:34-40 claims to cover this ("guards against nudging a session you already resumed") but calls `fresh_pane`, which kills the session and starts a new one, so its pane never had a banner at all; the test can only ever catch the trivial case.

*Suggested fix:* Give --verify the same recency scoping the retry path has: at schedule time record the banner's marker/line, or at fire time restrict has_limit_banner to the tail of the pane (below the last user input line) rather than the whole capture. Then extend test_e2e_tmux.sh with the realistic case — send the banner, then post-resume output, then assert --verify SKIPs — instead of resetting the pane.


### I20. `Makefile`:17 — correctness
**`make` hardcodes the cargo output path and creates a dangling `bin/nudge` symlink — reporting success — whenever CARGO_TARGET_DIR is set.**

*Failure scenario:* A user with `export CARGO_TARGET_DIR=~/.cargo-target` in their shell rc (a common setup to share a build cache across projects) runs `make`. Verified via `cargo metadata`: with CARGO_TARGET_DIR set, target_directory becomes that path, so `cargo build --release --manifest-path nudge-rs/Cargo.toml` (Makefile:41) writes the binary to `~/.cargo-target/release/nudge`, NOT `nudge-rs/target/release/nudge`. Makefile:31 then runs `ln -sfn "../nudge-rs/target/release/nudge" "bin/nudge"`. `ln -s` never validates that its target exists, so it silently creates a dangling symlink, prints `  link  bin/nudge -> ../nudge-rs/target/release/nudge`, and make exits 0. The user follows the README, puts ./bin on PATH, runs `nudge`, and gets `nudge: No such file or directory`. The target lies: it claims to have linked a binary it did not link. Corroborating evidence that this variable is a known hazard in this repo: both packaging/aur/nudge/PKGBUILD:25 and packaging/aur/nudge-git/PKGBUILD:24 explicitly `export CARGO_TARGET_DIR=target` to defend against exactly this leak — the Makefile does not.

*Suggested fix:* Derive the path instead of assuming it, and verify the link resolves. Either pin it (`build-nudge: export CARGO_TARGET_DIR := $(CURDIR)/nudge-rs/target`) matching what the PKGBUILDs already do, or query cargo: `NUDGE := $(shell cargo metadata --manifest-path nudge-rs/Cargo.toml --format-version 1 --no-deps | jq -r .target_directory)/release/nudge`. Additionally guard the link with `test -x "$(NUDGE)" || { echo "nudge binary not found at $(NUDGE)" >&2; exit 1; }` so a mislocated binary fails loudly rather than producing a broken symlink.


### I21. `.github/workflows/tests.yml`:37 — ci-coverage
**`scripts/cue2flac` has zero CI coverage: the syntax check covers only `scripts/nudge`, and no test file exercises cue2flac.**

*Failure scenario:* The Syntax check step runs `bash -n scripts/nudge` and `bash -n` over `tests/*.sh` — it never syntax-checks the other six utilities in scripts/. Five of those six are covered incidentally because their test files `source` them (e.g. tests/test_bisect_img.sh:6 sources scripts/bisect_img, so a syntax error there fails the suite). `cue2flac` is the sole exception: grepping tests/ for "cue2flac" returns nothing — no test file sources or invokes it. So scripts/cue2flac is validated by nothing in CI. Concretely: a contributor edits scripts/cue2flac and introduces a syntax error (unbalanced quote, missing `fi`) or breaks the `msf_to_sectors()` MSF-to-sector arithmetic at line 69. On push, tests.yml passes green (it never touches cue2flac) and nudge-rs.yml is skipped entirely by its `paths: ['nudge-rs/**']` filter. The PR merges with a completely broken utility. cue2flac is 173 lines with a pure-logic `msf_to_sectors()` function — the same testable shape as bisect_img's `is_landscape` and mkvpropedit_set_name's `derive_title`, both of which do have tests.

*Suggested fix:* Widen the syntax step to cover every script: `for f in scripts/*; do bash -n "$f"; done` (all seven pass `bash -n` today, so this goes green immediately). Then add tests/test_cue2flac.sh sourcing scripts/cue2flac and asserting `msf_to_sectors` against known MSF values, mirroring the existing test_bisect_img.sh pattern.


## MINOR (16)

### M1. `nudge-rs/src/daemon.rs`:62 — logging
**The "dropped stale job" info log fires unconditionally, including on the path where `q.remove` just failed.**

*Failure scenario:* Lines 58-63: `if let Err(e) = q.remove(*id) { tracing::warn!("removing stale job {id} failed: {e}") }` is immediately followed by an unconditional `tracing::info!("nudge: dropped stale job {id}")`. When `remove`'s `save()` fails (ENOSPC/read-only state dir), the log emits a warning that removal failed and then, on the very next line, claims the job was dropped. Anyone debugging why stale jobs keep reappearing after a restart (they do — see the queue.rs rollback finding) reads the log as confirmation the drop succeeded and looks in the wrong place.

*Suggested fix:* Move the `info!` into an `else` branch, or log it only when `remove` returns `Ok(true)` — which also correctly stays quiet when the id was already gone.


### M2. `nudge-rs/src/cli.rs`:42 — correctness
**`-r -1` / `--retries -1` — the infinite-retry value the help text documents — is rejected by clap, because the arg lacks allow_negative_numbers.**

*Failure scenario:* cli.rs:41 documents `Exact retry count (-1 = forever). Implies --auto-retry`, and -1 is a real supported value everywhere downstream (scheduler.rs:57 `-1 stays -1 (infinite)`; tests/cli_jobs.rs:89 asserts "infinite retries preserved"). But `retries: Option<i64>` is declared with no `allow_negative_numbers = true`, so clap parses `-1` as an unknown short flag.

Verified against the built binary (using --completions to short-circuit before any daemon/tmux work):
  ./target/debug/nudge --retries -1 --completions bash  -> exit 2, "error: unexpected argument '-1' found"
  ./target/debug/nudge -r -1 --completions bash          -> exit 2
  ./target/debug/nudge --retries=-1 --completions bash    -> exit 0

So the documented feature is reachable only via the undiscoverable `--retries=-1` equals form (or NUDGE_RETRIES=-1). A user following the help text hits a parse error with no hint that the equals form would work.

*Suggested fix:* Add `allow_negative_numbers = true` to the arg: `#[arg(short = 'r', long = "retries", allow_negative_numbers = true)]`, and add a CLI test parsing `["nudge", "-r", "-1"]`.


### M3. `nudge-rs/src/paths.rs`:26 — correctness
**resolve() treats a set-but-empty XDG_STATE_HOME / XDG_RUNTIME_DIR as a valid path, producing CWD-relative state and socket paths instead of falling back to $HOME as the XDG spec requires.**

*Failure scenario:* paths::resolve uses `std::env::var_os("XDG_STATE_HOME").map(PathBuf::from)`, which returns `Some("")` when the variable is set but empty. resolve_from then does `PathBuf::from("").join("nudge")`, and the `unwrap_or_else` $HOME fallback never runs. The XDG basedir spec states an empty value must be treated as unset.

Verified against the built lib:
  resolve_from("/home/d", Some(""), Some(""), Os::Linux)
    queue  = "nudge/queue.json"  absolute? false
    socket = "nudge.sock"        absolute? false

With `XDG_RUNTIME_DIR=` exported (empty vars are common in cron/systemd units and some login shells), both the CLI and daemon resolve their socket relative to their own CWD. app::ensure_daemon pings `./nudge.sock`, gets nothing, and spawns a daemon that binds `./nudge.sock` in whatever directory the caller happened to be in. Scheduling from ~/projects/a and then listing from ~/projects/b talks to a different socket and a different queue.json, so `nudge --list` reports "no pending nudge jobs" for jobs that exist, and a stray nudge.sock/nudge/ directory is littered into each working directory.

*Suggested fix:* Filter empty values before use, e.g. `std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())` (same for XDG_RUNTIME_DIR and XDG_CONFIG_HOME), and add a resolve_from test covering `Some(Path::new(""))`.


### M4. `nudge-rs/src/timespec.rs`:77 — correctness
**parse_clock silently accepts malformed time specs — an out-of-range meridiem hour like "13pm" is taken as 13:00, and the unanchored meridiem search lets arbitrary text containing "am"/"pm" parse as a time.**

*Failure scenario:* Two grounded issues, both verified against the built lib with now = 2026-07-13 10:00Z:

(1) The meridiem arms (lines 78-80) never validate that hour is in 1..=12. `Some("PM") if hour < 12` fails for hour=13, `Some(_) => {}` swallows it, and the 0..=23 range check at line 88 then passes it through:
    parse_timespec("13pm") => 2026-07-13 13:00
  A user who typos `nudge -m 13pm` meaning 1pm gets a job silently scheduled for 13:00 with no error.

(2) The meridiem regex at line 67 is unanchored over the whole uppercased string, and matching a meridiem skips the "must look like a 24h clock" guard at lines 81-86 that otherwise rejects a bare number:
    parse_timespec("spam 5") => 2026-07-14 05:00   ("SPAM" contains "AM"; time_re grabs the first digit run anywhere)
    parse_timespec("3: 00")  => 2026-07-14 03:00   (guard passes because up.contains(':') is true even though group 2 didn't capture)
  Garbage that should hit TimespecError::Unrecognized instead schedules a real job at a time the user never asked for.

For contrast, the range check does work where it is applied: "0:99" and "99:00" both correctly return Unrecognized.

*Suggested fix:* Anchor parse_clock's regex over the whole trimmed input (e.g. `^\s*(\d{1,2})(?::(\d{2}))?\s*(AM|PM)?\s*$`) so trailing/leading text is rejected, and reject hour outside 1..=12 when a meridiem is present before the 0..=23 normalisation.


### M5. `nudge-rs/src/register/mod.rs`:103 — misleading-ux
**`--uninstall-daemon` reports "removed <path>" and exits 0 even when nothing was installed and no file was removed.**

*Failure scenario:* Both arms of uninstall discard the result of the removal and then print unconditionally: `let _ = std::fs::remove_file(&unit);` followed by `println!("nudge: removed {}", unit.display())` (mod.rs:103-104), and the launchd equivalent at 112-113. The `systemctl`/`launchctl` status is discarded too (99-101, 108-110).

So a user who never ran `--install-daemon` (the common case — the daemon auto-starts via ensure_daemon) runs `nudge --uninstall-daemon` and sees `nudge: removed /home/d/.config/systemd/user/nudged.service`, exit 0, for a file that never existed. Worse, it implies the daemon is gone while an ad-hoc daemon spawned by ensure_daemon is still running and will still fire pending jobs — `disable --now` only stops a systemd-managed unit, and there is no `--stop-daemon`.

*Suggested fix:* Branch on the `remove_file` result: print "removed <path>" only on Ok, and something like "no registration found at <path>" on NotFound. Consider also pinging the socket and reporting/stopping a still-running ad-hoc daemon so the command's claim matches reality.


### M6. `nudge-rs/src/app.rs`:132 — misleading-ux
**`list()` ignores its `_plain` argument, so `--list` and `--list-plain` are identical despite the help text advertising `--list` as interactive.**

*Failure scenario:* cli.rs:52-57 documents `--list` as "Review pending jobs (interactive)." and `--list-plain` as "Review pending jobs as a plain table (non-interactive)." — two flags promising two behaviours. But `pub fn list(_plain: bool)` (app.rs:132) discards the parameter and unconditionally prints `format_jobs`, and dispatch collapses both to the same call (`if cli.list || cli.list_plain { return list(cli.list_plain) }`, app.rs:244-246). A user running `nudge --list` expecting the advertised interactive picker gets a static table with no error and no hint that the feature is unimplemented; `--list-plain` is a flag with literally no observable effect.

The code comment concedes this ("interactive picker lands in Task 6"), so it's a known gap — but the shipped `--help`/completions (generated from these same doc comments, app.rs:274) advertise it to users today.

*Suggested fix:* Either implement the interactive picker behind `!plain` (inquire::Select is already a dependency and used by pick_pane), or, until then, drop the "(interactive)" claim from the `--list` doc comment and hide/remove `--list-plain` so the help text matches actual behaviour.


### M7. `nudge-rs/tests/cli_jobs.rs`:33 — tautological-assertion
**assert!(out.contains('1')) is commented "// the id" but is satisfied by the pane string and the timestamp, so it passes even with the ID column entirely removed; the test's claimed count coverage is absent too.**

*Failure scenario:* format_jobs_shows_id_pane_and_count renders a job whose pane is "bot:0.1" and whose fire_at is 2026-07-13T15:00:00Z. Both contain the character '1', so `out.contains('1')` is true no matter what the id column does. Verified against the real row format (app.rs:115): the test's two assertions both still pass when the id column is deleted from the format string entirely, when the id renders as 999 instead of 1, and when the MSGS count renders as 42 instead of 1. This is the only test for format_jobs, which is the whole `nudge --list-plain` / `--list` renderer, so two of the three columns the test name advertises (id, count) are unverified. `assert!(!out.is_empty())`-style redundancy aside, only the pane column is actually covered.

*Suggested fix:* Assert on the rendered row rather than on characters: e.g. give the job a distinctive id/count and assert `out.contains("999")` and that the MSGS column shows the message count, or assert the exact row line. Pick fixture values that cannot appear incidentally in the pane or timestamp.


### M8. `nudge-rs/src/register/launchd.rs`:65 — weak-assertion
**plist_has_label_program_and_flags asserts the RunAtLoad/KeepAlive keys are present but not that they are true, so a regression disabling both still passes on macOS CI while the daemon would never auto-start.**

*Failure scenario:* The test's comment claims "RunAtLoad / KeepAlive true keys present", but the assertions are only `xml.contains("RunAtLoad")` and `xml.contains("KeepAlive")` -- the key *names*. plist XML emits a false value as `<key>RunAtLoad</key><false/>`, leaving both literal strings intact. Verified: serializing the same LaunchAgent struct with run_at_load: false, keep_alive: false yields XML for which both assertions still hold. If someone flipped either flag in plist_bytes (line 26), macOS users' daemons would silently never start at load and never restart after a crash, and the macos-latest CI job stays green. The systemd side does not have this gap -- its test asserts the full `ExecStart="/usr/bin/nudge" --daemon` and `Restart=on-failure` strings.

*Suggested fix:* Assert the value, not just the key -- e.g. `xml.contains("<key>RunAtLoad</key>\n\t<true/>")`, or better, deserialize the plist back with plist::from_bytes into a struct and assert run_at_load == true && keep_alive == true, which is robust to formatting.


### M9. `scripts/batch_img2pdf`:59 — portability
**`"${pids[@]}"` on an empty array under `set -u` aborts on bash < 4.4, so running against a directory of already-extracted folders (no zips) fails before any PDF is built.**

*Failure scenario:* `batch_img2pdf DIR` where DIR holds image folders but no `*.zip` (a normal workflow — archives already unpacked). The glob at line 53 does not expand, `[[ -e "$z" ]]` fails, `break` fires, and `pids` stays empty. Line 59 then expands `"${pids[@]}"` under the `set -u` from line 39. On bash 4.3 and earlier — including the bash 3.2 that macOS ships, which this repo explicitly targets (Homebrew packaging in packaging/, and video_pcm_to_flac line 85 calls out "macOS ships 3.2") — an empty array expansion is treated as unset and errors `pids[@]: unbound variable`, exiting before the PDF loop. Fixed in bash 4.4, so it cannot reproduce on this box's bash 5. The author already knows the idiom: `select_streams` guards with `"${args[*]:-}"`, and the other arrays here (`images`, `error_files`, `audio_streams`) are all length-guarded before expansion — `pids` is the one that was missed.

*Suggested fix:* Guard the expansion the way the rest of the repo does: `[[ ${#pids[@]} -gt 0 ]] || return`-style check before the loop, or expand as `"${pids[@]+"${pids[@]}"}"`.


### M10. `scripts/video_pcm_to_flac`:186 — portability
**The script gates itself to "bash 4.3+" but `"${metadata[@]}"` on an empty array under `set -u` is only safe on 4.4+, so on exactly bash 4.3 any file whose streams lack title tags aborts the batch.**

*Failure scenario:* On bash 4.3 the guard at line 84 passes (it only rejects < 4.3). Convert an MKV whose PCM streams carry no `title` tag — untitled audio tracks are entirely ordinary. `stream_metadata` correctly leaves `metadata` empty (line 153), and lines 176 and 186 expand `"${metadata[@]}"` under the `set -u` from line 82. Bash 4.3 treats the empty array as unset and errors `metadata[@]: unbound variable`, killing the whole run mid-batch rather than skipping one file. The stated floor is wrong: the real floor is 4.4. The suite's own test at line 40 covers an untitled stream returning rc 0, but never the empty-array expansion, and CI presumably runs bash 5 where it cannot reproduce.

*Suggested fix:* Either raise the version guard to 4.4 (and update the `Requires:` header on line 19 and the comment on tests/test_video_pcm_to_flac.sh line 18), or make the expansion 4.3-safe with `"${metadata[@]+"${metadata[@]}"}"` at both call sites and keep the 4.3 floor.


### M11. `scripts/batch_img2pdf`:23 — argument-parsing
**`-h` sets a `MAINDIR="__help__"` sentinel that a later positional silently overwrites, so `--clean -h DIR` performs the destructive run instead of printing help.**

*Failure scenario:* `batch_img2pdf --clean -h book` — a user reaching for help on the destructive flag. Line 23 sets `MAINDIR="__help__"` but does not exit; the loop continues, and `*) MAINDIR="$1"` at line 28 overwrites it with `book`. The help check at line 41 then fails, and the script runs the full pipeline against `book` with `CLEAN=1`, `rm -rf`-ing folders. The behavior is also order-dependent and inconsistent: `batch_img2pdf book -h` *does* print usage and exit 0. Every sibling script (`bisect_img` line 44, `batch_makemkvcon` line 30, `video_pcm_to_flac` line 93) instead does `-h|--help) usage; exit 0 ;;` inline. As a bonus, a directory legitimately named `__help__` prints usage instead of being processed.

*Suggested fix:* Drop the sentinel and exit immediately like the sibling scripts: `-h|--help) usage; exit 0 ;;`. If `parse_args` must stay side-effect-free for the tests, have it set a separate `HELP=1` variable rather than overloading `MAINDIR`.


### M12. `scripts/cue2flac`:162 — error-handling
**The `dd | ffmpeg` pipeline is unguarded under top-level `set -euo pipefail`, so one bad track kills the whole extraction with no summary — the opposite of the batch resilience every rewritten sibling script guarantees.**

*Failure scenario:* A 12-track CUE where track 5 has a bad boundary or a codec hiccup makes ffmpeg exit non-zero. The pipeline at line 162 is a bare statement, not an `if` condition, so it is not errexit-exempt; `set -e` (line 9) exits the script instantly. Tracks 6-12 are never extracted, the `Done: N extracted, M skipped` summary at line 173 never prints, and the partial `05 - Title.flac` is left behind (no cleanup — contrast video_pcm_to_flac lines 180/190, which `rm -f "$out"` on failure). `pipefail` widens this: a SIGPIPE or error from `dd` alone is equally fatal. This is the one script not rewritten, and the divergence is stark — tests/test_mkvpropedit_set_name.sh and tests/test_batch_img2pdf.sh both assert in their comments that "one failing X must not abort the batch" and that the script still exits 0 with an accurate N/M summary.

*Suggested fix:* Wrap it in the house pattern: `if dd ... | ffmpeg ...; then extracted=$((extracted+1)); else printf 'WARN: ffmpeg failed for track %02d\n' "$track_num" >&2; rm -f "$outfile"; failed=$((failed+1)); fi`, and report `failed` in the closing summary.


### M13. `scripts/cue2flac`:110 — correctness
**The `-t` tracklist length is validated against the total CUE track count including data tracks, so the documented "paste from VGMdb" workflow always dies on the mixed-mode discs the tool is built for.**

*Failure scenario:* A game-soundtrack CD with a MODE1 data track 1 followed by 12 audio tracks — precisely the disc whose tracklist you would paste from VGMdb. `tracks` is populated from every `INDEX 01` line (line 92-93) with no filter on type, so `num_tracks` is 13. The VGMdb paste lists only the 12 audio tracks, so `${#track_names[@]}` is 12 and line 110 dies with `Tracklist has 12 names but CUE has 13 tracks.` The tool's own `--help` advertises both "Data tracks (MODE1/MODE2) are skipped" and "-t <tracklist> ... paste from VGMdb", so the two documented features are mutually unusable here. Working around it requires knowing to pad a dummy name for the data track, because line 144 indexes `track_names[$i]` by CUE index rather than by audio-track ordinal — undocumented and unguessable. Verified against a synthetic mixed-mode CUE.

*Suggested fix:* Index names by audio track instead of CUE track: count audio tracks into `num_audio` while parsing, validate `${#track_names[@]}` against `num_audio`, and keep a separate `audio_idx` counter to look up `track_names[$audio_idx]`. Doing so also fixes the `-metadata track="$track_num/$num_tracks"` at line 141, which currently writes `2/13` onto what is really audio track 1 of 12.


### M14. `.gitignore`:4 — gitignore-gap
**makepkg build artifacts under packaging/aur/ are not gitignored, so testing a PKGBUILD leaves a full source tree and a nested git clone stageable by `git add -A`.**

*Failure scenario:* Verified with `git check-ignore -v` — none of these paths are ignored (exit 1). .gitignore only lists `tags` and `bin/`. The only way to validate a PKGBUILD is to run `makepkg` in its directory. Doing so in packaging/aur/nudge/ creates `src/` (the extracted daveyutils-nudge-v0.1.0 tree plus a vendored cargo registry, hundreds of MB), `pkg/`, `nudge-0.1.0.tar.gz`, and `nudge-0.1.0-1-x86_64.pkg.tar.zst`. Running it in packaging/aur/nudge-git/ clones the whole repo into `packaging/aur/nudge-git/daveyutils/` — a nested git repo. All of it shows as untracked in `git status`, so a routine `git add -A && git commit` (or `git commit -a` after `git add`) commits megabytes of build output, and the nested clone gets committed as a stray gitlink/embedded repo that breaks clones for everyone else.

*Suggested fix:* Add to .gitignore: `packaging/aur/*/src/`, `packaging/aur/*/pkg/`, `packaging/aur/*/*.tar.gz`, `packaging/aur/*/*.pkg.tar.*`, and `packaging/aur/nudge-git/daveyutils/`.


### M15. `.github/workflows/tests.yml`:41 — ci-coverage
**No workflow ever invokes the Makefile, so this branch's entire deliverable can be broken while CI stays green.**

*Failure scenario:* tests.yml runs `bash tests/run.sh` directly, duplicating what `make check` does rather than exercising it; nothing in either workflow runs `make`, `make check`, or `make link`. nudge-rs.yml's `paths: ['nudge-rs/**', '.github/workflows/nudge-rs.yml']` filter excludes Makefile, so it won't fire on Makefile edits either. Result: the Makefile — the sole feature of the feat/bin-install branch — has no CI at all. Concretely, a typo in `NUDGE := nudge-rs/target/release/nudge` (Makefile:17), or the CARGO_TARGET_DIR dangling-symlink defect reported separately, produces a `make` run that exits 0 while installing a non-functional `bin/nudge`, and every workflow reports green. This is the gap that lets that bug ship undetected.

*Suggested fix:* Add a job (or a step to the existing ubuntu leg) that actually drives the Makefile and asserts the result resolves: `make && test -x bin/nudge && test -L bin/bisect_img && ./bin/nudge --version`. The `test -x` on a symlink follows the link, so it catches a dangling bin/nudge — precisely the failure mode the current CI cannot see.


### M16. `README.md`:34 — docs-accuracy
**The README's Layout section omits the tracked top-level `docs/` directory.**

*Failure scenario:* The Layout section enumerates scripts/, nudge-rs/, tests/, and packaging/, presenting itself as the repo's map. But `git ls-files` shows 9 tracked files under docs/superpowers/ (7 plans in docs/superpowers/plans/ and 2 specs in docs/superpowers/specs/) — 9 of the repo's 72 tracked files, and an entire top-level directory the map doesn't mention. A contributor reading the README to orient themselves has no idea design specs and implementation plans exist, and will not find or update them (e.g. docs/superpowers/specs/2026-07-13-nudge-rust-rewrite-design.md, the design doc for the nudge rewrite the README does discuss).

*Suggested fix:* Add a bullet to the Layout list, e.g. `- \`docs/\` — design specs and implementation plans.` If these are considered transient agent scratch rather than repo documentation, the alternative is to untrack them and ignore the directory — but leaving a tracked top-level dir undocumented is the worst of both.


## Refuted (not real — recorded so they aren't re-raised)

- `nudge-rs/src/daemon.rs` — `retry_at` and `next_wake` are computed from the `now` captured before injection, so the MIN_RETRY_SECS/settle_secs guard collapses to zero whenever an injectio
  - **Why refuted:** Traced daemon.rs precisely: `now` (line 47) is never reassigned within a loop pass, so `retry_at` (line 83) and the `next_wake` call (line 96) both use the identical stale timestamp. This makes the staleness cancel algebraically: `next_wake`'s sleep computation always yields ≈`retry_secs` regardless of how long the actual injection (`run_injection`) took, because it's comparing `now+R` against the

- `nudge-rs/src/scheduler.rs` — apply_outcome's Err branch -- the only destructive one -- is never tested; all four scheduler tests pass Ok(...), so nothing pins the fact that a failed injecti
  - **Why refuted:** The code mechanics cited in the claim are all accurate: apply_outcome's only reschedule arm requires Ok(Sent(_)) + auto_retry + retries_left != 0; every other outcome (including Err) falls through to queue.remove(job.id); daemon.rs passes run_injection's raw Result straight through; tmux.rs bails with Err on non-zero exit; and indeed none of the four existing scheduler tests exercise the Err arm.


- `nudge-rs/tests/tmux_e2e.rs` — All four tmux integration tests early-return and report "ok" when tmux is absent, so the entire e2e layer reports green while testing nothing.
  - **Why refuted:** The core technical fact underlying the claim is true in isolation: a Rust test function that does a bare `return` before any assertion runs is reported by libtest as "ok" (passed), not "ignored" — that part isn't disputable. But the claim fails on two counts specific to this repo:

1. It's documented, intended behavior. The module doc comment at the top of tmux_e2e.rs states explicitly: "All self-


## Follow-ups raised by the Increment 1 final review (2026-07-15)

The final review of the criticals remediation found 4 Important issues (all fixed in
`dd8a85c`, before merge) and these minors, deferred to Increment 4. Two of the
Importants were regressions introduced *by* the remediation itself, which is why
they are recorded here rather than dropped.

### F1. `nudge-rs/tests/daemon_singleton.rs`:86 — test-quality
**`serve_refuses_to_steal_a_live_socket` hangs instead of failing on regression.**

*Failure scenario:* Mutation-tested — with `serve` reverted to an unconditional unlink, the test does not FAIL, it hangs ("running for over 60 seconds"). Old `serve` steals the socket and enters its infinite accept loop, so `expect_err` never returns. The guard does catch the regression, but as a CI timeout with no diagnostic.

*Suggested fix:* Run `serve` on a thread with `recv_timeout`, as `run_refuses_to_start_while_another_daemon_holds_the_lock` already does correctly.

### F2. `scripts/batch_img2pdf`:20 — correctness
**Symlinks are invisible to both sides of the `--clean` coverage check.**

*Failure scenario:* `find -type f` matches neither symlinks-to-files nor symlinks-to-dirs, and the `-mindepth 1 -type d` probe doesn't see a symlinked dir. Verified: `book/{cover.jpg, link.jpg -> elsewhere}` gives `total=1, n=1` → `folder_fully_covered` returns true → `rm -rf book/`. Blast radius is bounded (`rm -rf` does not follow symlinks, so the target survives), but `unar` can emit symlinks from zips.

*Suggested fix:* Count with `find "$dir" ! -type d | wc -l` so links are included in `total`, making the folder correctly *kept*.

### F3. `scripts/batch_img2pdf`:20 — usability
**A single dotfile permanently disables `--clean` for a folder.**

*Failure scenario:* `book/{cover.jpg, .DS_Store}` → `total=2, n=1` → kept forever. `.DS_Store` is ubiquitous in macOS-authored zips, so on that platform `--clean` may effectively never clean. Conservative-by-spec and not data loss, but the WARN won't tell the user a dotfile is the reason.

*Suggested fix:* Name the offending files in the WARN, or ignore a small known-junk set.

### F4. `tests/test_batch_img2pdf.sh`:78 — test-coverage
**The C3 test discards stderr (`2>&1`), so the WARN is unasserted.** C3's core complaint was that the run looks like a clean success; the WARN is the fix's only user-facing signal that a folder was kept, and nothing pins it.

### F5. `tests/test_jobs_e2e.sh`:31 — test-quality
**Scoped purge leaks `at` jobs precisely when M-series id-parsing breaks.** If `schedule` queues a job but a format regression breaks the `Job ID: N` grep, `ID` is empty → nothing is remembered → the file SKIPs *and* leaks a real `at` job into queue `w` every run, accumulating. The old blanket purge covered this.

*Suggested fix:* Diff `atq -q w` before/after rather than parsing nudge's stdout.

### F6. `tests/test_jobs_e2e.sh`:31 — robustness
**`remember_id` returns 1 on empty input.** `[ -n "$1" ] && ...` is the last command, so the function returns non-zero on the *expected* empty-id path. Harmless today (no `set -e` in this file), but a landmine for anyone who later adds errexit. Append `; return 0`.

### F7. `nudge-rs/src/queue.rs`:87 — resource-leak
**Pid-named temps now litter unboundedly.** A save that fails mid-write used to leave exactly one reusable `queue.json.tmp`; it now leaves a distinct `queue.json.<pid>.tmp` per crashed process, which nothing reaps.

*Suggested fix:* `tempfile::NamedTempFile` in the same dir (already a dev-dep; would need promoting), or a best-effort sweep on load.

### F8. `nudge-rs/tests/daemon_singleton.rs`:163 — test-quality
**`process::exit(1)` inside a test binary.** `a_running_daemon_holds_the_lock_for_its_whole_life` runs a real `daemon::run` in-process, and its IPC thread carries the fatal-serve `std::process::exit(1)`. If that `serve` ever hits a fatal `accept()` error, the whole cargo-test binary exits 1 with no test-level attribution. Low probability, nasty to debug.

### F9. `nudge-rs/src/daemon.rs`:81 — test-coverage
**No test covers the fatal `serve` exit.** `process::exit` is untestable in-process, but the policy could be extracted (e.g. `fn on_serve_exit() -> !`) or pinned by spawning the built binary. Currently inspection-only.
