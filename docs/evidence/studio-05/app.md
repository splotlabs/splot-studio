# STUDIO-05 app: picture area, row thumbnails, compare and playback

Host: i7-12700H (20 threads), 45 GiB, Ubuntu 26.04.1, Linux 7.0.0-44, GNOME on
Wayland, Mesa Iris Xe; Rust 1.100.0; Slint 1.19.0 (winit backend). The engine
agent was building and running engine tests on the same machine at the same
time (load average 2–7), so timings are upper bounds, not clean-room numbers.

## Snapshots

Slint's own `Window::take_snapshot` through `examples/snapshot.rs`, software
renderer (`SLINT_BACKEND=winit-software`), no window decorations or
compositor. The picture area is the app's real `Preview` state machine and
`render_preview`; its requests are answered with painted test pictures (a
landscape, coarser on the result side), not engine pictures.

```text
SLINT_BACKEND=winit-software SLINT_SCALE_FACTOR=1 \
  target/debug/examples/snapshot OUT.ppm light|dark [advanced] SCENARIO
```

| File | Scale | State |
|---|---|---|
| `renderer-light-1x-source.png` | 1 | file read: poster, transport, `Original` label |
| `renderer-light-1x-live.png` | 1 | converting: live picture, `Live · frame 117 of 300` |
| `renderer-light-1x-result.png` | 1 | converted: `AV2 result`, hold hint, settings hidden |
| `renderer-light-1x-compare-held.png` | 1 | result held: the original of the same frame |
| `renderer-light-1x-actual-pixels.png` | 1 | 1:1 on: centred crop of the 1920×1080 frame |
| `renderer-light-1x-result-unavailable.png` | 1 | output file gone: the reason inside the area |
| `renderer-light-1x-advanced.png` | 1 | Advanced open: the SDR-approximation note |
| `renderer-dark-2x-live.png` | 2 | converting, dark |
| `renderer-dark-2x-list.png` | 2 | list: thumbnails in rows, the card scrolls |
| `renderer-dark-1.5x-result.png` | 1.5 | converted, dark |
| `renderer-dark-1x-running-list.png` | 1 | list during a run: the converting file's live picture |

Looked at, and fixed after looking: a black 16:9 box for unreadable files
(now no area; the card already says why), a placeholder text repeating the
card's "Reading…", the transport row appearing late and moving the card
(now shown disabled while the picture loads), the std `Slider`'s blue knob
(replaced by a token-coloured scrubber), an invisible checked state of the
1:1 toggle in light mode, the row state icon spilling out of its row.

## Height on a 1366×768 display

The window is capped at 680 logical px including the in-window menu bar
(768 − Windows task bar 48 − title 31 = 689; GNOME and macOS leave more).
Content taller than that scrolls inside the card; Slint 1.19 scrolls a
control into view when it receives focus (`Window::set_focus_item` →
`try_scroll_into_visible`), so Tab still reaches everything. Measured at
scale 1 with the default font:

| State | Window height |
|---|---|
| empty | 566 |
| file read (poster) | 668 |
| converting | 670 |
| converted | 634 |
| output gone | 462 |
| MP4 with omissions, a failure message, the list | 680 (scrolls) |

To get single-file mode under the cap without scrolling: the wordmark shows
only in the empty state, the file name is a plain row under the picture,
Quality and Priority have their labels beside the controls, spacing is 16 px,
and a converted single file hides Quality and Priority (they would apply to
nothing until Convert Another). Before this phase the list alone was 794 px
and did not fit a 768 display at all.

## Tests

- `cargo test -p studio-app --locked`: lib 134 (24 new for the preview),
  `tests/preview.rs` 3, `tests/stage.rs` 1, `tests/queue.rs` 8 (+1 helper
  ignored), `tests/sheet.rs` 1, `tests/worker.rs` 13. All pass.
- `tests/preview.rs` runs the real `Session` against the real engine
  (`target/debug/splot-studio-engine preview` and `worker`): poster, seek,
  step and playback of a 320×180 Y4M at 8 and 10 bits; a real job's live
  pictures, holding on a live frame (source frame of the same index), the
  result decoded by the engine, holding on it, stepping it; a result removed
  after completion shown as unavailable.
- `tests/stage.rs` drives the compiled window headless with real key and
  pointer events: press and hold, hold O, Space, arrows, Home/End, focus loss
  while held, and the Tab order picture → Play → scrubber → 1:1.

## Measurements

Release build. `examples/measure.rs` opens the real window on a generated
640×360, 150-frame 8-bit Y4M and drives it through its own callbacks (no OS
input); the engine is symlinked next to the example.

- **Idle CPU, poster shown** (the real `splot-studio` binary, sampled from
  outside over 20 s for the app and its preview process): 0 ticks of 10 ms,
  i.e. under 0.05 % of one core. The preview process held 5.4 MB RSS.
- **Picture latency** (polled every 1 ms, so 1 ms resolution): stepping
  through all 150 frames, a new picture was on screen after p50 1.0 ms, p95
  1.6–2.0 ms, max 3–10 ms (the engine answers from its own cache after the
  first decode); first poster 103–186 ms after launch, probe included. The
  debug-build integration test: seek 32–44 ms, 12 frames of playback in
  408–439 ms (0.40 s of media).
- **UI thread while converting** (preset 10, 20 s): a closure posted to the
  event loop every 20 ms from another thread waited p50 26–31 µs, p95
  127–158 µs, max 3.0–4.9 ms over about 995 samples.
- **Memory**: app RSS 137 MB idle with the poster (GL renderer). The picture
  cache is bounded at 32 MiB of pixels (unit-tested by byte accounting).
  Stepping through every frame repeatedly (the cache overfills on the first
  pass): RSS 155 → 183 MB over four passes with the GL renderer, 57 → 92 MB
  over ten passes with the software renderer. With one malloc arena
  (`MALLOC_ARENA_MAX=1 MALLOC_MMAP_THRESHOLD_=131072`, software renderer) it
  rose to 101 MB by pass 9 and stayed at 101.5–102.1 MB through pass 16
  (2400 pictures): a plateau, not per-picture growth. What fills those first
  40 MB was not identified (no heap profiler was run).

## Not verified

- Clicking, dragging, scrolling and holding in the real window by hand:
  synthetic input does not reach the window on this host. The callbacks were
  driven programmatically and the keys headless.
- Minimize detection: Slint has no event for it; the app reads
  `Window::is_minimized()` whenever anything happens. winit on Wayland does
  not report minimization, so there playback does not stop on minimize.
- Windows and macOS runtime (cross clippy only), screen readers, mixed-DPI
  moves between monitors, reduced-motion settings (no new animation repeats).
