# Release checklist

What stands between the unsigned 0.1.0 test packages and a real release. An item
is ticked only with evidence (a log, a run link, a signed-off review); configured
is not executed, cross-built is not tested. Status as of 2026-11-21.

## Done for the test build

- [x] `cargo xtask package` builds the host package: portable app, x86-64-v3 engine
  (x86-64 targets), static C runtime on Windows, FFmpeg pair, generated Rust crate
  notices with licence texts, `BUILD.json`, `.sha256`. Run on Linux x86_64 only, with
  a placeholder FFmpeg.
- [x] `cargo xtask smoke` on Linux: unpacked, emptied environment, both executables
  report their identity, Y4M 8/10-bit and MP4 encoded to IVF and OBU and verified,
  start-up libraries checked.
- [x] Network audit on Linux (`strace`): no internet socket during launch, probe,
  preview and an encode.
- [ ] `.github/workflows/package.yml` executed on all three runners (written, never
  run). Expect the first run to need fixes on Windows and macOS: the `dumpbin`,
  `codesign`, `ditto`, `tar.exe` and Windows system-DLL paths are untested.

## Signing and notarization (needs owner credentials; none exist)

- [ ] **macOS:** Apple Developer ID Application certificate in a protected CI
  environment (never in pull-request jobs). Sign inside out with the hardened
  runtime and a timestamp: `ffmpeg`, `ffprobe`, `splot-studio-engine`, then the
  bundle: `codesign --force --options runtime --timestamp --sign "Developer ID
  Application: …"`. Decide entitlements (none expected). Verify with
  `codesign --verify --deep --strict` and `spctl --assess --type execute -vv`.
- [ ] **macOS notarization:** `xcrun notarytool submit … --wait` with an App Store
  Connect API key, then `xcrun stapler staple "Splot Studio.app"` and re-zip. Check
  the notarization log for warnings about the FFmpeg executables.
- [ ] **Windows:** Authenticode certificate (OV/EV or a cloud signing service),
  `signtool sign /fd SHA256 /tr <RFC 3161 timestamp URL> /td SHA256` on
  `splot-studio.exe`, `splot-studio-engine.exe`, `ffmpeg.exe`, `ffprobe.exe`; verify
  with `signtool verify /pa`. Signing does not buy SmartScreen reputation by itself.
- [ ] **Linux:** decide whether to publish detached signatures (for example
  minisign or GPG) of the archives and `.sha256` files, and where the public key lives.
- [ ] Remove "unsigned" from the archive name and `BUILD.json` only in the job that
  actually signed, and record the certificate identity there.

## Clean-machine tests (each advertised OS, real hardware or a fresh VM)

For each of Ubuntu 24.04, Fedora (current), Windows 10, Windows 11, macOS 15 and the
newest macOS, on a machine that never had Rust, FFmpeg or the source checkout:

- [ ] Download, check the SHA-256, unpack, first start through the OS warning.
- [ ] Open 8-bit and 10-bit SDR sources (Y4M, MP4, MKV, MOV, TS, AVI); inspect the
  excluded tracks; encode IVF and OBU; cancel and retry; verify; reveal the file.
- [ ] Quit and reopen; kill the app mid-encode and recover the interrupted list.
- [ ] Unicode and long paths, spaces; low disk space; destination collision;
  removable destination unplugged mid-encode; read-only data folder.
- [ ] Renderer fallback (`SLINT_BACKEND=winit-software`), high-DPI, light/dark,
  Wayland and X11, a portal-less Linux desktop for the Open dialog.
- [ ] Keyboard-only use, IME, and a screen-reader spot check (Orca, Narrator or
  NVDA, VoiceOver).
- [ ] The CPU floor on a real pre-AVX2 x86-64 machine (for example Sandy/Ivy Bridge):
  the app shows "This processor is not supported" and exits; nothing else starts.
- [ ] Windows: no console window flashes for the app, the engine or FFmpeg (the PE
  subsystem and `CREATE_NO_WINDOW` are in place, but nobody has looked).
- [ ] Uninstall by deleting the folder; the user's media and outputs are untouched.
- [ ] Upgrade over the previous test build once one exists (journal migration).

## Codec and media verification

- [ ] Independent AVM differential check of the release fixture set (no AVM
  decoder is integrated; Splot decoding its own output is not independent proof).
- [ ] Corpus regressions with the **bundled** FFmpeg (the tests use a development
  FFmpeg, 8.0.1, not 8.0.3-splot-studio), including every decoder the release names.
- [ ] Byte identity of x86-64-v3 and portable engine output on the release corpus at
  both depths (checked on one small clip only).

## Legal review (qualified reviewer, for the exact shipped combination)

- [ ] FFmpeg 8.0.3 LGPL 2.1+ and dav1d: statically linked executables shipped as
  separate programs; how source is offered (an upstream URL may not be enough:
  consider shipping the exact source archives next to each download, or a written
  offer), replacement instructions (`ffmpeg/licenses/SOURCE.md`), notices.
- [ ] Decoder patent exposure (H.264, HEVC, MPEG-2, MPEG-4 Part 2 and others) in the
  distribution countries; not settled by the LGPL configuration.
- [ ] Slint Royalty-free 2.0 conditions met by the About screen (AboutSlint), checked
  against the actual release.
- [ ] The Inter font (OFL 1.1) embedded by Slint, and every crate licence in the
  generated notices; crates whose published package has no licence file are listed
  in `BUILD.json` (`rust_crates_without_licence_file`) and need a decision.
- [ ] PolyForm Noncommercial terms for the app and the Splot codec, and their
  compatibility with distributing LGPL executables alongside.

## Product and process

- [ ] App icon (none yet: macOS shows a generic icon, Windows the default one).
- [ ] Bundle identifier `io.github.splotlabs.studio` confirmed or replaced before the
  first public build (changing it later orphans macOS permissions and preferences).
- [ ] Windows DLL search order: the app loads `opengl32.dll` and similar at run time;
  decide on `SetDefaultDllDirectories` (needs reviewed unsafe) or the linker's
  `/DEPENDENTLOADFLAG`, and test against a planted DLL.
- [ ] Release notes: fill in checksums from the CI artifacts; re-read the known
  limitations against the open issues at the release commit.
- [ ] A clean tree: `BUILD.json` must say `"dirty": false`.
- [ ] Human approval of the exact artifacts, then a manual upload. No tag, release or
  upload is made by any workflow in this repository.
