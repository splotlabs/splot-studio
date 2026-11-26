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

## Native sample viewer

The sample tests also use the vectors below. The source commit is
`7a1ef0978e69b4a0d06a2c3c3af9c97f28d8c793` for the Splot vectors.
The tests record selected values from AVM's native planes. Regenerate them
with the command above, omitting `--i420` to preserve each sampling format.
Omit `--skip-film-grain` to regenerate the post-grain output. All hashes
match with one and eight AVM threads.

| Bitstream | AVM native output SHA-256 |
| --- | --- |
| `syn-luma-palette2-row-mono-16x16-q0.ivf` | `418ecf79e4b2564419678cb25c28cbbf0259bf4602dd54f3e2868529d4df261d` |
| `syn-3frame-padded-filters-422-134x69-10bit-q150.ivf` | `88ed1eec0b69461416d840c759eaf37570b283d3c02ed18c482aa1a75a4f1a9f` |
| `syn-mrl-padded-444-67x67-10bit-q120.ivf` | `1d03e5c85f4542facb672568ae893e8c80194f322f9ccac111e128fccd1882c1` |
| `syn-2seq-crop-intra-64x64.obu` | `215d5e9d52a581a9ada3ba4d153016f2a65012661fc1e67289bd27e3ff74af0e` |
| `syn-3frame-filmgrain-inter-64x64-10bit.ivf` without grain | `5e4da3a2658836d7f15696188ecf895b16992d6fd033d30e4c62f579b81cdf89` |
| The same stream with grain | `e03fa2fe0bd341f21282ede12d527aea2066d00ab9603728a3acb31836f54b5b` |
| `syn-pq-420-10bit-64x64.ivf` | `d28d3ac800fa3af64ccbfba31bf7e38d16d871a3791ec57a8f4ab732723323a2` |

The PQ vector uses a Studio-owned 64 × 64 10-bit I420 source. Its Y4M header is
`YUV4MPEG2 W64 H64 F30:1 Ip A1:1 C420p10`. After the `FRAME` line, write
4,096 Y samples of 433, 1,024 U samples of 520, and 1,024 V samples of 620.
Store each sample in a little-endian 16-bit word. Use the AVM revision above.

```sh
avmenc --ivf --limit=1 --threads=1 --cpu-used=4 --bit-depth=10 \
  --input-bit-depth=10 --lossless=1 --color-primaries=bt2020 \
  --transfer-characteristics=smpte2084 --matrix-coefficients=bt2020ncl \
  -o syn-pq-420-10bit-64x64.ivf pq.y4m
```

The PQ input SHA-256 is
`bcdb7385fc217cf2c87885ef32295a559980fe31f9df404e9fa82106c1cbe235`.

`syn-msdo-ops-random-access.obu` is the independent AVM A1EC4594D9 fixture
from the Splot conformance corpus. It carries extended layers 3 and 7 and
declares global and local operating points. Global set 0, point 1 selects
six 64 × 64 pictures from extended layer 3. To verify the last exported
picture, hash the last 6,144 output bytes from:

```sh
avmdec --select-ops=0,1 --skip-film-grain --i420 --rawvideo -o - syn-msdo-ops-random-access.obu
```

The last picture's SHA-256 is
`4aaa603600b5c9c73dd0408845e59302b4e18974211eddb2ef3f203f459f1c44`.
