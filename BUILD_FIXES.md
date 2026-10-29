# Build fixes included in this archive

This archive supersedes the earlier `Yoru-full-render-effects-audio-proxy.zip` source.

- `src/engine.rs`: `invalidate_media` removes the decoder slot as `mut`, allowing the player to be stopped before the slot is dropped.
- `src/export.rs`: graphic text placement is generated as FFmpeg expressions. It no longer performs the old `f64 + f32` operation that failed at `export.rs:252`.
- Text left/center/right alignment uses the graphic box width consistently.

## Important when updating GitHub

Extract this archive and **replace the files in the repository**, including `src/export.rs` and `src/engine.rs`. Do not merely upload the ZIP as an extra file or copy only the README. The build log that reported `cx as f64 + w * 0.5` and `if let Some(slot)` came from the older source tree. Those expressions are not present in this updated source.

The source archive has been integrity-checked, but a Rust release build still needs to be confirmed by the included GitHub Actions workflow.


## Latest Actions build log fix

- Fixed the `FilmGrain` export filter by passing its `f32` parameter through `fnum(...)` before formatting it as an FFmpeg number.
- Removed an unused text-position variable and two unnecessary-parentheses warnings in the FX renderer.
- Retained the prior mutable player-slot fix in `src/engine.rs` and the corrected text-alignment expression in `src/export.rs`.

The project has not been compiled locally in this environment; run the Windows GitHub Actions workflow to confirm the full build.
