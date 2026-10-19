# subcheck

Proofread the text subtitles embedded in MKV files with an LLM, then review the fixes.

- `subcheck extract` writes a subtitle track out the way mkvextract does, reading only the
  subtitle packets (through the file's Cues) instead of the whole file.
- `subcheck scan` sends the English lines to an LLM and records each suggested fix in a run file
  under `~/.local/state/subcheck/`, with each track `timing` finds shifted. `--run <run>` adds to
  that run only the checks it lacks: the timing check, say, for a run made before it existed. An
  LLM check the cache has from another run of the unchanged file is not made again.
- `subcheck review` steps through a run's fixes to accept, reject or reword each one.
- `subcheck list` prints a run's fixes as text; `--mpv` adds a command that opens each one.
- `subcheck apply` makes a run's accepted fixes in the MKV files, keeping each original as
  `<name>.mkv.orig`: a file's typos and timing fixes in one remux, each shifted line moved by whole
  frames on the video's frame grid. Needs mkvtoolnix.
- `subcheck timing` reports subtitle tracks shifted by whole frames against the video, with the
  `mkvmerge --sync` that fixes a whole-track shift. A track with too few lines on cuts is timed by
  the lines it shares with another track. No LLM, nothing recorded.

## Review

A fix repeated across episodes (an OP lyric, say) is one row; `⏎` expands it into its episodes. A
season's timing findings on one track are one row too, whatever each episode's fix (`−1 frame ×23 ·
partial ×1`): `o` opens one paused on the frame that shows the error, `[` `]` step from there, and
`e` does not apply (a timing fix is accepted or rejected).

| Key | Does |
|---|---|
| `j` `k` ↑ ↓ `g` `G` PgUp PgDn | move |
| `n` `p` | first row of the next or previous file |
| `a` `r` | accept or reject (a row: every episode the filter shows) |
| `e` | reword, then accept |
| `u` | undo this session's last decision |
| `f` | filter: pending, all, accepted, rejected |
| `o` | open the line in mpv, 2 s early, with its subtitle track; again on the same line replays it |
| Space | pause or resume mpv |
| ← → | mpv back or forward 5 s |
| `,` `.` | mpv back or forward 1 s |
| `[` `]` | mpv back or forward one frame (it stays paused) |
| `q` | quit, closing the mpv window review started |

The mouse works too. Click a row to select it, and double-click to open it in mpv. The wheel moves
the selection over the list and scrolls the detail pane. The key bar and the playback buttons are
clickable. Since review takes the mouse, select text in the terminal with Shift+drag.

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

## Apply

`subcheck apply` takes the newest run's accepted fixes (or those of the run named), with your
wording where you reworded one, and makes them in each file:

1. It finds the events each fix was made from and changes just the words that differ, keeping the
   tags around them (`{\i1}Ihave{\i0}` becomes `{\i1}I have{\i0}`).
2. mkvmerge writes `.<name>.mkv.subcheck` beside the file: the same file with the fixed tracks
   replaced, each in its place, with its name, language, flags and compression.
3. That remux is checked against the original: titles, chapters, attachments, every track's
   header, frame counts, and every subtitle event. With `--full` (or `--no-backup`), every frame of
   every track too, which reads both files end to end.
4. The original becomes `<name>.mkv.orig` and the remux takes its name. The original is linked to
   that name first, so the file is never missing on the way; a filesystem without hard links
   needs `--no-backup`.

A fixed track's own tags (a `Source` note, say) are carried over to its replacement. The check does
not compare tags. Any that may not come over exactly are noted in the output, the dry run's too:
a value that is not plain UTF-8 text, a tag that also targets something else (another track, a
chapter, an edition), or an element mkvmerge's tags XML cannot hold.

mkvmerge leaves a text subtitle track's default duration out of any remux and gives each event a
duration of its own instead, rounded to the millisecond. The check expects that rounding, so a file
whose events take their duration from the track's header passes, without the header field.

A fixed track goes through its text format, which cuts each time down to the millisecond (SRT) or
the centisecond (ASS). Where that would change a time, a fix on that track is left for a hand fix
before any remux, and the output names the first such time: an ASS track timed to the
millisecond, say, or events that last a 23.976 fps default duration (41.708 ms, which a remux
rounds to 42, SRT writes as 41 and ASS as 40). A timing fix that moves every such event onto the
format's precision is made. The file's other fixes are made either way.

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
command). The swap is yours. Afterwards `subcheck apply` finds the fixes in place and records them.
