# Yoru

A video editor written in Rust (egui + FFmpeg).

## Controls
| | |
|---|---|
| Import | drag video files onto the window (they are appended to the timeline) |
| Play / pause | Space  (J / K / L shuttle) |
| Select / Trim / Position(slip) / Razor | V / T / P / B tools; click clips, drag body to move, drag edges to trim |
| Blade at playhead | Ctrl+K (or Ctrl+B) |
| Delete / ripple delete | Delete / Shift+Delete |
| Undo / redo | Ctrl+Z / Ctrl+Shift+Z (or Ctrl+Y) |
| Save / Save as / Open | Ctrl+S / Ctrl+Shift+S / Ctrl+O  (`.yoru` files) |

## Build
`cargo build --release` needs FFmpeg dev libraries (CI installs them with vcpkg, see
`.github/workflows/build.yml`). Unit tests for the editing logic: `cargo test project::`.
