# Splot Studio 0.1.0 — unsigned test build (draft)

**Status: draft for a test build, not a release.** Nothing has been published.
The packages are not code-signed or notarized and have not been installed on a
clean machine. What has and has not been run is in `STATUS.md` and
`docs/evidence/studio-06/package.md`; what remains before a real release is in
[`CHECKLIST.md`](CHECKLIST.md).

## What it does

Splot Studio converts local video files to **AV2**, the successor of AV1, with the
[Splot](https://github.com/splotlabs/splot) encoder. Processing is local; the app opens
no network connection.

- **Output is video only:** AV2 in an IVF file or an Annex B length-delimited `.obu`
  stream. Audio, subtitles, attachments, chapters and extra video tracks of a source
  are **not carried over**. The app lists what will be left out and asks once before a
  file with such tracks is converted; nothing is dropped silently.
- **Input:** Y4M, and MP4/MOV, Matroska/WebM, MPEG-TS and AVI decoded by the bundled
  FFmpeg (video decoders: H.264, HEVC, VP8, VP9, AV1 through dav1d, MPEG-2, MPEG-4
  Part 2, ProRes, DNxHD, MJPEG, FFV1 and raw video, as configured in
  `native/ffmpeg/README.md`; only H.264, VP9, FFV1, ProRes and raw video are covered
  by tests, and those with a development FFmpeg, not the bundled build).
- **Accepted video:** progressive, constant frame rate, SDR, limited range, square
  pixels, no rotation; 8-bit and 10-bit planar 4:2:0, 4:2:2, 4:4:4 and gray (plus
  `nv12`/`p010le`, repacked without changing a value). Every frame is checked before
  anything is written.
- **Refused, with a reason:** variable frame rate, interlaced, HDR (PQ, HLG, Dolby
  Vision, HDR side data), full range, non-square pixels, rotation/cropping/stereo/
  spherical side data, RGB, alpha, other bit depths, mid-stream changes of size,
  format or colour, encrypted streams, network and playlist inputs.
- A list of files that survives restarts, one encode at a time, cancel, a preview of
  the real encoded picture, and verification of every output before it appears at its
  destination (it never replaces an existing file).

## System requirements

| | Linux x86_64 | Windows x86_64 | macOS arm64 |
|---|---|---|---|
| Processor | **x86-64-v3** (AVX2, FMA, BMI1/2, F16C, LZCNT, MOVBE: Intel Haswell 2013 / AMD Excavator 2015 and newer) | **x86-64-v3**, as Linux | Apple silicon (M1 and newer) |
| OS | glibc 2.39 or newer (Ubuntu 24.04, Debian 13, Fedora 40 or newer), with fontconfig and a Wayland or X11 desktop | Windows 10 or 11, 64-bit | macOS 15 or newer |
| Built on | GitHub `ubuntu-24.04` | GitHub `windows-2025` | GitHub `macos-15` |

The exact glibc floor of a package is the `platform.glibc_required` value in its
`BUILD.json`, measured from the shipped executables (the numbers above are the build
runner's glibc; the measured value can only be lower or equal). The macOS floor is
declared in the bundle (`LSMinimumSystemVersion`) and checked against every
executable's `minos`.

On an x86-64 processor without x86-64-v3 the app shows "This processor is not
supported", names the missing features and exits; it never starts the engine there.
**Starting `splot-studio-engine` directly on such a processor crashes with an illegal
instruction**: the engine is compiled for x86-64-v3 and cannot check first. The app
itself is compiled for plain x86-64.

A 64-bit Intel Mac (x86_64 macOS), 32-bit systems, Windows on Arm and Linux on Arm
have no package.

## Unsigned: what the OS will say

- **macOS:** the app has only an ad-hoc signature (needed for Apple silicon to run it
  at all), no Developer ID and no notarization. After downloading, macOS says it
  cannot verify the app. To open it anyway: try to open it once, then System Settings
  → Privacy & Security → "Open Anyway". Only do this for a package whose SHA-256 you
  have checked.
- **Windows:** SmartScreen shows "Windows protected your PC" for an unknown publisher.
  "More info" → "Run anyway". Some antivirus products may flag unsigned executables.
- **Linux:** no warning; mark the files executable if your unpacker did not
  (`tar` keeps the modes).

Check the download first: each archive has a `.sha256` file
(`sha256sum -c <file>.sha256` on Linux, `shasum -a 256` on macOS,
`Get-FileHash -Algorithm SHA256` on Windows).

## Install, update, uninstall (by hand)

- **Install:** unpack the archive anywhere you can write to. Linux and Windows: keep
  the folder together and start `splot-studio` (`splot-studio.exe`). macOS: move
  `Splot Studio.app` where you like, for example Applications.
- **Update:** quit the app and replace the folder (or the `.app`) with the new one.
  Your list of files and settings stay where they are.
- **Uninstall:** quit the app and delete the folder or the `.app`. The list of files
  and settings are kept in a per-user folder; delete it too to remove everything:
  Linux `~/.local/share/SplotStudio`, macOS `~/Library/Application Support/SplotStudio`,
  Windows `%LOCALAPPDATA%\SplotStudio`. Your videos and converted files are never
  touched.

There is no installer, no updater and no background service.

## Known limitations (from `STATUS.md`)

- Encoding is slow: about 0.8 frames per second at 720p on the fastest priority, on a
  20-thread laptop processor, using under two cores. Quality and Priority mappings are
  provisional and uncalibrated.
- The encoder writes no colour description into the output, so players must guess
  how to show it (splotlabs/splot#1777); the app says so per source.
- No independent AV2 decoder check yet: outputs are verified for structure and by the
  Splot decoder, not by AVM.
- Closing the window stops the current file; nothing runs in the background and
  nothing resumes by itself after a restart (Continue runs the rest).
- No drag and drop from the file manager; no audio, subtitles, resize or target size.
- Windows and macOS have been built and tested only on CI runners; crash recovery is
  unproven on Windows. Screen readers, keyboard-only use of the list, disk-full and
  removable-drive loss are untested by hand.
- The preview is an SDR approximation, not a reference display.

## Licences and source

- Splot Studio itself: PolyForm Noncommercial License 1.0.0 (`LICENSE.md`);
  commercial use needs a separate written licence from Bartosz Tomczyk.
  Source-available, not open source.
- Third-party Rust crates: listed with their licences in `THIRD-PARTY-NOTICES.md`
  inside the package (generated for exactly what is linked); their licence texts are
  in `licenses/RUST-CRATES.txt`.
- Slint is used under the Slint Royalty-free License 2.0; its attribution is the
  "About Slint" panel in Splot Studio's About screen.
- The Inter font embedded by Slint: SIL Open Font License 1.1.
- **FFmpeg** (`ffmpeg`, `ffprobe`) is FFmpeg 8.0.3, LGPL 2.1 or later, with dav1d 1.5.5
  (BSD 2-clause), built from the unmodified upstream release archives by
  `native/ffmpeg/build.sh` in the Splot Studio source repository. The package carries
  its `MANIFEST.json` (source URLs and SHA-256, configure line, toolchain) and its
  licences under `ffmpeg/`. FFmpeg source: <https://ffmpeg.org/releases/ffmpeg-8.0.3.tar.xz>;
  dav1d source: <https://downloads.videolan.org/pub/videolan/dav1d/1.5.5/dav1d-1.5.5.tar.xz>.
  The tools are separate programs and can be replaced by rebuilding them
  (`ffmpeg/licenses/SOURCE.md`).

## Build identity

Each archive's `BUILD.json` records the version, the Git commit and whether the tree
had uncommitted changes, the target, the `rustc` version, the CPU baseline of each
executable, the control-protocol version, the FFmpeg version, configuration and
manifest digest, the signing status and the SHA-256 of every file. The archive name
says `unsigned-test`; a package made with a placeholder FFmpeg additionally says
`placeholder-ffmpeg` and must never be distributed.

| Package | Archive | SHA-256 |
|---|---|---|
| linux-x86_64 | `splot-studio-0.1.0-unsigned-test-linux-x86_64.tar.gz` | to be filled from the CI artifact |
| windows-x86_64 | `splot-studio-0.1.0-unsigned-test-windows-x86_64.zip` | to be filled from the CI artifact |
| macos-arm64 | `splot-studio-0.1.0-unsigned-test-macos-arm64.zip` | to be filled from the CI artifact |
