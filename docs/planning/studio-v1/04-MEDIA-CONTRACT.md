# Media contract: initial IVF/OBU product

## The source of truth

At inspected Splot commit `c1183fec4b81d72045d861973a9d8e563c6e9e2e`, the encoder
exports Config, Encoder, SourceFrame, Packet, presets, rate-control types,
ThreadCount, BitDepth, PixelFormat, Y4mReader, and IvfWriter. Reopen the actual
source before coding; do not invent signatures based on an old scaffold.
See [S02–S06](09-SOURCES-AND-OBSERVATIONS.md).

Observed reusable calls:

```text
splot_encode::y4m::Y4mReader::new(reader)
Y4mReader::header()
Y4mReader::read_frame()            → Option<[Vec<u16>; 3]>
SourceFrame::new(&config, pts, planes)   takes ownership of packed planes
Encoder::new(config)
Encoder::send_frame(frame)
Encoder::receive_packet()
Encoder::flush()
splot_encode::ivf::IvfWriter::new(writer, &config)
IvfWriter::write_packet(&packet)
IvfWriter::finish()
```

These are inspected API names, not a complete runnable sample. Use compiler/docs
and current tests to confirm borrowing, result types, and accepted configuration.
`SourceFrame::from_u8/from_u16` copies strided input; stride units there are samples,
not FFmpeg's byte-line-size units. Do not add an unnecessary second packing copy
when Y4mReader already produced owned planes.

## First encode path

```text
validated Y4M header
  → 8/10-bit SDR job settings and exact CFR
  → Y4mReader, streaming one frame at a time
  → SourceFrame::new with owned planes and consecutive CFR PTS
  → Splot encoder, draining available packets continuously
  → IVF writer OR AV2 Annex B output
  → flush encoder once, drain completely
  → finish writer, verify, safely publish
```

Never assume one packet appears immediately after every send_frame. Lookahead
and delayed output must be drained correctly. Never collect a full movie's packets
before writing. At EOF, failure or cancellation, distinguish intended drain from
abort and do not finalize an aborted stream as complete.

Reject empty input, truncated frames, out-of-range samples, unsupported depth,
invalid dimensions, unsupported interlace, and unreasonable memory requirements
with typed diagnostics. The upstream Y4M parser supports some depths beyond ten;
that does not make them accepted encoder inputs for this product.

## IVF

Use the existing writer. It writes the `AV02` fourcc, uses the inverse configured
frame rate as the time base, and writes each packet's PTS. The inspected writer
leaves header frame count at zero. Do not treat this as a failed encode or invent
another writer just to patch it. Check actual packet/frame output when verifying.
IVF's dimensions must fit its 16-bit fields; validate before output creation.
[S03](09-SOURCES-AND-OBSERVATIONS.md)

CFR frame rate stays rational (for example 30000/1001); no floating-point rounding
to 29.97, 30, or milliseconds in the stored job. For the initial path use a documented
zero-based CFR presentation timeline. Do not reuse the writer for arbitrary VFR
without proving the encoder and timestamp mapping contract.

## AV2 Annex B `.obu`

Use the precise name **AV2 Annex B length-delimited OBU stream** in docs/tooltips.
The filename extension alone does not define its framing. In the inspected API,
Packet data represents an Annex B temporal unit. Write packet bytes in the emitted
order after confirming that contract. Do not prepend a second OBU length, remove
required temporal delimiters, use AV1 framing, or concatenate bare OBU payloads.

Reopen output with Splot's actual stream/parser APIs. Keep timing and color
interpretation in the job report where they cannot be recovered independently
from the elementary stream. Do not claim a standalone `.obu` is self-describing
for every playback property. A later re-import must request missing timing rather
than silently using 30 fps.

No invented `.splot` multimedia container. An optional report is descriptive,
not a substitute for normative bitstream/container metadata.

## Input containers through FFmpeg

Deferring MKV **output** does not defer reading an existing MKV/MP4/MOV source.
The media adapter probes every stream and supported metadata, lets the user select
one video, decodes that stream, and supplies supported frames to the engine.
No AV2 encoder/decoder or AV2 muxer in FFmpeg is required for this architecture:
Splot handles AV2; the existing Splot writer handles the initial output.

Do not use a blanket `-map 0` mental model and assume preservation happens. The
initial output excludes nonvideo data explicitly. FFmpeg source decode support
comes from the actual bundled build, and remains a tested matrix rather than “any
file.” Reject encrypted/DRM or unsupported input with an explanation.

Use local file access and restrict protocols, nested resource access, and network
fetches. A local playlist may reference remote URLs or other local files; allowing
only a top-level local path is not a complete sandbox.

## Timing and geometry

Keep signed source timestamps, explicit time base, duration when known, and
stream start offsets in the input model. Normalize at a documented codec boundary.
Handle missing timestamps as an explicit unsupported/repair choice, not fabricated
average-fps timing. Initial public support is progressive CFR only. Detect nonuniform
timestamps as frames are decoded; header `avg_frame_rate` alone does not prove CFR.
If VFR is found late, abort safely, retain no completed output, and explain why.

A bounded preflight can say “CFR checking continues during encode.” Do not force a
whole-file scan for every open, or falsely claim complete preflight validation.

Preserve/resolve rotation, sample aspect ratio, display crop, and chroma siting.
If the current bitstream API cannot represent a property and pixel conversion is
not implemented, reject or require an explicit supported transformation. Sidecar
metadata alone does not establish correct standalone presentation.

## Eight-bit and ten-bit correctness

The encoding path must never pass through an eight-bit RGB preview image. Preserve
precision in owned input planes. Explicit conversions may change bit depth/chroma
only when requested, accurately described, and tested.

When importing FFmpeg frames, account for pixel descriptor, component layout,
endianness, byte line size, alignment, negative stride, and padding. In particular,
high-aligned ten-bit semiplanar storage and low-aligned planar ten-bit storage
are not interchangeable. No unchecked pointer casting from bytes to u16 planes.
Borrowed FFmpeg buffers must not outlive their owning frame.

Test 8/10-bit 4:2:0, and every additional advertised 4:2:2, 4:4:4, or monochrome
path. A constrained first UI may expose only tested combinations; it must not
quietly convert unsupported combinations. Add odd dimensions and unusual strides
where the codec/layout supports them. Include ramps and distinct low bits so a
hidden eight-bit truncation is detectable before lossy encoding.

## Color signalling is not only preview

Keep primaries, transfer, matrix, range, chroma location, and unknown values
separate. First audit which fields the pinned Splot encoder can actually emit and
which decoder fields can be read back. Do not assume that a GUI ColorDescription
or FFmpeg frame tag is automatically written into AV2.

If necessary public colour-signalling APIs are absent, prepare a small separate
Splot PR following that repository's current AGENTS. Until it is merged/pinned,
restrict the public input contract to the tested signalling/interpretation subset
and report the gap. Do not write duplicate AV2 syntax in Studio or enable arbitrary
BT.601/709/full/limited conversions with misleading labels.

HDR/HLG detection and unsupported-result reporting belong in the first import
implementation. HDR processing, tone mapping, mastering metadata changes, and HDR
display output are deferred. Retain their optional model fields only where real
probe data uses them; do not implement fake transformations.

## Verification

For positive tests, independently parse the output and decode it. Compare Splot's
decoded reconstruction against an independent AVM decoder for the exact same
encoded stream. Do not compare lossy decoded output byte-for-byte against original
input, and do not claim two Splot components constitute independent validation.

Separate production postflight from CI conformance. Production can use bounded
structural validation and a full decode with progress/cancellation; it must label
the exact checks and unsupported coverage. CI/release gates use independently
built/pinned AVM on tiny known fixtures plus broader owned/licensed corpus tests.
Missing AVM is “not run,” not a passing differential test.

Compare IVF and OBU representations of equivalent jobs at the decoded-output
level, and check packet/framing invariants independently. Test delayed output,
zero-frame input, one frame, multi-frame, tail flush, incomplete output, 8/10-bit,
and a rational noninteger frame rate. Keep byte-exact golden outputs scoped to a
pinned codec/configuration and its documented determinism contract.
