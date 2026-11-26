<p align="center">
  <img src="./docs/assets/hero.svg" alt="splot studio — The AV2 encoder workbench." width="100%">
</p>

<p align="center">
  <b>Native Rust AV2 encoder workbench.</b><br>
  Slint UI. Local processing.
</p>

A native desktop workbench for encoding video to AV2, built on the
[Splot](https://github.com/splotlabs/splot) codec. Rust and [Slint](https://slint.dev), for
Windows, macOS and Linux. Processing is local; there is no web or browser target.

## Status

Version 0.1.0, unsigned test builds only; nothing is released yet. Encoding is slow: the
codec is young.

**What it does**

- Converts ordinary local video files (MP4, MOV, MKV, WebM, MPEG-TS, AVI) and Y4M to
  video-only AV2, as IVF or an Annex B `.obu` stream. 8-bit and 10-bit SDR.
- One compact window, the Encode workspace: pick Quality and Priority, press Convert, watch
  real progress, cancel. Several files queue up, and the list survives restarts and crashes.
- Advanced lets you search encoder settings by name or option key. It shows each
  effective value and its origin. You can edit individual frame contexts, preview
  a base preset change, undo edits, and return to Simple with your overrides intact.
- Save a custom preset from Advanced. Import shows the changes before you apply them.
  A preset records the codec revision, schema, and portable settings. Source paths,
  crop coordinates, explicit tile dimensions, and first-pass statistics stay local.
- For two-pass variable bitrate (VBR), select a source, set the bitrate in Advanced,
  and choose Generate first pass. In Jobs, choose Encode with pass on the completed
  analysis. The engine checks the source content, stream, encoder settings, and frame
  costs before it opens the output.
- In Jobs, Clone settings restores a captured request to the editable draft.
  Re-encode checks the source and current encoder policy, shows changes for review,
  and writes to a new destination.
- Preview: the real encoded picture while converting, press and hold to compare with the
  original, scrubbing and playback of source and result.
- Every result is verified, then published without ever replacing an existing file. Source
  files are never modified.
- The Analyze workspace (File > Open to Analyze, or Open in Analyze on a converted file)
  reads AV2 IVF and Annex B files without changing them: a navigator of IVF records, OBUs and
  regions that did not parse, the selected element's envelope and header properties with
  their exact offsets, and its bytes. The Syntax view shows fields and bit spans that
  Splot can observe. In Findings, Validate runs the native checks on the current stream.
  Select a finding to inspect its location and field evidence where available. The report
  shows the scope of the checks and indicates partial coverage or a truncated report. Use the type and
  layer filters, Back and Forward, go to offset, and copy to navigate. A cancelled read
  keeps what it read. The open documents, their selection
  and filters, and the workspace are restored at the next start, after each file is checked
  again: a file that changed or is missing is said so, and nothing about it is shown until
  you read it again or locate it.
- You can inspect indexed rows, syntax, and bytes while Studio reads the rest of the
  file. Jobs shows the scan and lets you cancel it. If a seek reaches the end of
  the verified prefix, wait for more data and repeat the seek.
- Studio can reuse a complete structural index after it checks the source content.
  Use **Clear Index Cache** in Analyze to remove saved indexes. Opening the file again
  builds a new index when no valid copy exists. If an open document uses a saved
  index, close that document before you clear the cache.
- Select an OBU and open **Layers** to inspect the layer declarations and metadata for
  its position. Studio distinguishes global scope from numbered layers. When native
  effective metadata is available, Studio shows its origin and inheritance. Use
  **Show OBU** to open the source syntax. Declared profile, tier, and level describe
  the stream. Use **Findings** to see what the validator checked.
- The layer filter controls which navigator rows are visible. Use **Request Point**
  on an operating-point declaration to change the playback request. Studio reports
  whether a picture arrived for that request or why playback refused it. Use
  **Use Stream Default** to return to the default playback policy. Build the timeline
  again after a policy change. It uses the new operating-point request.
- Use **File > Validate Files…** to validate several local IVF or Annex B files. The default
  policy treats errors as failures. Strict policy also treats warnings as failures and
  preserves their warning severity. You can supply local Annex B sequence headers and
  operating point sets for streams that require external context.
- Jobs lists conversions, stream reads, validations, and decoded exports. Stop Validation
  Batch stops the current file and cancels files still waiting. Completed reports stay available until you
  remove them or close Studio. The session holds at most 32 validation jobs.
- In Analyze, play a supported AV2 document, step through decoded pictures, or seek to
  an output number. The picture links to the bytes that coded or displayed it. A backward
  seek can require decoding from the start of the stream. You can cancel the seek.
  When timing is unavailable, playback uses a labeled 30 fps assumption.
  A decode error leaves the navigator, syntax, and bytes available.
- In Analyze, select **Show Frame Timeline** and then **Build Timeline**. You can
  inspect the indexed rows during the scan. The timeline shows the available
  range and its verification status.
  Use **Coded order** to include frames with no output. Use **Output order** to
  inspect displayed pictures. A cancelled scan shows the retained rows with
  partial coverage.
- Use **Zoom In**, **Zoom Out**, **Previous Range**, and **Next Range** to change
  the view. A bar covers one row or a range of rows. Select a range bar to inspect
  that range. Bars show the largest byte count. Marks show the mean and minimum.
  Use **Show Table** to read the exact values and select a row with the keyboard.
  With focus on the graph, use Left and Right to step between rows. Enter a
  zero-based coded or output index in **Go**. When the output clock is valid,
  you can also enter an exact time in seconds, such as `1/30`.
- Selecting a row opens the linked OBU and its bytes. A row with an output also
  selects its picture. Use **Syntax** to inspect the selected OBU or **Findings**
  to inspect linked validation findings. **Back** and **Forward** restore the
  linked selection. The row shows available native base quantizer values and
  identifies them as signaled, derived, or inherited. These values apply to the
  frame header or its references. Block quantizer averages require block evidence.
- To inspect a coded frame, select it in the timeline or OBU list. Open **Blocks**
  and select **Inspect**. Studio decodes the frame and any earlier frames it needs.
  The table shows the tile, partition tree, coded and visible rectangles, prediction
  mode, segment ID, and block quantizer index where available. The capture status
  shows whether a limit stopped collection.
- Coded byte counts include the frame OBU and its continuation OBUs. Output byte
  counts assign each display OBU once. File accounting separates OBU bytes,
  framing, and unclassified bytes. The output bitrate uses the range's timestamp
  window, which ends at the next output timestamp. If the clock or end timestamp
  is unavailable, Studio gives the reason. Use **Copy Statistics** or **Save
  Statistics…** for exact values in the displayed range.

- Select a coded frame and open **References** to inspect its input and refreshed buffer
  slots. Use **Follow** on an available frame-level reference candidate. **Back** and
  **Forward** restore the inspection. Studio replays the source to obtain this state.
  The first indexed output appears when a verified timeline supplies that link.
- Inspect a decoded picture in RGB or select an available Y, U, or V plane. Fit shows the whole
  picture. Use integer zoom to inspect individual samples. Select a sample with the pointer
  or enter its coordinates to read and copy its exact integer value. The viewer shows
  plane dimensions, bit depth, and available color and chroma information. The grain control
  selects which decoded samples the viewer shows. Native planes remain available for
  sample inspection when the viewer cannot display the signaled colors in RGB.
- Pause on a decoded picture and choose a decoder overlay. Select a region in
  the picture or the list to inspect its geometry and decoder facts. Use the
  overlay plane control to distinguish overlapping luma and chroma trees.
  If a capture reaches its limit, zoom in or select a point to read more.
- In Analyze, use **Export Decoded Video** for a stream or range, **Export Frame** for
  the displayed output, or **Frame Hashes** for a report. A frame export keeps the
  displayed layer and grain policy. Jobs writes only the selected outputs, even
  when the decoder must read earlier pictures first.
- Y4M stores native samples with a constant frame rate. For a source with unknown
  timing, enter a rate to use for the export. Use raw planes for variable timing
  or picture metadata that Y4M cannot represent. The companion JSON file describes
  the plane layout and timing. Ten-bit raw samples
  occupy little-endian 16-bit words. Frame Hashes uses Splot's pre-grain
  `splot-dfh-sha256-v1` contract to compare decoded sample bytes.
- Jobs reports each published file. If part of a batch fails, the completed files
  stay on disk. Choose an unused destination before retrying a collision.
- Open a validation result in Analyze to inspect its findings without running validation
  again. From Jobs, export the complete retained report as JSON or plain text. The report
  records scope, interruptions, and omitted findings. When validation runs, the report
  includes the SHA-256 digest and the number of source bytes the validator read. A stopped
  validation may hash only a prefix. Studio attaches findings in Analyze only when both
  workers can verify the full source. Reports omit absolute paths. Completed checks cannot
  certify AV2 conformance.

**Inputs that Encode refuses**

Variable frame rate, interlaced, HDR (PQ, HLG, Dolby Vision), full range, non-square pixels,
rotation or cropping, RGB or alpha, encrypted streams, network and playlist inputs.

**What it does not do yet**

- Compare, and more of Analyze. The approved direction is one AV2 workbench with three
  workspaces (Encode, Analyze and Compare) and shared Jobs; see the
  [roadmap](https://github.com/splotlabs/splot-studio/issues/33).
- Audio, subtitles and chapters are not carried over; the app lists what a file would lose
  and asks before converting. Container output with audio is planned in
  [#12](https://github.com/splotlabs/splot-studio/issues/12).
- Closing the window stops the current file. Target file size, pause and background
  conversion remain unavailable.
- Packages are unsigned and need an x86-64-v3 processor (AVX2) on Windows and Linux.

Known defects are in the [issues](https://github.com/splotlabs/splot-studio/issues); what
stands between the test builds and a real release is tracked in
[#30](https://github.com/splotlabs/splot-studio/issues/30).

**Crates**

- `studio-app` (`splot-studio`): the Slint desktop app.
- `studio-engine` (`splot-studio-engine`): the headless engine. `worker` runs one job per
  process over a versioned, length-prefixed control protocol, `preview` serves pictures,
  `inspect` serves stream structure, syntax observations and validation findings from
  read-only IVF and Annex B files, and `encode` runs the same job from the command line.
- `studio-domain`: shared settings, job and protocol values (no GUI, codec or native types).

The engine accepts typed overrides with repeated `--option KEY JSON` arguments.
For example, this command requests the codec's half-size output and disables
rate-distortion optimized quantization. The engine checks the source and settings
before it opens the output file.

```sh
splot-studio-engine encode input.y4m output.ivf --option resize_denominator 16 --option preset.rdoq false
```

## Development

Rust is pinned by `rust-toolchain.toml` (1.100.0); rustup installs it on first use. On Linux the
GUI build needs `pkg-config` and the fontconfig development files (`libfontconfig-dev` on
Debian/Ubuntu). Reading files other than Y4M, and the test suite, need `ffmpeg` and `ffprobe`
on `PATH` during development; packages ship their own LGPL build instead (see
`native/ffmpeg/README.md`).

```sh
cargo xtask doctor                 # toolchain and native prerequisites; read-only
cargo xtask ci --fast              # fmt, repo checks, clippy, unit tests
cargo xtask ci                     # full gate, as CI runs it
cargo xtask run --release          # the desktop app, with its engine built to match
cargo run -p studio-engine -- --help
cargo run --release -p studio-engine -- encode in.y4m out.ivf
cargo xtask package --smoke        # unsigned test package for this OS in target/package/
```

`cargo xtask package` needs the FFmpeg pair from `native/ffmpeg/build.sh <target>`; it builds
the app for plain x86-64 and the engine for x86-64-v3 (the app refuses older processors
before starting it), writes `BUILD.json` and the notices, and archives a `.tar.gz` (Linux) or
`.zip` (Windows, and macOS with a `.app` bundle). `cargo xtask smoke` unpacks it and runs it
with an emptied environment. Packages are unsigned test builds; see `docs/release/`.

`cargo run -p studio-app` rebuilds only the app, not the engine it starts; use `cargo xtask run`.

Rules for contributors and coding agents are in [`AGENTS.md`](AGENTS.md).

## Licence

Source-available, not open source. Original project material is licensed under the
[PolyForm Noncommercial License 1.0.0](LICENSE.md). Uses outside that licence, which includes
typical commercial deployments, need a separate written licence from Bartosz Tomczyk
(bartekplus@gmail.com). SplotLabs and splot are project names he uses, not a separate company.
Third-party components keep their own licences, see
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md); a commercial licence for Splot Studio does
not cover them. Splot Studio uses Slint under the Slint Royalty-free License 2.0. How
contributions to Splot Studio are licensed does not change that, or the licence of FFmpeg or any
other dependency.

## Contributing

Read the [contribution policy](https://github.com/splotlabs/.github/blob/main/CONTRIBUTING.md)
first. Other people's contributions are licensed by their authors under the unmodified MIT No
Attribution License (MIT-0), through the
[SplotLabs Contribution Terms](https://github.com/splotlabs/.github/blob/main/CLA.md). You keep
your copyright. MIT-0 lets everyone who receives your contribution reuse it, including
commercially. It covers only your own original material. The rest of Splot Studio, including the
file you edit, keeps its licence.

A contribution is merged only after each rights holder's acceptance of the approved terms is
recorded. This link, a ticked box in a pull request and a green CI run are not that acceptance.
