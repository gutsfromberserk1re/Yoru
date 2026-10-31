# Yoru performance profiling

Yoru now has a small per-frame timing HUD so optimization can start from measurements rather
than assumptions.

## Timing HUD

- Click **Perf** in the monitor toolbar, or press **F3**.
- `DECODE` measures time spent receiving/decoding the displayed frame. During a seek, time spent
  decoding and discarding frames before the target is accumulated into the displayed frame's value.
- `CONVERT` measures hardware-surface readback when needed, FFmpeg `swscale` conversion to RGBA,
  plus the row-padding removal copy.
- `EFFECTS` measures the CPU compositor/effect stack.
- `IMAGE` measures construction of egui's `ColorImage` from the RGBA frame.
- `UPLOAD` measures the CPU time spent calling egui's texture `set`/`load_texture` API. It is a
  texture-update submission measurement, **not** a GPU fence or proof that the GPU has finished
  transferring the texture.
- `TOTAL` is the sum of those measured stages. It is not full end-to-end frame latency: queue wait,
  UI scheduling, GPU execution, vsync and monitor scanout are not included.

The HUD shows the latest presented frame rather than a rolling average. Record a 30–60 second
sample and note typical values and spikes. Compare runs with the same source, preview quality,
window size, effects and playback state. Also record CPU, GPU, RAM, OS, graphics driver and Yoru
build so results are reproducible.

## Benchmark media

On Windows, run the generator from a terminal that can find `ffmpeg.exe`:

```powershell
powershell -ExecutionPolicy Bypass -File .\benchmarks\generate-media.ps1
```

It creates short deterministic stress clips in `benchmarks\media`:

- 4K H.264 with a long GOP (seek/decode stress)
- 4K HEVC Main 10 / 10-bit (pixel format and decode stress)
- 4K ProRes 422 (intra-frame decode/conversion baseline)

The script requires an FFmpeg build with `libx264`, `libx265` and `prores_ks` encoders. It fails
with an explicit message if generation fails; no media is bundled in the source archive.

## Benchmark scenarios

1. Open each generated clip individually. Test normal playback, repeated backward frame stepping,
   and scrubbing repeatedly between the same two points. Record HUD values and subjective seek
   latency separately.
2. Build a 3-track sequence using the generated clips. Place all three on V1/V2/V3 over the same
   time range, then apply two effects to the upper clip(s). Record preview quality and the exact
   effects used. This tests the current preview compositor's limitations as well as the effect path.
3. Repeat at Full, 1/2 and 1/4 preview quality. Keep the monitor size fixed between runs.
4. Compare source playback with a generated proxy where available. Do not compare proxy playback
   against source playback without recording which file was decoded.

## External profiling

For Rust CPU sampling, use `cargo flamegraph --release --bin yoru` on a supported Linux system with
`perf` configured. On Windows, capture the Yoru process with Windows Performance Recorder and inspect
CPU Usage (Sampled) in Windows Performance Analyzer. The HUD is intended to identify which stage to
investigate; a system profiler is still needed to find hot functions inside a stage.

For correctness/regression checks, run `cargo test --release`. For a repeatable comparison, keep the
same benchmark clips and sequence and compare stage timings before and after each optimization.

## Hardware decoding (Windows preview)

Yoru attempts hardware decoding for codecs that advertise a matching FFmpeg hardware config.
The order is CUDA/NVDEC, D3D11VA, then DXVA2. Device creation and codec support are checked at
runtime; unavailable devices, unsupported codecs, and decoder-open failures fall back to the
existing software decoder. The Perf HUD reports the pixel format actually used by each presented
frame (`CUDA`, `D3D11VA`, `DXVA2`, or `SOFTWARE`) rather than merely reporting which device was
attempted.

The current effects/compositor path remains CPU-based. Hardware-decoded surfaces are therefore
transferred back to system memory before `swscale`, CPU effects, and egui texture submission. This
reduces CPU codec work where the driver/codec combination supports it, but it is **not** a zero-copy
GPU preview. GPU-native texture import, scaling, effects, track compositing, and export remain later
milestones. Non-Windows builds continue to use software decoding for now.

The Windows CI dependency now requests vcpkg's `nvcodec` feature. NVIDIA decoding additionally
requires a compatible NVIDIA GPU and driver at runtime; systems without one automatically use
D3D11VA, DXVA2, or software decoding.

### Verifying the selected path

1. Build and run Yoru on a Windows machine with a supported GPU driver.
2. Open a known H.264 or HEVC clip and enable **Perf** (F3).
3. Check the `BACKEND` row. It should show the actual decoded frame format, not just `AUTO`.
4. Repeat with the GPU disabled or on a system without compatible acceleration; `SOFTWARE` should
   appear and the clip should still open and play.
5. Compare identical media, preview quality, and effects settings. Hardware decode alone does not
   guarantee a faster end-to-end preview while CPU readback and CPU effects remain in the pipeline.
