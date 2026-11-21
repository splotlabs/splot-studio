# Smoke-test fixture

`tiny-h264-aac.mp4` (3,202 bytes) is the MP4 that `cargo xtask smoke` encodes
through a packaged engine: 64×64, 30 fps, 6 frames of H.264 (yuv420p, limited
range, BT.709 tags) and a 0.2 s mono AAC tone, so the engine must also refuse it
until the audio track is acknowledged. It is generated test material made by
this project (FFmpeg's `testsrc2` and `sine` sources), covered by the
repository's licence, and never shipped in a package. Made with a development
FFmpeg 8.0.1:

```sh
ffmpeg -f lavfi -i testsrc2=size=64x64:rate=30 \
  -f lavfi -i "sine=frequency=440:sample_rate=8000:duration=0.2" \
  -frames:v 6 -map 0:v -map 1:a \
  -c:v libx264 -preset veryslow -crf 28 -g 6 -pix_fmt yuv420p \
  -color_range tv -colorspace bt709 -color_primaries bt709 -color_trc bt709 \
  -bsf:v filter_units=remove_types=6 -c:a aac -b:a 8k -ac 1 -shortest \
  -map_metadata -1 -map_chapters -1 -fflags +bitexact -flags +bitexact \
  -movflags +faststart tiny-h264-aac.mp4
```

`filter_units=remove_types=6` drops the encoder's SEI (its settings banner).
The Y4M sources are written by the smoke test itself.
