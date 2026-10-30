# subcheck

Proofread the text subtitles embedded in MKV files with an LLM, then review the fixes.

- `subcheck extract` writes a subtitle track out the way mkvextract does, reading only the
  subtitle packets (through the file's Cues) instead of the whole file.
- `subcheck scan` sends the English lines to an LLM and records each suggested fix in its database
  ([Where findings are kept](#where-findings-are-kept)), with each track `timing` finds shifted,
  and what each LLM call used ([LLM costs](#llm-costs)). A file unchanged since any earlier scan
  checked it the same way is skipped; `--force` checks it again. `--run <run>` adds the files to
  that run, making only the checks they need. On a terminal it opens review's screen as it
  starts, and each file's findings join the list when the file is done ([Review](#review));
  `--no-tui` prints its log instead. `--karaoke` makes the screen's apply re-space per-letter
  karaoke as `apply --karaoke` does.
- `subcheck review` steps through a run's fixes, or those of the files under some paths, to
  accept, reject or reword each one. With no argument it opens the folders that hold the newest
  run's files.
- `subcheck list` prints a run's fixes (or those of the files under some paths) as text, under
  what their LLM calls cost; `--mpv` adds a command that opens each one.
- `subcheck apply` makes a run's accepted fixes in the MKV files, keeping each original as
  `<name>.mkv.orig`: a file's typos and timing fixes in one remux, each shifted line moved by whole
  frames on the video's frame grid. Needs mkvtoolnix.
- `subcheck timing` reports subtitle tracks shifted by whole frames against the video, with the
  `mkvmerge --sync` that fixes a whole-track shift. A track with too few lines on cuts is timed by
  the lines it shares with another track. No LLM, nothing recorded.
- `subcheck audio` reports whether audio tracks holding the same sound in different formats (a
  TrueHD and its AC-3, a FLAC and an AAC of it) line up, with the `mkvmerge --sync` that fixes a
  steady lag. Needs ffmpeg; nothing recorded.

## Review

`subcheck scan` opens this screen on the scan it starts, when stdout and stderr are a terminal
(`--no-tui` keeps the log). Each file's findings join the list when the file is done, the
selection staying on its row as rows come in above it. A typo that joins a row whose findings are
all decided alike takes that decision, your wording too, and the status line says so; `u` takes it
back. A timing finding that joins a row stays pending: each episode's fix can differ.

`Tab` shows the files instead: each with where the scan is with it (`queued`, `reading`, `LLM 2/3`,
`✓ 412 lines · 3 findings`, `✓ timing only · 1 finding`, `✗ FAILED: …`), and in the detail pane
what the scan said of it. `⏎` on a file goes to its first finding, and `u` undoes there as it does
in the list. `c` checks the file again, as `subcheck scan --run <run> --force <file>` does: into
the run you are reviewing, or in a view of files and folders, the newest run that named it, with
that run's LLM and languages. It asks first, since it spends money, and while a scan runs it waits
for that to end. While the scan runs, the list's title and the status line say how far it is; once
it ends, the status line gives its summary. What the scan warns of as it starts (a folder it cannot
read) stays on the terminal above the screen, and the first of it is on the status line as the
screen opens. Files the scan skips as unchanged are in the list from the start.

When a scan checks a file again, its earlier findings stay in the list until the new ones are
saved, and you can decide them meanwhile: a decision carries over to the finding that comes back
with the same line and fix. While it is checked, the file's name spins in the list, and the detail
pane says where the check is (`checking again: LLM 1/3`). If the check fails, the findings stay. A
decision on a finding that another subcheck checked again since the screen opened goes to the
finding that came back in its place, and the status line says so; when none came back, nothing is
written, and the status line says that too.

`q` while the scan runs asks first, then stops it: the run keeps every file that was done, and the
terminal gets the command that finishes it (`subcheck scan --run <run> <paths>`; after a `c`,
`subcheck scan --run <run> --force <files>`).
Files `c` queued for after the scan are left unchecked: the question counts them, and the terminal
gets the command that checks them (`Not checked again: subcheck scan --run <run> --force <files>`).

A bare `subcheck review` opens on the folders that hold the newest run's files: every file
recorded there, whichever run checked it. The list's top border names what is shown
(`run 2026-10-29T21-30-49`, or `Spy x Family (2023)`), and `v` switches: from a run to its folders,
or from folders to the newest run that named the selected file. The selection, the filter and what
you can undo come along where the other view shows them.

`A` applies the accepted fixes of the view on screen, whatever the filter, as `subcheck apply`
would with the same view: it asks first (`apply 37 fixes in 9 files (30 applied already), keeping
each .orig? y/n`), then remuxes, checks and swaps each file in turn while you go on reviewing. `D`
is a dry run: it asks nothing and writes nothing. Both wait for a scan to end. While they run, the
list's top border and the status line say how far they are, the files view shows each file's
stage (`queued`, `remux`, `✓ swapped · 3 applied · 1 by hand`), and its detail pane what apply
wrote for it; `v` and `c` are refused until apply ends. Each row then says what became of its fix:
`applied` (from the database, so it shows in every later review), `by hand` or `not found` for this
session, or after a dry run `would apply`, `already applied`, `would be by hand`, `would not be
found`; a row whose episodes differ counts them (`applied 5 · by hand 1`), and the detail pane gives
a by-hand fix's reason. `--karaoke` on `review` or `scan` re-spaces per-letter karaoke as `apply
--karaoke` does. `q` while apply runs asks first; the file it was on is left as it was, and the
terminal gets the command that makes the rest (`subcheck apply <view>`).

A fix repeated across episodes (an OP lyric, say) is one row; `⏎` expands it into its episodes. A
season's timing findings on one track are one row too, whatever each episode's fix (`−1 frame ×23 ·
partial ×1`): `o` opens one paused on the frame that shows the error, `[` `]` step from there, and
`e` does not apply (a timing fix is accepted or rejected).

Decided rows stay in the list: green when accepted, red when rejected, yellow when a group's
episodes are all decided, differently. A row with anything still pending keeps the terminal's
colour (its mark `±` when some episodes are decided). Review opens on the first row with anything
pending, and after `a`, `r`, `e` or `x` moves to the next one below (under the accepted or rejected
filter, to the next row).

| Key | Does |
|---|---|
| `j` `k` ↑ ↓ `g` `G` PgUp PgDn | move |
| `n` `p` | first row of the next or previous file |
| `a` `r` | accept or reject (a row: the pending episodes the filter shows, or all it shows when none is pending; an episode decided on its own keeps its decision) |
| `x` | clear: back to pending (a row: every episode the filter shows) |
| `e` | reword, then accept |
| `u` | undo this session's last decision or clear |
| `f` | filter: all, pending, accepted, rejected |
| `Tab` | the files, or back to the findings |
| `v` | a run's folders, or from folders, the newest run that named the selected file (not while a scan or apply runs) |
| `c` | in the files view: check the file again (asks first; while a scan runs, once it ends; refused while apply runs) |
| `A` | apply the accepted fixes of the view (asks first; not while a scan runs) |
| `D` | dry run: what apply would do, writing nothing |
| `o` | open the line in mpv, 2 s early, with its subtitle track; again on the same line replays it |
| Space | pause or resume mpv |
| ← → | mpv back or forward 5 s |
| `,` `.` | mpv back or forward 1 s |
| `[` `]` | mpv back or forward one frame (it stays paused) |
| `?` | show every key, and what the run's LLM calls cost, in a box over the list; `q` quits from it, any other key closes it |
| `q` | quit, closing the mpv window review started; while a scan or apply runs, ask first, and stop it |

The mouse works too. Click a row to select it, and double-click to open it in mpv. The wheel moves
the selection over the list and scrolls the detail pane. `? help` at the end of the status line, and
the editor's `⏎ accept · Esc cancel` there, are clickable. Since review takes the mouse, select text
in the terminal with Shift+drag.

## LLM costs

A scan records what each LLM call used, retries and failed calls too, and after its summary says
what its calls cost. `list` says it for the whole run under its first line (for the files under
some paths, the calls of the scans that made their current LLM checks), and
review in `?`'s box and on the status line as it opens, until the first key or click (as much as
fits beside `? help`: the tokens go first, then the calls, keeping the price):

```
LLM: 98 calls, 1.23M in / 31.2k out tokens, $3.21 at API prices
```

- `claude` reports tokens and a price. The price is what the calls would cost at Anthropic's API
  rates, not what a Claude subscription bills.
- `gpt` (Codex) reports tokens only, so no price is shown: subcheck keeps no price list of its own
  to go stale.
- `ollama:<model>` and `--llm-cmd` report nothing, and nothing is shown; nor for a run made before
  calls were recorded.

The tokens in are all of each call's input, read from the prompt cache or not.

Every call claude or codex started is counted, failed ones too. One that said nothing of what it
used (killed at the 10-minute limit, failing with nothing on stdout, printing something other
than its JSON, or JSON with no usage in it) is counted without numbers, and the sums leave it out;
any call without a price makes the price a floor:

```
LLM: 99 calls (1 reported no usage), 1.21M in / 30.5k out tokens, at least $3.15 at API prices
LLM: 200 calls (none reported usage)
```

A failed call's error is the exit status and stderr, then what the backend's JSON says of it:
`claude exited with exit status: 1; claude said: You've hit your session limit`.

## Where findings are kept

Everything subcheck records is in one SQLite database: `~/.local/state/subcheck/subcheck.db`
(under `$XDG_STATE_HOME` when that is set, `~/Library/Application Support/subcheck/` on macOS), or
the absolute path in `SUBCHECK_DB`. Findings and decisions belong to files: each file has at most
one current LLM check and one current timing check, and a check made again (the file changed, or
`--force`) replaces the one before, whichever scan made it. A decision carries over to a finding
that comes back with the same line and the same fix: a typo you rejected stays rejected when
`apply` changes the file and the next scan reads it again.
A timing fix is the same fix when it moves the same track's stretches by the same frames: timing
rules that change what a finding rests on keep your decision on it.

A run is the record of one scan, and a view of the files it named as they are now: two runs that
share a file show it the same. `review`, `list` and `apply` take a run name, or files and
directories instead, for every file recorded under them:

```
subcheck list ~/Videos/K-On!
subcheck review 2026-10-28T19-04-11
```

With neither, they take the folders that hold the newest run's files: after
`subcheck scan --force E04.mkv`, the whole season E04 is in.

A scan with nothing to check makes no run. On a terminal it opens the screen on its files; in a
pipe it says how to (`Nothing to check: 12 unchanged (subcheck review '/m/K-On!')`).

The database is in WAL mode, so to back it up copy it with
`sqlite3 ~/.local/state/subcheck/subcheck.db ".backup 'subcheck-backup.db'"`, not `cp`, which can
miss what is still in `subcheck.db-wal`.

To look inside, `sqlite3 ~/.local/state/subcheck/subcheck.db`: `scans` and `scan_files` are the
runs, `checks` each attempt to check a file (the current one has no `replaced_by` and no `error`),
then `findings`, `decisions` (a finding's latest is its current one), `applied` and `usage`.

Run files from before the database (`runs/` and `cache.jsonl` in the state directory) are no longer
read, and can be deleted.

## Keeping focus in the terminal

The first `o` starts mpv, and a new window usually takes focus. Later ones load into the same
window and leave focus alone. The ways below rest on two mpv options, `--focus-on` and
`--wayland-app-id`, that an older mpv may lack: one that exits on them is started again without
`--focus-on`, then without both, and without them its window takes focus.

- macOS: nothing to do; subcheck starts mpv with `--focus-on=never`.
- KDE Plasma: mpv's window has the app id `subcheck-mpv`, so a KWin rule can keep focus off it.
  Add this to `~/.config/kwinrulesrc`, then run `qdbus6 org.kde.KWin /KWin reconfigure`:

  ```ini
  [General]
  count=1
  rules=subcheck-mpv

  [subcheck-mpv]
  Description=subcheck mpv: never take focus
  wmclass=subcheck-mpv
  wmclassmatch=1
  acceptfocus=false
  acceptfocusrule=2
  above=true
  aboverule=3
  ```

  If the file already has rules, add `subcheck-mpv` to the end of its `rules=` list and 1 to
  `count=` instead of replacing them. To undo, remove the group and its entry in `rules=`, lower
  `count=`, and reconfigure.

  The rule stops this one window from ever taking keyboard focus. Review's playback keys control
  it; mpv's own keys (`f`, `q`, …) do not reach it. The window closes when the review that started
  it quits or its terminal closes; close one left by any other way from its title bar.
  (KWin's focus stealing prevention would be gentler, but KWin 6.8 on Wayland ignores it per
  window when a window opens.) It also turns Keep Above on when the window opens, because a
  window that is refused focus opens behind the active one, so a maximized terminal would hide
  it. Alt+F3 → More Actions → Keep Above turns that off.

## Audio

`subcheck audio <files or directories>` decodes every audio track with ffmpeg in ten-second
windows spread over each file (0:30, 20, 40, 60 and 80 % in, and a minute before the end) and
cross-correlates every pair of tracks in each. It is for one audio in two codecs (a TrueHD and
its AC-3, a FLAC and an AAC of it), not a dub against its original. Two tracks hold the same
sound when they match in every window, or all but one in four; tracks in different languages
only when every window with sound in both scores 0.5 or more (see below). Each track is
reported against the lower-numbered track it matches best:

```
FMAB - E16.mkv
  track 1  Japanese FLAC 2.0   no other track holds this audio
  track 2  English TrueHD 5.1  reference
  track 3  English AC-3 5.1    34.2 ms late against track 2
           fix: mkvmerge --sync 3:-34, or --sync 2:34 if track 2 is the one off
```

- `in sync`: within 1 ms. Matroska keeps times to the millisecond, so a window can read that much
  off for tracks that are in step.
- `N ms late` (or early): the same in every window, within 3 ms (a lossy track's lag wanders
  that much with its codec).
- `drifts`: the windows disagree, so no one `--sync` fixes it; each window's lag is printed. A sync
  step partway through a track reads like this.
- `no other track holds this audio`: no track matches it; `track N holds this audio too, but goes
  with another track`: track N matches it, but another better (a copy of a copy, say).

A `fix` moves a track onto the one it is reported against. The sound alone cannot say which of
the two is off, so when that one is the top of its group (reported against none), no other
track holds its sound and none is reported against the track to move, moving it instead is given
too. A track in sync with one that has a fix gets the same move. One late or early against a
track that is itself off gets no fix: it names the track to fix first, and `audio` says the rest
once that is done. No `--sync` moves a drift, so a track steady against one is fixed against it,
as against the top of a group.

Measured as ffmpeg plays a file, which is how mpv and Plex do: an AC-3 or AAC whose encoder
priming the file does not record plays late by it, and is reported late (an AC-3 ffmpeg made, by
5.3 ms; an AAC, by about 21). Samples are placed by their timestamps, so a track with frames
missing reads in sync around the gap, not shifted.

A dub shares its original's music and effects, and its dialogue windows tell them apart; but an
episode of action can carry it through every window. So tracks in different languages hold the
same sound only when every window with sound in both scores 0.5 or more, as a clean copy does
(FMAB's dubs reached 0.37 at worst, its AC-3 copies 0.88 or more). A clean copy tagged with the
wrong language still matches, with the two languages said; a poor one does not (K-On's AAC copies
scored 0.26 at worst). Two tracks' languages compare in BCP 47 when both have it (`zh-yue` and
`zh-Hans` differ, `en-US` and `en-GB` do not), else in ISO 639-2 when both have it (`fre` is
`fra`, case aside), and the line names them in that kind, as tagged. A track with no Language
element is `eng`, Matroska's default, unless it has a BCP 47 tag. One tagged only in BCP 47 and
one only in ISO 639-2 count as different languages, as does `und` against any other: nothing says
they are the same, so they need the same 0.5.

`-v` prints every pair's lag and score in every window. The exit status is 1 only when a file or
a window cannot be read; `--ffmpeg` names the program.

## Apply

`subcheck apply` takes the accepted fixes of the newest run's folders (or those of the run named,
or of the files under some paths), with your wording where you reworded one, and makes them in each
file:

1. It finds the events each fix was made from and changes just the words that differ, keeping the
   tags around them (`{\i1}Ihave{\i0}` becomes `{\i1}I have{\i0}`).
2. mkvmerge writes `.<name>.mkv.subcheck` beside the file: the same file with the fixed tracks
   replaced, each in its place, with its name, language, flags and compression.
3. That remux is checked against the original: titles, chapters, attachments, every track's
   header, frame counts, every subtitle event, and the times of the audio frames in six 5-second
   spans (the head, 20 to 80 % in, a minute before the end): a frame moved more than 2 ms fails
   it. With `--full` (or `--no-backup`), every frame of every track too, which reads both files end
   to end.
4. The original becomes `<name>.mkv.orig` and the remux takes its name. The original is linked to
   that name first, so the file is never missing on the way; a filesystem without hard links
   needs `--no-backup`.

A file apply swaps keeps its checks: they move to the swapped file, so the next scan leaves it
alone rather than sending its lines to the LLM again. A remux swapped in by hand (after
`--tracks-only` or `--check`) is checked again.

An existing `<name>.mkv.orig` stops a later apply of that file, since an earlier backup is never
overwritten, and the screen always keeps one. Move or delete the old `.orig`, or use `subcheck
apply --no-backup`.

A fixed track's own tags (a `Source` note, say) are carried over to its replacement. The check does
not compare tags. Any that may not come over exactly are noted in the output, the dry run's too:
a value that is not plain UTF-8 text, a tag that also targets something else (another track, a
chapter, an edition), or an element mkvmerge's tags XML cannot hold.

mkvmerge leaves a text subtitle track's default duration out of any remux and gives each event a
duration of its own instead, rounded to the millisecond. The check expects that rounding, so a file
whose events take their duration from the track's header passes, without the header field.

mkvmerge writes a track's language its own way too: Language lowercase and bibliographic (`fra`,
`Fre` and `fr` as `fre`), and a BCP 47 tag in canonical form (`iw` as `he`, `zh-yue` as `yue`);
one it does not know (`xyz`) as `und`. So the check takes each track's language as mkvmerge reads
the original (`mkvmerge -J`) and expects that in the remux: the tag when there is one (Language
beside it is written from the tag), else Language (the tag mkvmerge adds is no difference).
A fixed track goes into the remux in that language too, so mkvmerge takes it where it would refuse
the file's own (`xyz`). mkvmerge also trims spaces and tabs from a track name it is given, and
leaves an empty one out, which the check expects too.

A fixed track goes through its text format, which cuts each time down to the millisecond (SRT) or
the centisecond (ASS). Where that would change a time, a fix on that track is left for a hand fix
before any remux, and the output names the first such time: an ASS track timed to the
millisecond, say, or events that last a 23.976 fps default duration (41.708 ms, which a remux
rounds to 42, SRT writes as 41 and ASS as 40). A timing fix that moves every such event onto the
format's precision is made. The file's other fixes are made either way. An ASS or SSA track whose
every time is the same whole milliseconds past the centisecond (a mux 7 ms late, say) is made: its
`.ass` (or `.ssa`) holds each time that much early, and the remux puts them back with `--sync`, which the
output says. Where most of its lines start the same whole milliseconds past, the output names the
first time that is not that far past (a start or an end), or whatever else first keeps the track
from being written so.

A timing fix moves each line by whole frames on the frame grid the video's keyframes (its Cues)
give, each time to the same place in its new frame. Some muxes store a frame exactly half a
millisecond past one at either millisecond, both ways in one file. Where a line's move turns on
such a frame, apply takes its time from the Cues if it is a keyframe, and otherwise reads it from
the video around it; video there that does not read fails the file. A line still left a
millisecond unsure leaves its fix for a hand fix, and so does a typo on it once the fix is in.

`--no-backup` keeps no original. `--dry-run` shows each changed event before and after, and writes
nothing. Running apply again is safe: a fix already in its file is reported and left alone.

Some fixes are left for a hand fix, with the reason: per-letter karaoke (one event per letter)
without `--karaoke`, signs set letter by letter (a tag before nearly every letter), a change
across a tag or a line break, two fixes on one line, a track whose times its text format cannot
keep or whose header is not all UTF-8, every fix in a file with any text track (ASS, SSA, SRT,
WebVTT, USF, ASCII) whose lines are not all UTF-8 (mkvmerge replaces those bytes in any remux),
and a line that is not there any more.

`--karaoke` makes per-letter karaoke fixes too. Each changed letter keeps its event (tags,
colours, animation), and a new one is a copy of the letter beside it. The row is then re-spaced
by each letter's width, measured from where the style's other rows put their letters, and stays
anchored as the style aligns it: a centred row grows both ways. A letter the style never shows
gets a typical width, and the output says so. Look at each one afterwards (`subcheck review`,
then `o`). It leaves for a hand fix a row whose letters are timed one by one (`\k` or `\t`
times that differ from letter to letter; the same `\k` on every letter is fine), drawn at
different sizes, or not centred on their `\pos`.

A file in a directory you cannot write to (a library share, say) can be remuxed elsewhere:
`--tracks-only DIR` writes its fixed tracks to DIR and prints the mkvmerge command that remuxes
it to `.<name>.mkv.subcheck` beside the original; `--check` then checks that remux against the
original (with `--karaoke` too when the tracks were written with it; `--tracks-only` prints the
command), which takes mkvmerge here too, to read the original's languages: a remux made by an
mkvmerge that writes them another way fails the check. The swap is yours. Afterwards `subcheck
apply` finds the fixes in place and records them.
