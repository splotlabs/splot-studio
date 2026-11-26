# AV2 playback fixtures

Copyright (c) 2026 Bartosz Tomczyk. PolyForm Noncommercial 1.0.0.

The bitstreams are [Splot conformance fixtures][vectors] from commit
`6d5ada491438c6a26c2e792bf733ba18c3fb90a9`. The
[conformance manifest][manifest] records their origin and stream features.
Each `.yuv` file contains the corresponding AVM output in display order.
The pictures are 64 × 64 in I420 layout. The 10-bit files store the samples
in little-endian 16-bit words.

Use [AVM][avm] at commit `a1ec4594d97028ebc34083ec8346616d697c2fc9` to
regenerate a reference file. The output hashes match at one and eight threads.

```sh
avmdec --threads=1 --all-layers --skip-film-grain --rawvideo --i420 -o "$input.yuv" "$input"
```

| Bitstream | Input SHA-256 | AVM output SHA-256 |
| --- | --- | --- |
| `syn-frame-sef-families-64x64.ivf` | `5dd105511461c6dc9fab962544196e07a0f0ed78623deb58fa121e8049f77d43` | `074c7ea9e44e30e5017ffcd197a7528acbdd8b45e79f66839bc28e504208aa5d` |
| `syn-tlayer2-dependent-10bit.ivf` | `dec55a496b7d7395aaa38ba52a5c9b1f830bc1df926372f96a478c6034526617` | `2dba95f537ff163df9ff77be59f12418e14dbaa338a86a3e47f57ef8a99cb6ec` |
| `syn-mlayer2-derived-sef-8bit.obu` | `0deddfdf288ede3b467bfabbc456a128a817520b2578a46f8e17d4f6de6a557e` | `bca7d45ad940d920c77414eea6516e243bd0ea98d58441a6de270fa1fb231033` |
| `syn-mlayer2-crossref-10bit.obu` | `5e9110a00ed5c7cba80dc11dbc8af196ce8a0533054745e9b9e21a43539fcc8b` | `22240aff68d5f8bcdd886248862456ebdc731b0a978520badece7509961774a3` |

[vectors]: https://github.com/splotlabs/splot/tree/6d5ada491438c6a26c2e792bf733ba18c3fb90a9/tests/conformance/vectors/valid
[manifest]: https://github.com/splotlabs/splot/blob/6d5ada491438c6a26c2e792bf733ba18c3fb90a9/tests/conformance/manifest.toml
[avm]: https://github.com/AOMediaCodec/avm/tree/a1ec4594d97028ebc34083ec8346616d697c2fc9
