# Status

**Phase:** STUDIO-02 complete on Linux x86_64.
**Updated:** 2026-11-20.
**Next prompt:** `docs/planning/studio-v1/prompts/03-FFMPEG-INPUT.md`, with the
deviations below (run only on the owner's go).

## What works

- **Convert progressive 8-bit or 10-bit Y4M to AV2** as IVF or an Annex B `.obu`
  stream, with real codec calls in an isolated worker process.
- **App (`splot-studio [FILE]`):** one compact window. Open a file → the engine
  probes it → pick Quality (4 steps) and Priority (3 steps) → Convert → real
  stage, counters and elapsed time → Cancel, or a result with each verification
  check and Show in Folder. Advanced holds output format, rate factor, speed
  preset, destination and the full source facts.
- **Engine (`splot-studio-engine`):** `worker` (one job per process, protocol v2)
  and `encode <in> <out>` for the command line. Chroma 4:2:0, 4:2:2, 4:4:4 and
  monochrome at both depths are tested end to end.
- **Safety:** source opened read-only; source ≠ destination by file identity;
  exclusive `.partial` file beside the destination; verified before publishing;
  published without replacing an existing file. Cancel removes the partial file.
- **Verification per job:** Structure (framing and frame count, plus
  `splot-validate`) and Splot decode. The independent AVM check is reported as
  **not run** — no AVM decoder is installed. Splot decoding its own output is not
  independent proof.

## Not supported (rejected with a reason)

MP4/MKV/MOV and every non-Y4M input; interlaced; bit depths other than 8 and 10;
non-square pixel aspect; `XCOLORRANGE=FULL`; frames over the encoder or IVF size
limits or the 8 GiB memory bound (5K/8K). No audio, subtitles, HDR, VFR, resize,
target size, preview, queue or background continuation: closing the window stops
the job.

Y4M states no colour model and the pinned encoder writes none, so the output
carries no primaries, transfer, matrix, range, chroma siting, aspect or timing
(`.obu` has no frame rate at all). The app discloses this per source.

## Deliberate deviations from the pack (owner decisions, 2026-11-20)

- **UI:** one compact "Simple" window (Quality, Priority, Convert, Advanced)
  instead of the three-panel workbench in `05-DESIGN-SYSTEM.md`.
- **STUDIO-03:** ffprobe/ffmpeg child processes, not rsmpeg FFI.
- **STUDIO-04:** GUI-owned durable queue, no supervisor process.
- **Preview** (thumbnail, live frame) stays in STUDIO-05.
- Phase PRs are opened and merged by the agent once CI is green.
- `splot-*` dependencies track `main` until Splot has releases; codec
  limitations are reported and fixed in the Splot repository.
- No `clap`/`anyhow`; arguments are parsed with `std`.

## Measurements

i7-12700H (20 threads), 45 GiB, Linux, Rust 1.100.0, release, portable x86-64.
Clip: ffmpeg `testsrc2` 1280×720, 30000/1001, 60 frames, CRF 20.

| Run | Wall | fps | Peak RSS |
|---|---|---|---|
| 8-bit, preset 6 | 424.8 s | 0.14 | 425 MB |
| 10-bit, preset 6 | 425.6 s | 0.14 | 483 MB |
| 8-bit, preset 10 | 77.5 s | 0.77 | 472 MB |
| 10-bit, preset 10 | 75.9 s | 0.79 | 482 MB |
| upstream `splot encode`, 10-bit, preset 10 (x86-64-v3 build) | 63.9 s | 0.94 | 433 MB |

The speed is the codec's: Studio's wrapper adds nothing measurable. A one-off
x86-64-v3 build of the engine was 1.32× faster with byte-identical output; the
committed build stays portable. Encoding uses under two cores.

Cancel: acknowledged in under 1 ms; finished in 9 ms between frames, otherwise
500 ms (the engine abandons the codec call, removes its file and exits).

## Checks run (Linux x86_64, Wayland)

- `cargo xtask ci` — pass, about 8.5 min cold: fmt, SPDX headers, dependency
  direction, clippy `-D warnings`, 115 tests (domain 10, engine 12 + 14 + 21,
  app 46 + 9 against the real engine, xtask 3).
- App launched with a clip: window sized to content (464×529 logical), clean
  stderr, engine children reaped.
- Evidence: `docs/evidence/studio-02/` — 22 Slint renderer snapshots and one
  real window capture (X11 backend under XWayland).

## Not tested / open

- **Clicking through the real window** (Convert, Cancel, Show in Folder, Open and
  Save dialogs, closing mid-job): synthetic input does not reach the window on
  this host. Covered only by the worker-client tests against the real engine.
- Windows and macOS: never run locally; CI is the first execution. Removing the
  open `.partial` file on forced cancel is untested on Windows.
- AVM differential check; disk-full; screen readers; keyboard walk-through.
- OS file drag-and-drop is not possible with Slint 1.19's winit backend.
- A killed worker (`kill -9`, crash) leaves its `.partial` file behind.
- Quality and Priority mappings are provisional (CRF 36/28/20/12, preset 8/6/3)
  and not yet calibrated; preset 3 is impractically slow at these speeds.
- Chroma siting is neither signalled nor disclosed.
- Memory bound is calibrated at 720p only.

## Provenance and dependencies

Splot `main` at `529f3ee673c83b2469701953d0c0d13f44c517da` (in `Cargo.lock`) for
`splot-encode`, `-decode`, `-validate`, `-core` (dev profile builds them at
`opt-level = 2`). The measurements above were taken at `c1183fec`. `slint` 1.19.0,
`rfd` 0.18.0, `tempfile` 3.27.0, `serde`, `serde_json`, `thiserror`. Linux build
needs `pkg-config` and `libfontconfig-dev`. Cargo fetches Git crates with the git
CLI (`.cargo/config.toml`).
