# FFmpeg for Splot Studio

The engine reads ordinary media files (MP4, MOV, Matroska/WebM, MPEG-TS, AVI)
by running the FFmpeg command-line tools `ffprobe` and `ffmpeg` as child
processes (owner decision, 2026-11-20: no FFmpeg libraries are linked, no FFI,
no `studio-media` crate). Decoded frames reach the engine as a Y4M stream on a
pipe. See `crates/studio-engine/src/media/`.

## Status: no release build exists yet

There is no Studio FFmpeg build. Producing, verifying and shipping one is a
release blocker for STUDIO-06. No version, source hash or configuration is
chosen here yet, and none may be assumed.

## What the release must ship

A controlled build made by this project from FFmpeg source:

- LGPL only: configured without `--enable-gpl` and without `--enable-nonfree`,
  with every external library listed and licence-reviewed.
- Both executables, `ffmpeg` and `ffprobe`, built from the same source and
  configuration (the engine refuses a pair that reports different versions).
- Demuxers: `mov` (MP4, MOV, M4V, 3GP), `matroska` (MKV, WebM), `mpegts`, `avi`.
  The engine allows only these, through `-format_whitelist mov,matroska,mpegts,avi`.
- Protocols: `file` (input) and `pipe` (output). Input is restricted with
  `-protocol_whitelist file`; network protocols are not needed.
- Decoders for the video formats the release claims, and nothing is claimed
  without a test. Tested today (with a development build): H.264, VP9, FFV1,
  ProRes, raw video (including `nv12`).
- Muxer `yuv4mpegpipe` with encoder `wrapped_avframe`.
- `libavfilter` and `libswscale` for the one pixel operation used: repacking
  semi-planar `nv12`/`p010le` to planar samples at the same depth.
- `ffprobe` writers `json` and `compact`.

Decoder patent licensing is a separate review and is not settled by the LGPL
configuration.

## Where the engine looks

Only these places, in this order, and only a directory holding both tools:

1. The directory of the engine executable (`ffmpeg`, `ffprobe`, with `.exe` on
   Windows), then its `ffmpeg/` subdirectory: reported as **bundled**.
2. Only if the environment variable `SPLOT_STUDIO_SYSTEM_FFMPEG=1` is set: each
   absolute directory on `PATH`, first match wins: reported as **system**.

Nothing else is searched. The identity is the first line of `ffmpeg -version`,
read once per engine process; the app sees it in the handshake
(`Capabilities.media_tool`). Without a usable pair, non-Y4M sources fail with
`MediaToolMissing` and Y4M still works.

## Development builds

`.cargo/config.toml` sets `SPLOT_STUDIO_SYSTEM_FFMPEG=1` for commands run
through Cargo, so tests and `cargo run` use the FFmpeg installed on the
developer's machine or CI runner (apt, Homebrew, Chocolatey). Those are
development tools only and are never distributed. Ubuntu's `ffmpeg`, for
example, is configured with `--enable-gpl`. `cargo xtask doctor` reports the
version found and the encoders the test fixtures need.
