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

Graphics and audio settings are saved in the YORU project format. This build reads older YORU 1–8 projects; YORU 9 adds additional effect, transition, and audio-preset IDs.
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


## Video effects

Yoru's Effects browser includes non-destructive Wave Warp, Ripple, Twirl, Gaussian Blur, Lens Blur,
Directional Motion Blur, Glow, Sharpen, RGB Split, Vignette, Pixelate, Sepia, Black & White,
Invert, Posterize, Film Grain, Scanlines, Mirror, Edge Detect, Color Temperature, VHS/Tape, Flash,
and fades. Drag an effect onto
a video clip in the timeline to apply it. Select the clip and use **Effect Controls → Applied Effects**
to enable/disable, reorder, reset, tune, or remove each effect. The timeline displays an `FX` badge and
the number of enabled effects on a clip. Keyframe interpolation supports Linear, Ease In, Ease Out,
Ease In/Out, and Hold; new keys start Linear and can be changed from the value graph.

Effects are evaluated in the monitor preview and the export filter graph. Because the preview uses
custom CPU kernels while export uses FFmpeg filters, some visual details can differ; animated keyframe
support also varies by filter. Always check a short render before a long export.

## Export, transitions, audio automation, and proxies

- **Export MP4** from the monitor's Export button. Choose 480p/720p/1080p/4K, frame rate, and video bitrate. The renderer composites visible video tracks in track order, title/shape clips, supported effects, and inbound transitions, then mixes the visible audio clips with gain, keyframed gain, fades, and a limiter. Exports are rendered to a staging file and finalized only after FFmpeg succeeds.
- **Keyframe effects** from Effect Controls by clicking the diamond at the playhead. Parameter tracks are saved in `.yoru` projects; choose Linear, Ease In, Ease Out, Ease In/Out, or Hold for each keyframe segment.
- **Transitions** can be applied on a selected video clip in Effect Controls → Transition In. Supported transitions are Cross Dissolve, Dip to Black, Wipe Left, and Flash. Their duration and enabled state are stored in the project and marked on the timeline.
- **Audio gain automation** is available in the Audio inspector. Click the diamond next to Volume to add/remove a keyframe, change the gain at different positions, and select key easing. Export mixes all non-hidden audio clips from the timeline with per-clip gain, fades, automation, and a final limiter. Live monitor playback still follows the active video source's audio clock; a full low-latency real-time multitrack monitor mixer remains future work.
- **Preview proxies** can be generated for the selected Project-bin item with Generate proxy. A 960px H.264/AAC copy is stored under `%LOCALAPPDATA%\Yoru\proxies` and used for interactive decoding when present. The source path remains unchanged and export always uses the original file. Proxy paths are stored in YORU 7–9 projects; older YORU 1–6 projects still load.
- Preview decoding uses a bounded frame queue and coalesces stale frames during seeking. A generated proxy can materially improve playback with high-resolution/long-GOP source footage, while quarter/half-resolution preview quality remains available.

## Export executable and GitHub Actions

The Windows Actions workflow builds the Rust/FFmpeg bindings with vcpkg, then downloads and bundles a separate static FFmpeg CLI as `ffmpeg.exe`. Yoru uses that executable for MP4 export and proxy generation, so users do not have to install FFmpeg manually. The package includes an FFmpeg build notice and license documents from the selected build archive. The CLI package is large (roughly 185 MB before compression) and adds a download step to CI.

Effect appearance in final output is implemented through FFmpeg filters; some filters are approximations of the monitor's custom CPU renderer, and not every filter parameter currently supports frame-by-frame animated interpolation in export. Always inspect a short export test before rendering a large project.

## Expanded creative presets (YORU 9)

- **Video effects:** Wave Warp, Ripple, Twirl, Gaussian/Lens/Motion Blur, Glow, Sharpen, RGB Split, Vignette, Pixelate, Sepia Tone, Black & White, Invert, Posterize, Film Grain, Scanlines, Mirror, Edge Detect, Color Temperature, VHS/Tape, Flash and fades. Effect instances and their keyframes are non-destructive and saved per clip.
- **Transitions:** Cross Dissolve, Dip to Black, Flash, left/right/up/down wipes, Circle Iris, Diagonal Wipe, and Diamond Iris.
- **Text animation presets:** Fade In, Rise Up, Slide Left/Right, Drop In, Pop In, Float In, Fade Out, Zoom In, Soft Zoom, Bounce In, Slam In, Rotate In, Drift Up/Down, Scale Pulse, and Soft Reveal (a slide/fade entrance, not per-character typing). Presets create regular editable transform/opacity keyframes.
- **Audio presets:** Neutral, Voice Clear, Music Bed, Cinematic, Telephone/Radio, Lo-Fi, Underwater, Podcast/Spoken Word, Bass Boost, Bright Voice, Room Reverb, and Muffled/Distant. These non-destructive filters are applied in export; the preview audio path does not yet render the tonal processing.
- Audio presets are saved with the project in YORU 9; existing YORU 1–8 files load with Neutral as the default. New IDs are append-only so saved effect and transition data keeps its meaning.


## Build fix archive notice

This archive supersedes the earlier full-render build. If updating an existing GitHub
repository, replace the source files, especially `src/export.rs` and `src/engine.rs`; the
previous text-position type error and mutable-player error have been addressed in this source.
See `BUILD_FIXES.md` for details.
