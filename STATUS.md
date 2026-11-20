# Status

**Phase:** STUDIO-01 complete on Linux x86_64; awaiting owner review.
**Updated:** 2026-11-20. Nothing committed or pushed.
**Next prompt:** `docs/planning/studio-v1/prompts/02-RAW-VIDEO.md` (run only on the owner's go).

## What exists

- `studio-domain`: `OutputKind`, `SpeedPreset` (0–10), `Crf` (1–63), `EncoderSettings`;
  control protocol v1 (little-endian `u32` length + JSON `Envelope`, 64 KiB cap,
  version checked before the body, `Command::Hello`, `Event::{Hello, Error}`).
- `splot-studio-engine`: `--help`, `--version`, `handshake` (framed stdin/stdout).
  Reports `encode_outputs: []`. **It cannot encode.**
- `splot-studio`: Slint window with menu bar (File › Open…, Quit; Help › About),
  toolbar, source sidebar, empty state, settings inspector, status bar, light/dark
  following the system, About dialog with the `AboutSlint` widget, native Open
  dialog (sets a label only — the file is not opened or probed), real engine
  handshake result in the status bar. Encode is disabled and says why.
- `cargo xtask ci`, `cargo xtask ci --fast`, `cargo xtask doctor`.
- `.github/workflows/ci.yml` for `ubuntu-24.04`, `windows-2025`, `macos-15`.

## Provenance

- Splot baseline `c1183fec4b81d72045d861973a9d8e563c6e9e2e`; AGENTS blob
  `daf08ea45c6386223f791b1d2046179a357c92bd`, identical on `main` at inspection.
  `main` was 2 commits ahead (`ac1f5ecf`, `00097ef8`, encoder-only); not adopted.
- `LICENSE.md` is the byte-exact file from that commit. Its Required Notice names
  “splot”, not Splot Studio — owner to decide whether it needs its own notice.
- `AGENTS.md`, `CLAUDE.md`, `.github/copilot-instructions.md` are the pack templates,
  unmodified. The pack is in `docs/planning/studio-v1/` (manifest checksums verified).
- Remote `splotlabs/splot-studio` already existed with one commit and is **public**
  (the pack assumed a private staging repo). Not created, changed, or pushed to here.

## Deliberate deviations from the pack (owner-approved 2026-11-20)

- **STUDIO-03:** ffprobe/ffmpeg as child processes piping into the worker's Y4M
  path, instead of rsmpeg FFI. No `studio-media` unsafe adapter.
- **STUDIO-04:** the GUI owns the durable queue and its journal; no supervisor
  process or local-socket IPC. An encode does not survive the GUI closing.
- **Pacing:** stop after every phase.
- Engine and xtask parse arguments with `std`; no `clap` or `anyhow` yet.

## Dependencies (direct, locked)

`slint` and `slint-build` 1.19.0 (features `std`, `compat-1-18`, `backend-winit`,
`renderer-femtovg`, `renderer-software`, `accessibility`, `raw-window-handle-06`;
Royalty-free 2.0 route, licence file read in the crate), `rfd` 0.18.0
(`xdg-portal`, `wayland`; no GTK, no async runtime), `serde`, `serde_json`,
`thiserror` 2. Linux build needs `pkg-config` and `libfontconfig-dev`.

## Checks run (Linux x86_64, Wayland, Rust 1.100.0)

- `cargo xtask ci` — pass: fmt, SPDX headers, dependency direction, clippy
  `-D warnings`, 37 tests (7 domain, 6 engine unit, 13 engine process, 8 app, 3 xtask).
- `cargo xtask doctor` — pass.
- App launched on Wayland (FemtoVG) and with `SLINT_BACKEND=winit-software`: stayed
  up, clean stderr, engine child reaped, 0 CPU ticks idle over 10 s.
- Evidence in `docs/evidence/studio-01/`: one real window capture (X11 backend under
  XWayland, via `xwd`) and six Slint renderer snapshots (not OS screenshots).

## Not tested / open

- Windows and macOS: nothing built or run. The CI workflow is configured, never executed.
- Keyboard (Tab order, shortcuts, Escape), the Open dialog, opening About from the
  menu, and screen readers were not exercised by hand.
- Scaling other than 2×; multi-monitor.
- Known rough edges: no timeout if the engine hangs during the handshake; the About
  popup has no backdrop; primary buttons use the Slint style accent because std
  widgets cannot be recoloured (`accent`/`focus-ring` tokens defined but unused).
