# STUDIO-07 — future AV2 MKV and full multimedia output

**Do not execute during the initial video-only build.** This prompt is a future
entry gate, not a claim about current Matroska/FFmpeg status and not an automated
monitoring task.

---

The owner has explicitly started the multimedia-output phase. Read root AGENTS,
the current Studio code/tests, the media model, and this pack's original scope.
Keep existing IVF/OBU output working. Begin with an evidence review, not a muxer.

## Public compatibility gate

Retrieve the current authoritative public AV2-in-Matroska mapping and its exact
revision/status. Check codec identifier, CodecPrivate/configuration, packet/sample
framing, timing/duration, seeking/random access, metadata, and supported layers.
A codec ID appearing in a patch is not a complete mapping or stable implementation.

Identify an available tested muxer/demuxer implementation and pin it. Verify
independent demux→decode and seek on small Splot-produced files. Prefer released,
publicly reviewable support; do not invent an identifier, reuse AV1 framing, or
silently ship a private mapping. Record external player/version support separately
from container validity. Studio self-playback is not interoperability proof.

If this gate fails, stop container implementation and report the exact missing
public pieces and tests. Do not work around the gate by naming a private format
.mkv. Do not change the existing product claims or wait asynchronously for a release.

## Scope once the gate passes

Design complete-media jobs with explicit per-track Copy / Encode / Convert / Burn-in
/ Exclude policies as applicable. Preserve selected compatible audio/subtitle
streams, language/title/default/forced dispositions, chapters, attachments/fonts,
and supported metadata. Text, styled text, and bitmap subtitles require different
paths. Audio passthrough is not equivalent to audio decoding/re-encoding.

Use a timestamp-aware muxing adapter and checked rational time mapping. Account
for offsets, delay, preroll, encoder lookahead, decode/presentation order, variable
frame durations, audio encoder delay, negative starts, and trimming only if supported.
Audit Splot timing APIs before advertising VFR or accurate audio synchronization.

Keep incompatible output combinations visible in preflight. Do not convert without
warning, discard secondary tracks, or promise preservation of metadata that a
transcode destroys. Do not force every input into 4:2:0 or eight-bit conversion.

Reuse the current source inventory, immutable job spec, process workers, safe
publication, and verification model. Introduce the smallest adapter required, not
an NLE/pipeline framework. Evaluate FFmpeg support at the selected version and
maintain any justified upstreamable patch transparently with build/licence review.

## Acceptance

Round-trip track inventory, real packets, dispositions, subtitle rendering where
relevant, chapters/fonts/attachments, long synchronization, VFR, seek accuracy,
missing/unknown metadata, errors/cancellation, independent demux/decode, and tested
external-player compatibility. All prior video-only regression gates remain green.

Produce a narrow phased implementation plan based on what actually passed the
gate; do not implement the entire multimedia product in the evidence-review step.
HDR/HLG processing and display remain separately authorized work even when the
container supports storing those metadata fields.
