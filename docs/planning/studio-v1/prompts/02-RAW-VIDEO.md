# STUDIO-02 — real 8/10-bit Y4M to AV2 IVF/OBU

---

Implement STUDIO-02 only in the existing Studio repository. Read root AGENTS,
STATUS.md, and the architecture/media contracts. Verify STUDIO-01 is implemented;
fix only blocking prerequisites, without unrelated redesign. No MKV/MP4 muxer,
FFmpeg integration, audio output, HDR, or browser work.

## Inspect and reuse

Read the exact pinned Splot lib.rs, config.rs, frame.rs, encoder.rs, y4m.rs, ivf.rs,
relevant decode/validator public APIs, and source-use notices. Use the source
baseline from the pack; keep all splot-* dependencies on one rev. Do not modify
the codec checkout or duplicate its Y4M/IVF code. If a needed public API is absent,
report/prepare a narrow upstream change rather than writing another AV2 parser.

## Implement

1. A real worker mode of splot-studio-engine accepting a typed job over inherited
   framed pipes. It opens/validates input, maps validated settings to Splot Config,
   constructs SourceFrame values, drains packets, writes output, and returns a
   structured result. Encoder lifetime belongs entirely to the worker.

2. The supported input contract: progressive constant-frame-rate Y4M, eight or ten
   bits, initially the tested color/geometry/chroma combinations. Parse the actual
   header. Reject unsupported depths, HDR interpretation, interlace, invalid frame
   rate, invalid aspect/geometry treatment, out-of-range samples, empty/truncated
   input, and excessive memory requirements with typed errors. Unknown color
   values must remain explicit; disclose required interpretation. Do not infer
   missing properties from resolution alone.

3. Reuse Y4mReader with BufReader and SourceFrame::new ownership transfer. Preserve
   both precisions. Set rational frame rate and consecutive CFR PTS according to
   the existing writer contract. Resolve encoder settings once per immutable job.
   Never run encoder configuration construction in every UI redraw.

4. Implement two output routes: existing IvfWriter and documented AV2 Annex B
   packet-byte output. Verify the already-length-delimited Packet contract before
   writing .obu. No double prefixes, AV1 header/framing, or removed temporal units.
   Drain during input and completely after flush. The raw stream is labeled with
   its exact format; it is not uncompressed video or a multimedia container.

5. Integrate real GUI Open/Encode, supported settings, destination selection,
   progress stage/counters, cancellation, completion/reveal, and useful failure.
   Background processing remains out of the Slint thread. Unknown total frames
   produce indeterminate progress plus counts, not fake percentages.

6. Bounded, coalesced progress; reliable terminal events; independent control read
   loop during synchronous encode calls. Cancellation flag at safe boundaries and
   parent termination escalation for an unresponsive worker. Define and test EOF,
   UI close, worker death, and cleanup. Do not implement fake pause or background
   continuation before the supervisor phase.

7. Safe output transaction: source-identity checks, exclusive sibling temporary
   file, streaming writes, complete flush/finalize, verification, no-clobber
   publication, no false Completed state. A last-minute destination collision must
   not overwrite an unrelated file. Cap messages/logs and source dimensions.

8. Structural verification and a bounded decode pass using actual public APIs.
   Preserve named verification coverage; unsupported validator rules are not proof
   of conformance. Generate tiny owned fixtures and exercise independent AVM where
   available. Do not use ffprobe as a required AV2 verifier unless its actual AV2
   support is verified; the design does not depend on it.

## Tests

Use tiny fast fixtures at both depths (including meaningful ten-bit low bits),
one-frame and delayed multi-frame streams, noninteger rational fps, and supported
chroma layouts. Test low-level import preservation before lossy encoding. Compare
encoded output's Splot/AVM reconstructions, not lossy reconstruction to original
samples as though byte equality were expected.

Test header constraints, EOF/truncation, unsupported depth, source=destination,
existing destination, permission/writer error injection, cancellation, worker
crash, malformed IPC, full output pipes, max allocation bounds, and final drain.
Test each output by reopening it. Account for the upstream IVF zero frame-count
header. No whole-file allocation in the production path.

Add a small test harness/engine subcommand only when useful; don't invent a full
external CLI product. Keep fixtures generated or clearly licensed. A skipped AVM
check must report not-run, and the independent release gate remains open.

## Performance evidence

Measure a release worker job outside GUI versus through GUI with identical
settings, at both depths, to identify wrapper overhead without flaky CI timing
thresholds. Check memory high-water and UI response under load. Inspect the
worker-thread stack needs and implement them explicitly if required. No global
x86-64-v3/native flag copied into all application builds.

Run targeted tests and full supported CI. Launch a real encode through the GUI.
Update STATUS.md with exact commands and unsupported contracts. Stop with the
next prompt 03-FFMPEG-INPUT.md. Do not claim persistence, HDR, audio retention,
independent conformance, or cross-platform execution not actually tested.
