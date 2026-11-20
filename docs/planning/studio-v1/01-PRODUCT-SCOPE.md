# Product scope and settled decisions

## Product definition

Splot Studio is a local-first, native Rust AV2 encoder workbench. It combines a
quiet, polished primary workflow with detailed settings and diagnostics on demand.
It is not a general video editor, streaming server, cloud transcoder, or a website
wrapped in a desktop window.

Accepted by the owner: Splot Studio name, multiplatform desktop focus, Slint and
`.slint` files, the attribution-based royalty-free Slint licence, 8/10-bit support,
HDR/HLG later, and IVF/OBU output before multimedia containers.

Proposed implementation choices in this pack: FFmpeg libraries through `rsmpeg`
for non-Y4M input, a private staging remote, source-available PolyForm licensing,
isolated media workers, and a durable local supervisor introduced incrementally.
These choices can be changed deliberately; do not silently reinterpret them.

## First usable slice versus first distributable release

The **first usable slice** opens progressive 8/10-bit Y4M, selects encoder
settings, writes IVF or AV2 Annex B, reports real progress, supports cancellation,
and verifies the result. It must use real codec calls, not a simulation.

The **first distributable video-only release** additionally imports tested common
source formats through the bundled FFmpeg build, inventories all streams, has a
persistent queue, safely handles failures, provides source/result preview, and
runs as a packaged application on the supported operating systems.

Do not present the Y4M slice as the complete ordinary-media application. Likewise,
do not block its development on future Matroska output.

## Capability policy

| Area | First slice | Video-only release | Later |
|---|---|---|---|
| Input | Y4M, generated fixtures | Audited FFmpeg-supported local files + Y4M | More codecs and explicit transformations |
| Output | AV2 IVF, AV2 Annex B `.obu` | Same | AV2 MKV after evidence gate; assess MP4 independently |
| Video | One selected track | One selected track per job | Multi-video/multilayer only with a separate design |
| Precision | 8-bit and 10-bit equally tested | Same | Additional depths only when encoder supports them |
| Color | Explicit SDR contract | Tested SDR conversions and signalling | HDR/HLG processing and presentation |
| Audio/subtitles | No output | Inventory and explicit omission | Preservation/transcoding/rendering with container support |
| Timing | Progressive CFR | CFR only unless VFR has an explicit tested contract | VFR preservation and any deliberate CFR conversion |
| Queue | One active worker | Durable queued jobs, safe retry | Parallel jobs only after resource-budget work |
| Preview | Minimal still/frame feedback | Video-only source/result preview and frame comparison | Synchronized audio, HDR preview |

This table sets deliverables, not an exhaustive codec compatibility claim. A
capability must be supported by the installed build and tests, not a filename
extension or a static list copied from FFmpeg's website.

## Omission contract

A file with nonvideo streams must remain usable as an input for video extraction.
Preflight shows the exact selected video and the inventory not included in the
output. Require acknowledgement in the job plan before execution, for example:

> Video-only export. This source contains 2 audio tracks, 3 subtitle tracks,
> chapters, and font attachments. They will not be included in the IVF/OBU file.
> The source file is not modified.

Persist the acknowledgement against the source fingerprint, selected stream,
output kind, and immutable job specification. Source replacement or a changed
selection invalidates it. Do not hide the warning behind a global “never warn
again” preference in the initial release.

The source inventory and output report are not backups of audio/subtitle data.
No sidecar media extraction, subtitle burn-in, or hidden remuxer is required in
this release. These are deliberately not half-implemented substitutes for MKV.

## SDR and unsupported media

Bit depth is independent of transfer function and dynamic range. Ten-bit SDR is
supported; HDR is not enabled merely because the encoder accepts ten-bit samples.
Recognized PQ/HLG/HDR input is blocked with a helpful explanation until the HDR
phase. Unknown metadata remains unknown; ask for an explicit interpretation or
reject the job when correctness depends on it. Never infer SDR solely from eight
bits, resolution, or the absence of mastering metadata.

Initially reject unsupported interlacing, alpha, dynamic geometry, and transforms
that cannot be represented or correctly applied. Where a tested explicit
conversion is added later, disclose it in preflight. Do not “support everything”
by silently dropping fields or normalizing frames.

## Scope exclusions

No browser/WASM target, mobile application, network URLs, capture devices,
playlist fetching, cloud upload, paid account system, hidden telemetry, plugin
marketplace, NLE timeline, distributed encoding, or new codec algorithms in Studio.
No automatic updater for the first release; signed manual downloads are sufficient.
No generic pipeline graph framework or preset language. No fabricated HDR, VFR,
MKV, or “zero-copy” capability labels.

## Completion semantics

“Encoded” means the codec produced output. “Published” means the finalized file
was safely made available at the chosen destination. “Verified” names a specific
successful verification operation. “Conformant” is not inferred from a same-codebase
round trip or the absence of implemented validator findings.

Record verification coverage as complete for a named check, partial, unsupported,
failed, or not run. Surface warnings independently from completion. A failed
verification must never be presented as success.
