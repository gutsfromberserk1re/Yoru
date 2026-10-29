# Yoru

A video editor written in Rust (egui + FFmpeg).

## Getting media in
Drag video files onto the window, or drag clips from the **Clips** tab onto a track
(Ctrl while dropping = insert instead of overwrite). Double-click a clip to append it.

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

## Playback resolution
The **Playback** menu above the monitor (Full, 1/2, 1/4) lowers the decode + preview resolution for
smoother playback on heavy footage. Default is 1/2.

## Build
`cargo build --release` needs the FFmpeg development libraries (CI installs them with vcpkg, see
`.github/workflows/build.yml`). Unit tests: `cargo test`.
