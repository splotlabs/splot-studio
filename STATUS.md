# Status

**Phase:** STUDIO-05 complete on Linux x86_64.
**Updated:** 2026-11-21.
**Next prompt:** `docs/planning/studio-v1/prompts/06-RELEASE-HARDENING.md`.

## What works

- **Convert ordinary local video files** (MP4/MOV, MKV/WebM, MPEG-TS, AVI) and
  Y4M to video-only AV2, as IVF or an Annex B `.obu` stream.
- **How files are read:** Y4M directly; everything else by `ffprobe` (inventory,
  JSON) and `ffmpeg` (decode) as child processes of the worker, piped into the
  same reader. No in-process FFmpeg, no unsafe code. Inputs are limited to local
  files and four demuxer families; playlists, concat scripts and image
  sequences are refused.
- **Accepted video:** progressive, constant frame rate, SDR, limited range,
  square pixels, no rotation; planar 8/10-bit 4:2:0, 4:2:2, 4:4:4 and gray, plus
  `nv12`/`p010le` repacked to planar without changing a value (disclosed).
- **Checked before anything is written:** the job decodes the source once
  (`Stage::Checking`) and requires every frame to keep the probed size, format
  and colour and to sit exactly on the declared frame period (within one
  time-base tick). The decode that feeds the encoder must then deliver exactly
  that many frames.
- **Nothing dropped silently:** the probe inventories every stream and chapter.
  A source with audio, subtitles, attachments, chapters or extra video needs an
  explicit acknowledgement per job; the engine refuses the job without it, and
  the app asks once per run with a confirmation whose default is Cancel. The
  acknowledgement is kept per file only while its fingerprint, stream,
  omissions and settings are unchanged, and is never moved to another file.
- **Stale sources:** a job carries the fingerprint (size, mtime) of the file the
  user looked at; a changed file is refused and re-read.
- **App:** unchanged Simple window, plus the omission summary, the confirmation,
  a video-track picker and the FFmpeg identity under Advanced.
- **Durable file list (STUDIO-04):** several files become a list of rows; Convert
  runs the waiting ones one after another, one encode at a time. Cancel stops
  the current file and pauses the run; a failed file does not stop it. Each file
  takes an immutable snapshot of the settings when it starts.
- **Survives restarts:** the list, settings, destinations and results are kept in
  `queue.json` in the per-user data folder (`~/.local/share/SplotStudio`,
  `~/Library/Application Support/SplotStudio`, `%LOCALAPPDATA%\SplotStudio`),
  rewritten atomically on every state change and never on progress ticks. On the
  next launch nothing starts by itself: the file that was converting is shown as
  Interrupted and Continue runs the rest. A newer or unreadable journal is left
  untouched; an unparsable one is moved aside and reported.
- **Recovery is exact:** the engine announces its partial file (`Event::Working`);
  after a crash the app removes exactly that file, only if it is a regular file
  in the destination's folder with the engine's naming. If the app died while a
  file was being published and a file exists at the destination, it says so and
  never overwrites or claims it.
- **One instance:** a lock file in the data folder; a second instance says the
  app is already open and exits without touching the journal.
- **Preview (STUDIO-05):** a 16:9 picture at the top of the card. It shows a
  poster after a file is added, the **real encoded picture** while converting
  (the worker decodes its own packets with the Splot decoder, at most 4 a
  second) and the result when done. Press and hold (or hold O) shows the
  original at the same frame and view. Scrubber, frame stepping, video-only
  playback at the stream's own timing, Fit / 1:1, thumbnails in the file rows.
- **How pictures travel:** a long-lived `splot-studio-engine preview` process
  serves pictures and playback; a picture is a small 8-bit RGB copy (at most
  1280×720), a JSON header followed by raw bytes on the same pipe. Requests
  carry generation IDs; stale answers are dropped; latest-picture slots, never
  queues. One display transform for source, result and live pictures; an
  unstated matrix is assumed by size and labelled "colours assumed". SDR
  approximation, not a reference display, and the app says so.
- **Bounds:** engine picture cache 64 MiB, app cache 32 MiB, preview decoder 2
  threads, live decoder 1 thread; the encode never waits for a picture and its
  output is byte-identical with and without live pictures.
- **Seeking:** Y4M by offset; other files seek to the key frame before the
  frame (checked per seek, falling back to exact counting; MPEG-TS always
  counts); results restart at most one key-frame interval (120 frames) back.
- **Running it:** `cargo xtask run --release` builds the app and its engine
  together; `cargo run -p studio-app` alone can start a stale engine.
- Everything from STUDIO-02 (safe publish, verification, bounded cancel) applies
  to both routes. Cancel and a killed worker leave no `ffmpeg`
  child (Linux).

## Not supported (rejected with a reason)

Variable frame rate; interlaced; HDR (PQ, HLG, Dolby Vision or HDR side data);
full range (`yuvj*`, `color_range=pc` — gray files made by FFmpeg 8 are tagged
full range and therefore refused); non-square pixels; rotation, cropping, stereo
or spherical side data; RGB, alpha, other bit depths; size, format or colour
changing mid-stream; encrypted streams; network or playlist inputs; frames over
the size or 8 GiB memory limits. No audio, subtitles, resize, target size,
reordering, pause or background continuation: closing the window stops
the current file.

The pinned encoder writes no colour description, so the output never says how to
interpret its samples (splotlabs/splot#1777); the app discloses this per source.

## FFmpeg

- **Development:** commands run through Cargo opt in to the machine's FFmpeg
  (`SPLOT_STUDIO_SYSTEM_FFMPEG=1` in `.cargo/config.toml`); the app labels it as
  a development setup. Tested with 8.0.1 only. The test suite needs it and fails
  clearly without it.
- **Release:** the engine otherwise uses only `ffmpeg`/`ffprobe` beside itself.
  **That controlled LGPL build does not exist yet** — a STUDIO-06 release
  blocker, see `native/ffmpeg/README.md`. Without any FFmpeg the app reads Y4M
  only and says so.

## Deliberate deviations from the pack (owner decisions, 2026-11-20)

- **UI:** one compact "Simple" window (Quality, Priority, Convert, Advanced)
  instead of the three-panel workbench in `05-DESIGN-SYSTEM.md`.
- **STUDIO-03:** ffprobe/ffmpeg child processes, not rsmpeg FFI.
- **STUDIO-04:** GUI-owned durable queue in one atomically written JSON file;
  no supervisor process, no socket IPC, no SQLite; nothing resumes by itself
  after a restart; Permute-style file list.
- **STUDIO-05 (2026-11-21):** stills plus playback; the live frame is the real
  encoded picture; the preview sits in the card, not in its own window.
  Pictures share the control pipe (header JSON, raw pixels after it) instead
  of a physically separate channel: one reader, one ordering, no base64. No
  GPU path: the software image path met the measured needs.
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

Ordinary files (320×180, 30 frames, preset 10, release): 8.5–8.7 s, 75–80 MB peak;
probe 0.08 s, frame check plus decoder start about 0.07 s. A 600-frame 720p H.264
check took 0.20 s.

Cancel (release, over the worker pipes, sent mid-mini-GOP): `Cancelled` after
3–5 ms at 720p 8-bit preset 6 and 720p 10-bit preset 10, 4–7 ms at 1080p preset
10, 12–14 ms for a 720p H.264 MP4 (`ffmpeg` reaped first); always cooperative,
the worker kept serving until its input closed.

Preview (release, same host under other load; `docs/evidence/studio-05/`):
720p playback in real time for Y4M, MP4 and 8-bit results, 26 of 30 frames for
a 10-bit result. Cold far frame of a 10-minute clip 242 ms (MP4) / 588 ms
(MKV); next frame 2–5 ms; result backward seek 256 / 420 ms (8 / 10 bit).
Live pictures: overhead inside run-to-run noise (about 20 % on this loaded
host), estimated 1–2 % of job CPU, 40–80 MB more peak RSS; the first one
arrives when the encoder emits its first packet (32–36 s at 720p preset 10).
Idle with a poster shown: 0 CPU ticks in 20 s. UI thread during an encode:
posted closure waited p95 under 0.2 ms, max 4.9 ms.

## Checks run (Linux x86_64, Wayland)

- `cargo xtask ci` — pass: fmt, SPDX headers, dependency direction, clippy
  `-D warnings`, 295 tests (domain 16, engine 38 + 15 + 6 + 15 + 14 + 1 + 27,
  app 134 + 3 + 8 + 1 + 1 + 13 against the real engine, xtask 3). Clippy is
  also clean for
  `x86_64-pc-windows-msvc` and `aarch64-apple-darwin`.
- Real-process fault tests: worker killed mid-job; app killed mid-job (a real
  second process, SIGKILL); app death during publication; destination taken
  before, during and between attempts; source edited between probe and start;
  second instance against a held lock.
- Real app by hand: two files on the command line → two waiting items in the
  journal; a second instance left the journal byte-identical; relaunch restored
  the list and ran nothing.
- Evidence: `docs/evidence/studio-04/` and `docs/evidence/studio-05/` (Slint
  renderer snapshots, engine and app measurements).

## Not tested / open

- **Preview:** press-and-hold, scroll-panning in 1:1 and dragging the scrubber
  are unexercised in the real window (keys and callbacks are tested headless).
  Minimizing does not stop playback on Wayland (no signal). Live pictures
  cannot be stopped or resized mid-job, so a minimized window still costs the
  live decode. About 40 MB of app memory growth on the first passes through a
  full cache is unexplained (it levels off). 1080p/4K preview speed unmeasured.
  The preview trusts the frame-period rule without the job's full scan, so a
  variable-rate file (which a job refuses) shows the nearest frame.
  `preview.rs` in the app is 1,113 lines.
- **Splot decoder gaps** behind the seek costs: no decoder reset/seek or
  cancel, no random-access test or frame type on pictures, Annex B reader
  yields single OBUs, no file seek index (to be filed upstream).
- **Clicking through the real window** is still unexercised on this host. The
  confirmation's keyboard behaviour is tested against the real compiled window
  running headless.
- Windows and macOS runtime, and FFmpeg versions other than 8.0.1 (Ubuntu 24.04
  CI ships 6.1). Several checks read ffprobe field and side-data names that may
  differ between versions.
- AVM differential check; disk-full; screen readers; Matroska encryption.
- The tools open the source by path, so a file swapped between checks is caught
  by fingerprint re-checks, not prevented.
- Scan and decode have no whole-file timeout; they stop on cancel.
- Damage is caught only when FFmpeg reports it. A codec without checksums can
  decode damaged data as valid frames; nothing downstream can tell.
- If the app dies between the engine's rename and its own journal write, the
  journal still says running; recovery then reports that a file exists at the
  destination but cannot prove it is this job's. Closing that needs an engine
  handshake.
- The worker-kill tests are Linux only and the app-crash test is skipped on
  Windows (its helper process never reports ready on the CI runner; not yet
  investigated on a real machine), so crash recovery is unproven on Windows. Directory fsync, the lock
  file and rename-over-existing on Windows/macOS are first exercised by CI.
- Disk full and a read-only data folder are unit-tested only.
- Keyboard handling of the list and screen readers are untested by hand.
- Quality and Priority mappings are provisional and uncalibrated.
- OS drag-and-drop is not possible with Slint 1.19's winit backend.
- Cancel reaches a running codec call through the encoder's `CancelToken`
  (measured 3–14 ms). The forced worker exit is only a backstop after 3 s
  (`CANCEL_GRACE`); it is unit-tested, not exercised end to end.

## Provenance and dependencies

Splot `main` at `e05a86fe866f35f891e8d7fa17454af27922106c` (in `Cargo.lock`) for
`splot-encode`, `-decode`, `-validate`, `-core` (dev profile builds them at
`opt-level = 2`). The measurements above were taken at `c1183fec`. `slint` 1.19.0,
`rfd` 0.18.0, `tempfile` 3.27.0, `directories` 6.0.0, `serde`, `serde_json`, `thiserror`. Linux build
needs `pkg-config` and `libfontconfig-dev`. Cargo fetches Git crates with the git
CLI (`.cargo/config.toml`).
