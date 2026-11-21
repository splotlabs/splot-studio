# STUDIO-06 FFmpeg: the shipped build

Measured 2026-11-21. Host: Intel i7-12700H (20 threads), Ubuntu 26.04.1,
Linux 7.0.0-44, gcc 15.2.0, GNU Make 4.4.1, NASM 3.01, meson 1.10.1, ninja
1.13.2, pkgconf 2.5.1, glibc 2.43. Engine: `cargo build --release -p
studio-engine` at `d311e4a` plus the uncommitted STUDIO-06 tree of that day,
Rust 1.100.0. Another agent was building in the same tree at the same time.

Only `linux-x86_64` was built and run. **`windows-x86_64` and `macos-arm64`
are written in `native/ffmpeg/build.sh` but have never run; they are
unverified until CI runs them on `windows-2025` and `macos-15`.**

## Versions and how they were checked

| Archive | Version | SHA-256 |
|---|---|---|
| `https://ffmpeg.org/releases/ffmpeg-8.0.3.tar.xz` | FFmpeg 8.0.3 "Huffman" | `6136812ea6d4e68bdba27e33c2a94382711cdf4f8602ffef056ff792bd6f9818` |
| `https://downloads.videolan.org/pub/videolan/dav1d/1.5.5/dav1d-1.5.5.tar.xz` | dav1d 1.5.5 "Sonic" | `ca96b1d0ba3f01f55de7231d775892b64994ba061aaeeaaa4f662a390b3abf9e` |

- FFmpeg: the `https://ffmpeg.org/releases/` index lists 8.0, 8.0.1, 8.0.2
  and 8.0.3 (2026-06-18) in the 8.0 branch, and `download.html` names
  "FFmpeg 8.0.3 Huffman" as the 8.0 release (8.1.3 is the newest release
  overall; the owner chose 8.0.x). `gpg --verify ffmpeg-8.0.3.tar.xz.asc`:
  "Good signature from FFmpeg release signing key", key
  `FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8`, imported from
  `https://ffmpeg.org/ffmpeg-devel.asc`; the same fingerprint is printed on
  `https://ffmpeg.org/download.html`.
- dav1d: `https://downloads.videolan.org/pub/videolan/dav1d/` lists up to
  1.5.5 (17 Nov 2026). The published `dav1d-1.5.5.tar.xz.sha256` matches.
  `gpg --verify dav1d-1.5.5.tar.xz.asc`: "Good signature from VideoLAN Release
  Signing Key", key `65F7 C6B4 206B D057 A7EB 7378 7180 713B E58D 1ADC`,
  fetched from `https://download.videolan.org/pub/keys/7180713BE58D1ADC.asc`.
  Its NEWS lists x86/AArch64 optimisations and fixes, including an
  out-of-bounds read in high-bit-depth SGR AVX-512 assembly. It is four days
  old.
- The keys were checked against the fingerprints the projects publish, not
  against a web of trust (gpg: "not certified with a trusted signature").

## Configuration

FFmpeg (`ffmpeg -buildconf`, Linux):

```text
--cc=gcc --extra-version=splot-studio --disable-everything --disable-autodetect
--disable-network --disable-doc --disable-ffplay --disable-avdevice
--disable-swresample --enable-static --disable-shared --pkg-config-flags=--static
--enable-libdav1d
--enable-decoder='h264,hevc,vp8,vp9,mpeg1video,mpeg2video,mpeg4,prores,dnxhd,ffv1,mjpeg,rawvideo,libdav1d'
--enable-parser='h264,hevc,vp8,vp9,mpegvideo,mpeg4video,av1,dnxhd,mjpeg,ffv1'
--enable-demuxer='mov,matroska,mpegts,avi' --enable-muxer=yuv4mpegpipe
--enable-encoder=wrapped_avframe --enable-protocol='file,pipe'
--enable-filter='select,scale,format,null'
--enable-pthreads --toolchain=hardened
```

Windows replaces the last line with `--enable-w32threads
--extra-ldexeflags=-static`, macOS with `--enable-pthreads`.
`PKG_CONFIG_LIBDIR` points only at the dav1d just built.

dav1d (meson):

```text
--prefix=<work>/prefix --libdir=lib --buildtype=release --default-library=static
-Dbitdepths=8,16 -Denable_asm=true -Denable_tools=false -Denable_examples=false
-Denable_tests=false -Denable_docs=false
```

Every component actually built, from the built `ffmpeg` (`MANIFEST.json`):

| Kind | Built |
|---|---|
| Decoders | libdav1d, dnxhd, ffv1, h263 (selected by mpeg4), h264, hevc, mjpeg, mpeg1video, mpeg2video, mpeg4, prores, rawvideo, vp8, vp9 |
| Encoders | wrapped_avframe |
| Demuxers | avi, matroska/webm, mov/mp4/m4a/3gp/3g2/mj2, mpegts |
| Muxers | yuv4mpegpipe |
| Protocols | file, pipe (input and output) |
| Filters | select, scale, format, null; selected by the ffmpeg tool: aformat, anull, atrim, crop, hflip, rotate, transpose, trim, vflip, abuffer, buffer, abuffersink, buffersink |
| Bitstream filters | vp9_superframe_split (selected by vp9) |
| Parsers | as configured (no listing option exists) |
| Hardware acceleration, devices | none (`-hwaccels`, `-devices` list nothing) |

## Local build and checks (linux-x86_64)

`native/ffmpeg/build.sh linux-x86_64`: 2 min 20 s wall on 20 threads, exit 0.
`shellcheck build.sh` and `shellcheck -x build.sh`: clean (shellcheck on this
host). The script's own checks all passed; by hand:

```text
$ env -i ./ffmpeg -version | head -1
ffmpeg version 8.0.3-splot-studio Copyright (c) 2000-2026 the FFmpeg developers
$ env -i ./ffprobe -version | head -1
ffprobe version 8.0.3-splot-studio Copyright (c) 2007-2026 the FFmpeg developers
$ ./ffmpeg -hide_banner -buildconf | grep -cE -- '--enable-(gpl|nonfree|version3)'
0
$ ./ffmpeg -hide_banner -L
... GNU Lesser General Public License ... version 2.1 of the License, or
(at your option) any later version.
$ ldd ffmpeg     # ffprobe identical
    linux-vdso.so.1
    libm.so.6 => /usr/lib/x86_64-linux-gnu/libm.so.6
    libc.so.6 => /usr/lib/x86_64-linux-gnu/libc.so.6
    /lib64/ld-linux-x86-64.so.2
$ readelf -n ffmpeg | grep ISA
    x86 ISA needed: x86-64-baseline
$ readelf -d ffmpeg | grep FLAGS
    (FLAGS) BIND_NOW   (FLAGS_1) Flags: NOW PIE
```

No `-march` in `CFLAGS` (FFmpeg `ffbuild/config.mak`) or in the dav1d build.
The newest glibc symbols used are `atan2f@GLIBC_2.43` and `hypot@GLIBC_2.35`:
that is this host's glibc, so this local build runs only on glibc 2.43+ and
is evidence, not a release artefact. A CI build on `ubuntu-24.04` gets
glibc 2.39's versions.

| Executable | Bytes | SHA-256 |
|---|---|---|
| `ffmpeg` | 11 468 744 | `240b30b079c1ba1f00ddb8b229d2c0f49581a36a4ca690374ba9b894a5895b86` |
| `ffprobe` | 11 216 104 | `836b77c5d30c43c8d24d97182a40cbaee9dd51d60bc62e995ae56026296f5ccc` |

These are the build with `mpeg1video` (owner decision after the first build;
sizes unchanged, section alignment). The first configuration, without it, gave
`6cdf3892…dc4dc` / `5f5e623c…accc9` three times from clean runs of the script
(it deletes `work/<target>` and `out/<target>` first) in the same directory
with the same toolchain: the same SHA-256 each time. The compile log has upstream compiler warnings only
(`-Wformat-truncation`, `-Warray-bounds`, NASM deprecations), no errors.

## The engine on the built pair

The pair copied beside a copy of the release engine binary, as
`tests/worker.rs::tools_beside_the_engine_are_bundled` does, and every engine
run started with `env -i` (so `SPLOT_STUDIO_SYSTEM_FFMPEG` unset, no `PATH`):

```text
$ env -i <dir>/engine --version
media files: FFmpeg 8.0.3-splot-studio (Bundled, <dir>/ffmpeg)
```

Fixtures: made by the system FFmpeg 8.0.1 (Ubuntu, development only) from
`testsrc2`, 320×240, 25 fps, 10 frames, in a temporary directory. For each:
`engine encode <file> <out.ivf> --preset 10 --crf 40` (probe, full-stream
scan, decode, AV2 encode, the engine's own output checks: Structure and
SplotDecode passed for every completed job; AVM not integrated). Then the
engine's exact decode command (`-protocol_whitelist file -format_whitelist
mov,matroska,mpegts,avi … -fps_mode passthrough -noautoscale [-pix_fmt]
-strict -1 -f yuv4mpegpipe pipe:1`) on the built `ffmpeg`, its frame payloads
hashed, against the system FFmpeg's `-f rawvideo` decode of the same stream at
the same planar format.

| File | Stream | Engine | Decode vs system FFmpeg |
|---|---|---|---|
| `h264-8.mp4` | h264 yuv420p | completed, 10 frames | bit-exact |
| `h264-aac.mp4` (+AAC, acknowledged) | h264 yuv420p | completed; audio listed and left out (no audio decoder needed) | bit-exact |
| `h264-10.mkv` | h264 yuv420p10le | completed | bit-exact |
| `hevc-8.mp4` | hevc yuv420p | completed | bit-exact |
| `hevc-10.mp4` | hevc yuv420p10le | completed | bit-exact |
| `hevc-10.ts` | hevc yuv420p10le | completed | bit-exact |
| `vp8-8.webm` | vp8 yuv420p | completed | bit-exact |
| `vp9-8.webm` | vp9 yuv420p | completed | bit-exact |
| `vp9-10.webm` | vp9 yuv420p10le (profile 2) | completed | bit-exact |
| `av1-8.mkv` (SVT-AV1) | av1 yuv420p, libdav1d | completed | bit-exact |
| `av1-10.mp4` (SVT-AV1) | av1 yuv420p10le, libdav1d | completed | bit-exact |
| `mpeg2-8.ts` | mpeg2video yuv420p | completed | bit-exact |
| `mpeg1-25.mkv` | mpeg1video yuv420p | completed | bit-exact |
| `mpeg1.ts` (25 fps), `mpeg1-24.ts`, `mpeg1-30000_1001.ts` | mpeg1video yuv420p | refused: variable frame rate (`r_frame_rate` reads 50/1, 48/1, 60000/1001), the same with the system FFmpeg | – |
| `mpeg1-25.avi` | mpeg1video yuv420p | refused: frame 1 has no timestamp, the same with the system FFmpeg | – |
| `mpeg4-8.avi` | mpeg4 yuv420p | completed | bit-exact |
| `prores422-10.mov` | prores yuv422p10le | completed | bit-exact |
| `dnxhr-8.mov` (DNxHR HQ) | dnxhd yuv422p | completed | bit-exact |
| `dnxhr-10.mov` (DNxHR HQX) | dnxhd yuv422p10le | completed | bit-exact |
| `ffv1-8.mkv` | ffv1 yuv420p | completed | bit-exact (and equal to the raw NV12 clip's repack: both lossless) |
| `ffv1-10.mkv` | ffv1 yuv420p10le | completed | bit-exact |
| `raw-nv12.avi` | rawvideo nv12, repacked | completed, `LayoutRepacked` | bit-exact |
| `av1-12.mkv` (libaom) | av1 yuv420p12le | refused: pixel format yuv420p12le not supported | – |
| `prores4444-10.mov` | prores yuv444p12le (ProRes 4444 decodes to 12 bits) | refused: pixel format | – |
| `mjpeg.avi` | mjpeg yuvj420p | refused: full-range (JPEG) samples | – |

The AV1 comparison is between two dav1d versions (1.5.5 here, Ubuntu's in the
system FFmpeg); the others compare FFmpeg 8.0.3's native decoders with
8.0.1's. A 10-bit raw clip could not be made: FFmpeg's AVI and Matroska
muxers do not store `p010le`/`yuv420p10le` raw video (Matroska VFW mode
turned it into `rgb555le`, which the engine refused as RGB).

The table was run again with the final build and engine: 19 completed and
bit-exact, the same three refusals.

Errors, the same files through the bundled pair and through the system FFmpeg
(opt-in), with the engine's refusal rule below:

| File | Bundled pair | System FFmpeg 8.0.1 |
|---|---|---|
| `list.m3u8` (HLS playlist) | UnsupportedSource: "this is not a file Studio reads, or not a media file: Studio reads MP4, …" | UnsupportedSource: "this kind of file is not read: Studio reads MP4, …" (whitelist) |
| `list.ffconcat`, `still.png`, `raw.h264`, `notes.dat` (text) | UnsupportedSource, not a file Studio reads | UnsupportedSource, whitelist |
| `notes.bin` (text) | UnsupportedSource, not a file Studio reads | UnsupportedSource, not a file Studio reads (no format named) |
| `notes.mp4` (text; mov takes it by extension) | InvalidSource: "moov atom not found" | same |
| `h36.bin` (Matroska EBML header cut at 36 bytes; taken by content, score 100) | InvalidSource: "EBML header parsing failed" | same |
| `theora.mkv` | UnsupportedSource: "this FFmpeg build cannot decode the theora video" | (decodes) |
| `cenc.mp4` | UnsupportedSource: encrypted | same |
| truncated MKV, truncated MP4, CRC-damaged FFV1, empty | InvalidSource with the decoder's/demuxer's words | same |

Before that rule (first run), the bundled pair gave InvalidSource "Invalid
data found when processing input" for the first group: their demuxers are not
built, so nothing reaches the whitelist check. Nothing is read in either case.

### The refusal rule (engine, `media/probe.rs`)

When the probing `ffprobe` fails:

1. Its diagnostics (with the file's own URL removed, so a file name cannot
   pose as a message) contain "whitelist": a built demuxer recognised a format
   Studio does not read. UnsupportedSource, "this kind of file is not read".
2. They contain "Invalid data found when processing input" and a second
   `ffprobe` of the file at `-v debug` names no probed format: no demuxer
   that is built claimed the file. UnsupportedSource, "this is not a file
   Studio reads, or not a media file". `libavformat` always names the format
   it picks before the demuxer reads anything: "Format X probed with size=…"
   (debug) or "Format X detected only with low score of N" (warning)
   (`av_probe_input_buffer2`, `libavformat/format.c`). If that second run
   cannot run, the file counts as claimed.
3. Otherwise InvalidSource, as before: a whitelisted demuxer took the file
   (by content or by extension) and could not read it.

The second run happens only after a failure.

### The engine's own tests on the built pair

The tests make fixtures with the `ffmpeg` on `PATH`, which needs encoders this
build lacks, so they cannot simply run with the pair first on `PATH`. Without
editing the crates: a directory first on `PATH` holding the built `ffprobe` and
an `ffmpeg` wrapper (test scaffolding only, in a temporary directory) that
runs the built `ffmpeg` for every engine invocation (they all carry
`-protocol_whitelist`, and `-version`) and the system `ffmpeg` otherwise
(fixtures and the tests' reference decodes). `PATH=<dir>:$PATH
SPLOT_STUDIO_SYSTEM_FFMPEG=1 cargo test -p studio-engine --no-fail-fast`:

| Binary | Result |
|---|---|
First run (first build, engine unchanged): unit 38, job 15, live 6, media
13 of 15, preview 14, seek 1, worker 27 passed. The two media failures were
the PNG cover art listed as 0×0 instead of 8×8 (no PNG decoder) and the
playlist refused as InvalidSource.

After the refusal rule and the test changes (final build), with the bundled
pair (wrapper) and with the system FFmpeg 8.0.1 (`cargo test -p studio-engine
--locked --no-fail-fast`), the same result for both:

| Binary | Bundled pair | System FFmpeg |
|---|---|---|
| unit tests | 39 passed | 39 passed |
| `tests/job.rs` | 15 passed | 15 passed |
| `tests/live.rs` | 6 passed | 6 passed |
| `tests/media.rs` | 16 passed | 16 passed |
| `tests/preview.rs` | 13 passed, **1 failed** | 13 passed, **1 failed** |
| `tests/seek.rs` | 1 passed | 1 passed |
| `tests/worker.rs` | 27 passed | 27 passed |

- `tests/media.rs`: the new `unknown_kinds_are_refused_and_damaged_headers_are_not`
  (text as `notes.bin`: UnsupportedSource; a cut EBML header: InvalidSource);
  `refused_media` unchanged in its cases; the cover-art assertion accepts 8×8
  or 0×0 (unknown) for the attached picture only.
- `tests/preview.rs::refusals_name_their_reason` fails with both: it expects
  `notes.txt` holding "hello" to be `Unreadable`, and the rule now makes it
  `Unsupported` (no FFmpeg names a format for it). The test, outside this
  change's files, needs that expectation updated.

## Not verified

- `windows-x86_64` and `macos-arm64` have not been built or run. Specific
  assumptions to confirm in CI: MSYS2 converts the POSIX paths in meson's
  `--prefix` and in `PKG_CONFIG_LIBDIR` for the native tools; the UCRT64
  executables import only the allowlisted system DLLs (the list is a reasoned
  guess; the script fails and names any other); `env -i SYSTEMROOT=…` starts a
  native executable; on macOS nothing but `/usr/lib/libSystem.B.dylib` is
  linked and `minos` is 12.0.
- Byte-for-byte reproducibility across hosts or paths was not checked (only the
  same path and toolchain, above; assertion messages embed source paths).
- No run on another Linux distribution or an older glibc; no run on a CPU
  without AVX2 (the ISA note says baseline; runtime dispatch was not exercised
  on such a CPU).
- Seeking/preview on MPEG-TS, VP8, DNxHR, ProRes through the bundled pair only
  as far as the engine tests cover them (they use their own fixtures).

## Licences in this configuration

- FFmpeg 8.0.3 as configured: LGPL 2.1 or later as a whole (`ffmpeg -L`, no
  GPL, nonfree or version-3 parts). Within it, files under permissive terms
  listed in its `LICENSE.md`; of those, the libjpeg-derived
  `libavcodec/jrevdct.c`, `jfdctint.c` (`jfdctint_template.c`) and
  `jfdctfst.c` are compiled in (`libavcodec/*.o` present), which obliges a
  binary distribution to credit the IJG. `licenses/SOURCE.md` carries that
  credit.
- dav1d 1.5.5: BSD 2-clause (copyright notice and licence travel with the
  binaries in `licenses/dav1d/COPYING`).
- Linked from the OS only: glibc (Linux), the UCRT and Windows system DLLs,
  libSystem (macOS).
- LGPL does not settle patent licensing. H.264, HEVC, MPEG-2, MPEG-4 part 2,
  VP8, VP9, AV1, ProRes and DNxHD decoding each have their own patent
  situation; that is an owner/legal review, not something this build answers.
