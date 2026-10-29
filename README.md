# Yoru

A video editor written in Rust (egui + FFmpeg).

## Getting media in
Use **Project → + Import** to add video files to the project bin without filling the timeline.
You can also drop video files onto the window to import and append them to V1. Drag a project-bin
item onto a track to place it (Ctrl while dropping = insert instead of overwrite), or double-click
an item to append it. Grid and List are two views of the same media pool.

## Timeline (Premiere-style)
| | |
|---|---|
| Tools | V select, B ripple edit, N rolling edit, Y slip, C razor, Z zoom, H hand |
| Select | click, Shift-click, drag a box; Alt-click selects a clip without its linked audio |
| Move / trim | drag a clip (also between tracks); drag its edges to trim |
| Add edit (blade all tracks) | Ctrl+K  (Razor tool + Shift = all tracks) |
| Delete / ripple delete | Delete / Shift+Delete |
| Snapping | S toggles (magnet pill); snaps to clip edges and the playhead |
| Zoom | + / - , Alt+wheel or Ctrl+wheel at the cursor, \\ fits the project |
| Play / shuttle | Space, J K L, arrow keys step a frame, Up/Down jump between cuts |
| Insert / overwrite at playhead | `,` / `.` with a clip selected in the browser |
| Track buttons | L locks a track, E hides a video track, M mutes an audio track |
| Undo / redo | Ctrl+Z / Ctrl+Shift+Z (or Ctrl+Y) |
| Unlink video + audio | Ctrl+L |
| Save / Save as / Open | Ctrl+S / Ctrl+Shift+S / Ctrl+O (`.yoru` files) |

## Autosave and recovery
Yoru writes a recovery copy after edits settle (and at least every 20 seconds during continuous
editing). On Windows it lives in `%LOCALAPPDATA%\Yoru\autosave.yoru`; on Linux/macOS it uses
`$XDG_DATA_HOME/yoru/autosave.yoru` or `~/.local/share/yoru/autosave.yoru`. At startup, a valid
recovery copy triggers a prompt to **Recover project** or **Discard autosave**. A successful manual
save or opening another project clears the recovery copy.

## Effect Controls
Select a video clip. Every value has a stopwatch: click it to animate, then change values at
different playhead positions to create keyframes (diamond button adds/removes one, arrows jump
between them). The value graph shows the curve: drag keys, double-click to add one, right-click to
remove, click a diamond to pick its easing.


## Titles, shapes, and Essential Graphics
The inspector's **Text** tab creates editable title clips, rectangles, and ellipses at the playhead.
**Essential Graphics** edits fill/outline colors, opacity, shape dimensions, text background, padding,
font mode, alignment, and shadow. Graphic clips render over the monitor preview and use the same
keyframed transform controls as video clips.

## Audio controls
Select an audio clip and open the **Audio** tab to adjust gain (0–400%) and linear fade-in/fade-out
durations. Playback gain is applied by the audio output callback without restarting playback.

Graphics and audio settings are saved in the YORU 4 project format; YORU 1–3 projects remain readable.
They are included in the existing clip-snapshot undo/redo history.

## Playback resolution and seeking
The **Playback** menu above the monitor (Full, 1/2, 1/4) lowers the preview conversion resolution
for smoother playback on heavy footage. Default is 1/2. During a seek or paused frame step, Yoru
now discards frames before the target before scaling/copying them to RGBA, and drains stale queued
preview frames so an old frame does not sit ahead of the requested one. If a container rejects a
timestamp seek, Yoru reopens the source and scans forward as a slower fallback rather than leaving
the preview stuck at the previous position.

Projects now reopen their media decoders and thumbnail workers from the stored source paths. If a
source file has moved or been deleted, that source still needs to be restored to its recorded path.

## Build
`cargo build --release` needs the FFmpeg development libraries (CI installs them with vcpkg, see
`.github/workflows/build.yml`). Unit tests: `cargo test`.
