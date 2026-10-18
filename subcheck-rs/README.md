# subcheck

Proofread the text subtitles embedded in MKV files with an LLM, then review the fixes.

- `subcheck extract` writes a subtitle track out the way mkvextract does, reading only the
  subtitle packets (through the file's Cues) instead of the whole file.
- `subcheck scan` sends the English lines to an LLM and records each suggested fix in a run file
  under `~/.local/state/subcheck/`.
- `subcheck review` steps through a run's fixes to accept, reject or reword each one.
- `subcheck list` prints a run's fixes as text; `--mpv` adds a command that opens each one.
- `subcheck timing` reports subtitle tracks shifted by whole frames against the video, with the
  `mkvmerge --sync` that fixes a whole-track shift. No LLM, nothing recorded.

## Review

A fix repeated across episodes (an OP lyric, say) is one row; `⏎` expands it into its episodes.

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
