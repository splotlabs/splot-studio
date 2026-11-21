# FFmpeg for Splot Studio

The engine reads ordinary media files (MP4, MOV, Matroska/WebM, MPEG-TS, AVI)
by running the FFmpeg command-line tools `ffprobe` and `ffmpeg` as child
processes (owner decision, 2026-11-20: no FFmpeg libraries are linked, no FFI,
no `studio-media` crate). Decoded frames reach the engine as a Y4M stream on a
pipe. See `crates/studio-engine/src/media/`.

The release ships a pair built here from source by `build.sh`.

## Building

```sh
native/ffmpeg/build.sh linux-x86_64     # on Linux x86_64
native/ffmpeg/build.sh windows-x86_64   # in an MSYS2 UCRT64 shell on Windows x86_64
native/ffmpeg/build.sh macos-arm64      # on macOS arm64
```

Each target is built natively on its own OS; nothing is cross-compiled. The
script downloads the archives pinned in `versions.env` (one URL and SHA-256 per
archive) into `work/downloads/`, refuses any other hash, builds dav1d and then
FFmpeg under `work/<target>/`, checks the result, and writes `out/<target>/`:

- `ffmpeg[.exe]`, `ffprobe[.exe]`
- `MANIFEST.json`: versions, URLs and SHA-256 of the sources, the configure and
  meson lines, toolchain versions, the `-version` line, `-buildconf`, the
  components actually built (decoders, demuxers, muxers, encoders, protocols,
  filters, bitstream filters), and per executable its SHA-256, size, the shared
  libraries it needs and its OS floor.
- `licenses/`: FFmpeg's `LICENSE.md`, `COPYING.LGPLv2.1` and `CREDITS`, dav1d's
  `COPYING`, and `SOURCE.md` (where the exact source is, how to rebuild and
  replace the pair, and the IJG credit FFmpeg's licence asks for).

`work/` and `out/` are ignored by Git; sources and binaries are never committed.

Tools the script needs (beyond a shell, `curl`, `tar` with xz and `sha256sum`
or `shasum`):

| Target | Environment | Install first |
|---|---|---|
| linux-x86_64 | Ubuntu (GitHub `ubuntu-24.04`) | `apt-get install nasm meson ninja-build pkg-config` (gcc, make, binutils, curl, xz-utils are on the runner) |
| windows-x86_64 | MSYS2 UCRT64 (GitHub `windows-2025` with `msys2/setup-msys2`, `msystem: UCRT64`) | `make diffutils tar xz curl git mingw-w64-ucrt-x86_64-gcc mingw-w64-ucrt-x86_64-nasm mingw-w64-ucrt-x86_64-meson mingw-w64-ucrt-x86_64-ninja mingw-w64-ucrt-x86_64-pkgconf` |
| macos-arm64 | Xcode command-line tools (GitHub `macos-15`) | `brew install meson ninja pkgconf` |

On macOS the executables target `MACOSX_DEPLOYMENT_TARGET`, 12.0 unless the
caller sets it. On Linux the glibc floor is that of the build host (the
manifest records the newest GLIBC symbol version used), so build on the oldest
distribution the release supports.

## Configuration

LGPL 2.1 or later only: no `--enable-gpl`, `--enable-nonfree` or
`--enable-version3`; the script fails if `-buildconf` shows any of them or
`ffmpeg -L` does not report LGPL 2.1. `--disable-everything` and
`--disable-autodetect` mean nothing is built unless listed below (plus what
those components select, for example the ffmpeg tool's own null, format, trim,
crop and flip filters, the h263 decoder under mpeg4, and VP9's superframe
splitter), and no system library is picked up by accident.

| Component | Enabled | Why |
|---|---|---|
| Demuxers | `mov`, `matroska`, `mpegts`, `avi` | `-format_whitelist mov,matroska,mpegts,avi` |
| Protocols | `file`, `pipe` | `-protocol_whitelist file` input, `pipe:1` output |
| Decoders | `h264`, `hevc`, `vp8`, `vp9`, `mpeg1video`, `mpeg2video`, `mpeg4`, `prores`, `dnxhd`, `ffv1`, `mjpeg`, `rawvideo`, `libdav1d` (AV1) | the formats the release reads |
| Parsers | `h264`, `hevc`, `vp8`, `vp9`, `mpegvideo`, `mpeg4video`, `av1`, `dnxhd`, `mjpeg`, `ffv1` | packet splitting for those decoders (MPEG-TS needs them) |
| Muxer, encoder | `yuv4mpegpipe`, `wrapped_avframe` | `-f yuv4mpegpipe pipe:1` |
| Filters | `select`, `scale`, `format`, `null` | `select=gte(n,…)`/`gte(t,…)` for pictures; `-pix_fmt` repack of `nv12`/`p010le` |
| Libraries | avutil, avcodec, avformat, avfilter, swscale | avdevice and swresample are disabled |
| External | dav1d only | AV1 decoding (BSD 2-clause) |

Disabled as well: network, devices, hardware acceleration, ffplay,
documentation, and every autodetected library (zlib, bzip2, lzma, iconv, TLS,
SDL, Xlib, VAAPI, VideoToolbox, D3D, …). Threads are named explicitly
(`--enable-pthreads`, Windows `--enable-w32threads`) because autodetection is
off. dav1d is built static, release, with assembly, for 8-bit and high bit
depth, without tools, examples, tests or docs. FFmpeg and dav1d choose their
SIMD at run time; no `-march` is set, so x86-64 builds need only the x86-64
baseline. Linux uses FFmpeg's `--toolchain=hardened` (fortify, stack protector,
PIE, full RELRO).

The version reads `8.0.3-splot-studio` (`--extra-version`), so the bundled
build is distinguishable from a system FFmpeg of the same release.

## Linking

The engine starts the tools directly, so they must not need libraries that
are not part of the OS. FFmpeg's libraries and dav1d are linked statically into
each executable (owner decision, 2026-11-21) rather than shipped as shared
libraries.
A user replaces the LGPL components by replacing the executables, which are
separate programs; `licenses/SOURCE.md` says how. The script fails if an
executable needs anything outside:

- Linux: `libc.so.6`, `libm.so.6`, `libpthread.so.0`, `libdl.so.2` (glibc).
- Windows: `-static` puts libgcc and winpthread into the executable; allowed
  are `KERNEL32`, `ADVAPI32`, `BCRYPT`, `SHELL32`, `OLE32`, `PSAPI`, `USER32` and
  the `api-ms-win-crt-*`/`api-ms-win-core-*` (UCRT) DLLs.
- macOS: anything under `/usr/lib/` or `/System/Library/` (expected:
  `/usr/lib/libSystem.B.dylib` only).

It also starts both with an empty environment (no `PATH`) and requires both to
report `ffmpeg|ffprobe version 8.0.3-splot-studio`.

## Where the engine looks

Only these places, in this order, and only a directory holding both tools:

1. The directory of the engine executable (`ffmpeg`, `ffprobe`, with `.exe` on
   Windows), then its `ffmpeg/` subdirectory: reported as **bundled**.
2. Only if the environment variable `SPLOT_STUDIO_SYSTEM_FFMPEG=1` is set: each
   absolute directory on `PATH`, first match wins: reported as **system**.

Nothing else is searched. The identity is the first line of `ffmpeg -version`,
read once per engine process; the app sees it in the handshake
(`Capabilities.media_tool`). Without a usable pair, non-Y4M sources fail with
`MediaToolMissing` and Y4M still works. A pair that reports different versions
is refused.

## What this build does not read

Measured differences from a full development FFmpeg:

- Files of any other kind (playlists, concat lists, images, raw `.h264`, `.mpg`,
  …) are not recognised at all, because their demuxers are not built. `ffprobe`
  then names no probed format, and the engine refuses the file as "not a file
  Studio reads, or not a media file" (UnsupportedSource), where a full build
  gives the format whitelist refusal. See `crates/studio-engine/src/media/probe.rs`.
- Video in codecs not listed (Theora, VC-1, …) is refused as "this FFmpeg build
  cannot decode".
- PNG cover art is listed with size 0×0, meaning unknown (no PNG decoder; it
  needs zlib). Attached pictures are never encoded and the app does not show
  their size.
- MPEG-1 video decodes (bit-exact in Matroska), but MPEG-1 in MPEG-TS is
  refused as variable frame rate with any FFmpeg: `ffprobe` states twice the
  real rate there (`r_frame_rate` 50/1 for 25 fps), an engine-side limit.
- Matroska tracks with zlib or bzip2 content compression and compressed MOV
  headers (`cmov`) cannot be read (zlib and bzip2 are not linked).

## Development builds

`.cargo/config.toml` sets `SPLOT_STUDIO_SYSTEM_FFMPEG=1` for commands run
through Cargo, so tests and `cargo run` use the FFmpeg installed on the
developer's machine or CI runner (apt, Homebrew, Chocolatey). Those are
development tools only and are never distributed. Ubuntu's `ffmpeg`, for
example, is configured with `--enable-gpl`. `cargo xtask doctor` reports the
version found and the encoders the test fixtures need. The engine tests make
their fixtures with that FFmpeg's encoders, which this build does not have.

## Updating

Change `versions.env` (version, URL and SHA-256 together) after checking the
upstream signature: FFmpeg's `.asc` with the FFmpeg release signing key, dav1d's
`.asc` with the VideoLAN release signing key. Then rebuild every target and
run `cargo xtask package --smoke` on each.

Decoder patent licensing is a separate review and is not settled by the LGPL
configuration.
