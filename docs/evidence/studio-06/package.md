# STUDIO-06 packaging: what was run

Run 2026-11-21 on Linux only. Host: Intel i7-12700H (20 threads, has
x86-64-v3), 45 GiB, Ubuntu 26.04.1 (glibc 2.43), kernel 7.0.0, Rust 1.100.0.
The host was shared with other builds (load average 4–10), so times vary
between runs. Tree: `d311e4a` plus the uncommitted STUDIO-06 changes, so every
`BUILD.json` here says `"dirty": true`.

**The project's FFmpeg build (`native/ffmpeg/out/linux-x86_64/`) did not exist
yet.** Every package below was made with `--ffmpeg-dir` pointing at copies of
the host's Ubuntu `ffmpeg`/`ffprobe` 8.0.1 (GPL-configured, dynamically linked):
a **placeholder**, named `…-placeholder-ffmpeg-…`, never to be distributed.
Not tested with the real build: the bundled tools' own start-up libraries, the
`MANIFEST.json`/`licenses/` staging, and the LGPL configuration check on a
real pair. Windows and macOS packaging have not run at all.

## Package

```text
cargo xtask package --ffmpeg-dir <placeholder dir> --smoke
```

- App: `cargo build --release --locked -p studio-app --target
  x86_64-unknown-linux-gnu --target-dir target/package/cargo/…-portable`, plain
  x86-64, with `SPLOT_STUDIO_ENGINE_CPU=x86-64-v3` and the commit compiled in.
- Engine: the same with `-Ctarget-cpu=x86-64-v3` in its own target directory.
- Both with `CARGO_ENCODED_RUSTFLAGS` (overrides `RUSTFLAGS` and
  `build.rustflags`) and `--remap-path-prefix` for the checkout and Cargo home:
  `strings` finds no `/home/…` path in either executable.
- `ok: the app checks exactly rustc's x86-64-v3 features: avx, avx2, bmi1,
  bmi2, cmpxchg16b, f16c, fma, fxsr, lzcnt, movbe, popcnt, sse, sse2, sse3,
  sse4.1, sse4.2, ssse3, xsave` (from `rustc --print cfg -C
  target-cpu=x86-64-v3`, compared with the list the built app prints).
- Notices: 365 third-party crates (the `cargo tree -e normal` set for the two
  executables on this target), all with accepted licence expressions; 18 ship no
  licence file in their published package (accesskit ×5, lyon ×4, rspolib,
  taffy, the seven `splot-*` crates), listed in `BUILD.json`.
- Archive: `target/package/splot-studio-0.1.0-unsigned-test-placeholder-ffmpeg-linux-x86_64.tar.gz`,
  18,672,989 bytes, SHA-256
  `de1aa49e3fe000f1f81b0fc10b5d9cf31f4bbc87abb5ad9a140f7d396a0b99a5`.
  A second `cargo xtask package` run gave the same SHA-256 (sorted tar, root
  ownership, normalized modes, commit time, `gzip -n`).

```text
drwxr-xr-x        0 splot-studio-0.1.0-unsigned-test-placeholder-ffmpeg-linux-x86_64/
-rw-r--r--     4535   BUILD.json
-rw-r--r--     4791   LICENSE.md
-rw-r--r--     1724   README.txt
-rw-r--r--    25574   THIRD-PARTY-NOTICES.md
drwxr-xr-x        0   ffmpeg/
-rw-r--r--      166   ffmpeg/PLACEHOLDER.txt
-rwxr-xr-x   453104   ffmpeg/ffmpeg            (placeholder; real build adds MANIFEST.json, licenses/)
-rwxr-xr-x   216864   ffmpeg/ffprobe
drwxr-xr-x        0   licenses/
-rw-r--r--   840687   licenses/RUST-CRATES.txt
-rwxr-xr-x 34824840   splot-studio
-rwxr-xr-x 18022064   splot-studio-engine
```

## Smoke test (`cargo xtask smoke`)

Unpacked into `/tmp/splot-studio-smoke-ü-<pid>/`; every program ran with
`env_clear()` plus `PATH=/usr/bin:/bin`, a fresh `HOME` and `TMPDIR`,
`LANG=C.UTF-8`; no `SPLOT_STUDIO_SYSTEM_FFMPEG` (which `.cargo/config.toml`
otherwise sets for everything Cargo starts, xtask included), no Cargo/Rust
variables, no `LD_LIBRARY_PATH`. `/usr/bin` holds the system FFmpeg, so
"Bundled" below proves the engine used the packaged pair.

```text
ok: SHA-256 matches ….tar.gz.sha256
ok: 9 files match BUILD.json
splot-studio-engine 0.1.0
protocol version: 5
media files: FFmpeg 8.0.1-3ubuntu2 (Bundled, /tmp/splot-studio-smoke-ü-…/unpacked/…/ffmpeg/ffmpeg)
splot-studio 0.1.0
commit: d311e4a96b50cbe98b101680eff72eddbc5f40ba-dirty
protocol version: 5
engine CPU floor: x86-64-v3; checked: avx, …, xsave; this CPU has all of them
ok: MP4 with audio refused until the omission is acknowledged
ok: clip 8-bit ü.y4m -> ivf: 4 frames, 229 bytes, structure and Splot decode passed
ok: clip 10-bit ü.y4m -> obu: 4 frames, 151 bytes, structure and Splot decode passed
ok: clip ü.mp4 -> ivf: 6 frames, 1074 bytes, structure and Splot decode passed
ok: clip ü.mp4 -> obu: 6 frames, 970 bytes, structure and Splot decode passed
ok: splot-studio loads only system libraries: libfontconfig.so.1, libgcc_s.so.1, libm.so.6, libc.so.6, ld-linux-x86-64.so.2 (glibc 2.43 or newer)
ok: splot-studio-engine loads only system libraries: libgcc_s.so.1, libm.so.6, libc.so.6, ld-linux-x86-64.so.2 (glibc 2.34 or newer)
NOT CHECKED (placeholder FFmpeg): …/ffmpeg/ffmpeg: needs libavdevice.so.62, … libz.so.1
xtask: smoke test passed
```

The app `--version` ran with no `DISPLAY`/`WAYLAND_DISPLAY`. The MP4 fixture is
`packaging/smoke/tiny-h264-aac.mp4` (64×64, 6 frames H.264 + AAC; see
`packaging/smoke/README.md`).

**glibc:** the app built on this host needs 2.43 (`acosf`, `atan2f` got new
symbol versions in glibc 2.43's libm); the engine needs 2.34. Weak references
(`pidfd_spawnp@GLIBC_2.39` and others that std resolves at run time) are not
counted. A package built on the `ubuntu-24.04` runner can need at most 2.39;
its exact floor is the `platform.glibc_required` in that package's `BUILD.json`.

## x86-64-v3 engine versus portable engine

Both built from the same tree and flags except `-Ctarget-cpu`. Clip: FFmpeg
`testsrc2` 640×360, 30 fps, 30 frames, 4:2:0, Y4M; `encode … --preset 10 --crf 20`
(IVF). Times from `/usr/bin/time`.

| Run | portable wall / user | x86-64-v3 wall / user | wall speed-up |
|---|---|---|---|
| 8-bit, 1st (portable first) | 15.56 s / 23.30 s | 10.83 s / 16.76 s | 1.44× |
| 10-bit, 1st | 13.77 s / 21.67 s | 11.25 s / 17.43 s | 1.22× |
| 8-bit, 2nd (v3 first) | 14.06 s / 22.13 s | 11.11 s / 17.20 s | 1.27× |
| 10-bit, 2nd | 13.95 s / 22.08 s | 11.68 s / 18.12 s | 1.19× |

Outputs are **byte-identical** between the two builds and between runs
(`cmp`; 8-bit SHA-256 `1cb58366…2f935`, 172,246 bytes; 10-bit `a2703dfe…c8316`,
173,030 bytes). One clip per depth at one preset is not the release corpus
(see `docs/release/CHECKLIST.md`). Peak RSS 127–148 MB for both.

## Network audit

**Dependency tree** (`cargo tree -e normal --target x86_64-unknown-linux-gnu`):
the engine's 63 crates contain no networking crate (codec, rayon, serde,
tempfile, sha2, getrandom). The app's tree has no HTTP, TLS or socket crate;
it has `zbus`/`atspi`/`accesskit_unix` (accessibility over the D-Bus session
bus), `wayland-client` and `x11rb` (display server), `calloop`/`async-io`/
`polling` (event loops), and `webbrowser` + `url` from Slint's winit backend,
which hands a URL to the desktop's browser when the user activates a link (for
example in About Slint); the app itself fetches nothing.

**Runtime** (`strace -f -e trace=network`, the packaged executables, outside
Cargo, `SPLOT_STUDIO_SYSTEM_FFMPEG` unset):

- App launched on Wayland with an MP4 on the command line, 20 s (handshake,
  probe through the engine, preview process and poster), then SIGTERM:
  5 `socket()` calls, all `AF_UNIX`; `connect()` only to
  `/run/user/1000/wayland-0` (once) and `/run/user/1000/bus` (four times:
  accessibility and the portal); one `socketpair`. No `AF_INET`/`AF_INET6`/
  `AF_NETLINK`, no `bind`/`listen`.
- Packaged engine `encode` of the MP4 to OBU (ffprobe ×4, ffmpeg ×2 started):
  **zero** network system calls in any process.
- The app's real-engine worker tests (`crates/studio-app/tests/worker.rs`,
  13 tests: 27 engine and 43 FFmpeg processes, encodes included) under
  `strace -f -e trace=execve,socket,connect`: **zero** `socket()` calls.

Not covered: an encode started from the window's Convert button (no input
automation here), live pictures in the window, Windows and macOS.

## What is written to disk, and logs

- Per-user data folder (`~/.local/share/SplotStudio`, `~/Library/Application
  Support/SplotStudio`, `%LOCALAPPDATA%\SplotStudio`): `queue.json` (the list:
  absolute source and destination paths, settings, probed facts, results),
  `queue.json.tmp` while it is rewritten, `queue.json.corrupt-<n>` if an
  unreadable journal is moved aside, and `instance.lock`. Nothing else.
- Destination folder: `.<name>.<random>.partial` while a file is encoded, then
  the verified output under its final name; the partial file is removed on
  failure or cancel, and by recovery after a crash.
- No log file and no crash report anywhere. Diagnostics go to the standard
  error of whoever started the app (nothing when started from a desktop): app
  and engine error messages, which name the file concerned; FFmpeg's own error
  output is captured with a size bound and summarized into the failure message.
  Frame data never enters a message: pictures travel as raw bytes on the
  preview pipe and are dropped after display.
- Packages carry no builder paths: `--remap-path-prefix` in the executables,
  and `BUILD.json` records a `--ffmpeg-dir` only as "a directory given with
  --ffmpeg-dir".

## Other checks

- `cargo xtask ci` after these changes: pass (fmt, SPDX headers on 81 files,
  dependency direction, clippy `-D warnings`, 309 tests with one ignored as
  before; app 136 including the two CPU-check tests, xtask 13).
- `cargo clippy --workspace --all-targets --locked --target x86_64-pc-windows-msvc
  -- -D warnings` and the same for `aarch64-apple-darwin`: clean (check only;
  nothing was linked or run for those targets).
