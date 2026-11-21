# STUDIO-05 engine: preview, live pictures, playback

Measured 2026-11-21. Host: Intel i7-12700H (20 threads), 45 GiB, Linux
7.0.0, Rust 1.100.0, `cargo build -p studio-engine --release` (portable
x86-64), Splot `main` at `e05a86f`, FFmpeg 8.0.1 (system, development
opt-in). **The host was not idle:** six unrelated `avmenc` processes ran
throughout (load average 9–12), so wall and CPU times of long encodes vary
by about 20 % between identical runs. Numbers come from a Python driver
speaking the framed protocol to the release binary (wall clock in the
driver, CPU and peak RSS from `wait4`, which include reaped `ffmpeg`
children).

Clips: FFmpeg `testsrc2`, 1280×720, 30000/1001 fps, 4:2:0. Y4M at 8 and 10
bits (30 frames for encodes, 300 for playback), H.264 MP4 of 600 frames
(libx264 defaults). Results: those 30-frame clips encoded by the worker at
preset 10, CRF 20 (one key frame; the encoder's default spacing is 120).
Pictures: `Fit` into 1280×720, so 720p frames are converted 1:1.

## Live pictures during an encode (30 frames, 720p, preset 10)

| Run | Wall | CPU | Peak RSS | Live pictures |
|---|---|---|---|---|
| 8-bit, no live (1st) | 102.6 s | 159.0 s | 397 MB | – |
| 8-bit, live | 100.3 s | 155.2 s | 437 MB | 7 (frames 0 6 8 14 16 22 24) |
| 8-bit, live | 96.0 s | 153.0 s | 508 MB | 7 (same frames) |
| 8-bit, no live (2nd) | 81.7 s | 129.7 s | 430 MB | – |
| 10-bit, live | 107.1 s | 170.3 s | 518 MB | 7 |

- All four 8-bit outputs are byte-identical (`cmp`), with and without live.
- The difference with and without live is inside the run-to-run noise of
  this loaded host; no wall-time or CPU overhead can be claimed or ruled
  out from these runs. The live thread's own work is bounded by decoding
  the same 30 frames once (one decoder thread) plus converting at most four
  pictures a second: the preview process steps through the same file in
  about 2.1 s CPU including 30 conversions, i.e. about 1–2 % of the job's
  ~150 CPU-seconds. Peak RSS grows by roughly 40–80 MB (the decoder's
  frames).
- The first live picture arrived 32–36 s after `Start`: the encoder emits
  its first packet only once its lookahead is full; nothing earlier exists
  to decode. Pictures then follow mini-GOP bursts, newest first, at most one
  per 250 ms.

## Playback (`Play`, Fit 1280×720, paced to 29.97 fps)

| Target | Frames | Shown | Skipped | Achieved | CPU |
|---|---|---|---|---|---|
| Y4M source, 8-bit | 300 | 300 | 0 | 30.0 fps | 5.95 s / 10 s |
| Y4M source, 10-bit | 300 | 300 | 0 | 30.0 fps | 6.09 s / 10 s |
| MP4 (H.264) source, 8-bit | 600 | 600 | 0 | 30.0 fps | 14.8 s / 20 s (incl. `ffmpeg`) |
| IVF result, 8-bit | 30 | 30 | 0 | 31.0 fps* | 1.45 s / 1 s |
| IVF result, 10-bit | 30 | 26 | 4 | 26.0 fps (stream covered at 30.2) | 1.68 s / 1 s |

\* Over a 1 s clip the first picture's start-up delay inflates the rate;
frames were shown on their due times.

The Splot decoder (two threads) plus conversion just keeps up with 720p
8-bit and falls slightly behind at 10 bits, where late frames are skipped
as specified (at least one picture per 250 ms is still shown).

## Single pictures (request to answer, measured in the driver)

| Request | Latency |
|---|---|
| Y4M 10-bit 720p, any frame (direct seek), incl. open | 41–47 ms |
| Y4M 720p, stepping frame by frame | 28.5–28.9 pictures/s |
| MP4 720p frame 0 (open: `ffprobe`, start `ffmpeg`) | 284 ms |
| MP4 next frame (running decode read ahead) | 39–40 ms |
| MP4 frame 300 / 599 / back to 100 (counted from frame 0; before the seek change) | 381 / 547 / 242 ms |
| MP4 stepping | 24.4 pictures/s |
| MP4 poster (frames 0..30 read; frame 30) | 182 ms |
| IVF result frame 0 (open, index, new decoder) | 74 ms (8-bit), 88 ms (10-bit) |
| IVF frame 29 from frame 0 (forward, one session) | 216 / 375 ms |
| IVF frame 28 after 29 (back: new session from the key frame) | 256 / 420 ms |
| IVF stepping | 19.4 / 16.7 pictures/s |
| Cached picture (poster again) | 6.5–7.4 ms |

About 30 ms of every 720p picture is the conversion (area filter and
matrix in `f32`, single thread); it is the largest cost for Y4M.

## Idle

A preview process that has answered a result picture and keeps its decoder
session open used 0 CPU ticks over 10 s (6 threads, all blocked).

## Bounds

- Pictures ≤ 1280×720 RGB (2.76 MB); boxes are validated before anything.
- Preview cache: 64 MiB of pictures. Up to 4 files open, 2 with a decoding
  session. Preview decoder: 2 threads, frame delay 1, decoded frames
  limited to 1 GiB (`MaxReferenceStoreBytes`) and luma samples per frame to
  the job's own frame-size limit. Live decoder: 1 thread, 32 queued units.
- Result index: ≤ 2^20 units, each ≤ 64 MiB.
- Measured peak RSS of the preview process: 33 MB (Y4M picks), 87–89 MB
  (Y4M stepping), 121–139 MB (720p result stepping), 158 MB (MP4 route).

## Far frames on the FFmpeg route: by time (follow-up)

Far frames (more than 48 frames from a running decode) are now found by
time: `ffmpeg -copyts -noaccurate_seek -seek_timestamp 1 -ss <t − 1 s>`
and `select=gte(t, pts₀ + N·P − P/2)`, after `ffprobe` confirms that the
seek lands on a key frame before the threshold. Files where it does not
(MPEG-TS, whose seek lands on any packet), whose first timestamp cannot be
read, or whose fractional period is under three ticks are counted from the
first frame as before. Identity is tested against the job's own reader at
first, last, non-key and reordered frames in MP4, MP4 with a 2 s offset,
MOV 10-bit, MKV with a 3 s offset, WebM (VP9), MPEG-TS 10-bit (counted)
and AVI (P-frames only; AVI has no timestamps for reordered frames, which a
job refuses anyway).

Clip: `testsrc2` 320×180, 29.97 fps, 10 min (17 982 frames), libx264
defaults (key frames up to 250 apart, B-frames), the same stream in MP4 and
remuxed to MKV. Release, same host, load average 3–5. Each row is one
preview process, requests in this order (cold = first request, which also
opens the file).

| Request | MP4 before | MP4 after | MKV before | MKV after |
|---|---|---|---|---|
| frame 17000, cold | 1144 ms | 242 ms | 2771 ms | 588 ms |
| 17001 (next) | 2.2 ms | 1.9 ms | 5.0 ms | 3.0 ms |
| 9000 (back) | 600 ms | 160 ms | 1148 ms | 194 ms |
| 9010 (read ahead) | 3.7 ms | 5.6 ms | 5.1 ms | 5.4 ms |
| 12000 | 792 ms | 148 ms | 1956 ms | 177 ms |
| 500 | 78 ms | 121 ms | 177 ms | 239 ms |
| 17981 (last) | 1131 ms | 141 ms | 2681 ms | 235 ms |
| CPU for the row | 19.1 s | 1.4 s | 36.0 s | 2.3 s |
| `Play` from 15000, first picture | 1009 ms | 261 ms | 2414 ms | 631 ms |

After the change a far frame costs one `ffprobe` landing check plus the
decode from the key frame before it (here up to 250 frames), independent
of the index; near the start (frame 500) the check makes it slightly
slower than counting. Playback from a far frame takes the same path as a
single picture; it then played at 29.97 fps with no skips, except the
last MKV run (56 of 2982 frames skipped while the host's load average rose
to 20).
