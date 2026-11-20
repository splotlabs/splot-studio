# STUDIO-03 — broad local input, video-only output

---

Implement STUDIO-03 only. Read root AGENTS, STATUS, product scope, architecture,
media contract, and dependencies/release guide. Preserve the working Y4M route.
Output stays IVF and AV2 Annex B. Reading MKV/MP4/MOV does not authorize writing
AV2 multimedia containers or processing audio/subtitle output.

## First establish the native build

Select an actual published `rsmpeg` release and compatible FFmpeg release/revision.
Read their exact build/binding/licence documentation. Build shared FFmpeg libraries
without GPL/nonfree flags and with only intentional external dependencies. A
separate FFmpeg command-line executable is not the production processing API.
Do not download an arbitrary “full build” and assume its licence/capabilities.

Create `native/ffmpeg/` recipes and a manifest containing exact source hashes,
configuration, toolchain, target, patches, licences, and build identity. Prove
library discovery on the current host and define native CI builds for each
required target. Do not force the basic Slint shell to require system-installed
FFmpeg on every developer machine before the adapter works.

Add `studio-media` as the narrow native boundary. Safe interfaces return owned
application media facts/frames, not raw FFmpeg types. Any new app-owned unsafe
block needs the human review prescribed by AGENTS. Prepare the ownership/safety
note and tests; do not bypass missing approval with a blanket allow. Independent
safe/domain/build work can proceed while this review is pending.

## Input workflow

1. Probe the source in a process-isolated worker, with bounds/timeouts/cancellation.
   Inventory video, audio, subtitles, attachments, chapters, dispositions, language,
   supported color, geometry, and timing facts. Do not decode all tracks to learn
   their names. Treat unavailable metadata as unknown, not a fabricated default.

2. Enumerate video tracks, avoiding attached artwork as an accidental primary
   video selection. Support one user-selected video per job. Build capabilities
   from actual decoder/pixel-format support and tested policy. Do not hardcode a
   claim that every extension FFmpeg knows is supported by this build.

3. Decode supported compressed input to frames. Respect timestamps, component
   layout, endian, byte stride, negative stride, sample alignment, and buffer
   lifetime. Convert/pack into Splot's actual owned sample representation without
   an intermediate eight-bit preview path. No unsafe transmute to u16. No extra
   frame clone just for progress. Use bounded queues and file reads.

4. Implement the supported progressive CFR SDR contract. Check decoded timestamps
   continuously; average fps metadata is insufficient. For VFR or midstream
   geometry/color changes, fail with a precise capability error unless a tested
   explicit policy already exists. Do not rewrite timestamps or round rates.
   Source offsets remain represented even though this release has no audio output.

5. Audit output colour signalling through Splot. If the API cannot convey a
   required property, constrain accepted input or prepare a separate upstream PR.
   Never call a sidecar tag “encoded color metadata.” Do not write AV2 syntax here.
   Recognize PQ/HLG/HDR and block unsupported processing; distinguish ten-bit SDR.

6. Show preflight's actual source inventory and explicit video-only omission
   warning. Record the acknowledgement against source identity, selected track,
   output kind, and immutable settings. If the source changes, re-probe and
   re-acknowledge. Do not silently discard audio/subtitles or claim them preserved.

7. Ensure production uses the pinned native libraries from the application bundle,
   not arbitrary PATH DLLs. Development system libraries are permitted only through
   an explicit documented opt-in that reports the loaded build identity.

8. Restrict FFmpeg protocols and nested external resource access. Reject network
   sources, URLs, playlists or side references outside the approved access policy,
   and unsupported encrypted/DRM inputs. Local top-level input alone is not a
   sandbox. Bound metadata length, stream count, dimensions, and memory before
   allocation where the APIs permit; retain process isolation for native failures.

## Test corpus and negative cases

Generate or use licensed tiny samples: compressed eight-bit SDR video, ten-bit SDR,
multiple video/audio/subtitle tracks, attached artwork, chapters, non-ASCII names,
unknown color, full/limited range, aspect/rotation properties, and damaged input.
Include timestamp offsets, real VFR, interlacing, HDR PQ/HLG, odd strides/layouts,
truncation, and changing dimensions as rejected/handled cases. Test exclusion
acknowledgement and stale-source invalidation, not just successful encoding.

Run frame-import tests before lossy encoding, independent source decoding checks
where practical, and Splot output verification. For each supported target record
native ABI/library-loading and a real 8/10-bit end-to-end test. A target with only
a cross-build is not ready for a multiplatform release.

No mandatory libass/audio-output/resampling dependencies for this phase. Reuse
available FFmpeg conversion facilities only under an explicit tested policy.
Do not add GStreamer alongside FFmpeg or modify the GUI/codec graph unnecessarily.

Update source/build/licence notices and STATUS with the exact supported matrix,
unknown/rejected cases, unsafe review status, tests, and actual loaded versions.
Stop. Next prompt: 04-PERSISTENCE-AND-RECOVERY.md.
