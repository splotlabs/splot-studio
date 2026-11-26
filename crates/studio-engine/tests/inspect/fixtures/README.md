# Identity index fixture

Copyright (c) 2026 Bartosz Tomczyk. PolyForm Noncommercial 1.0.0.

`syn-bridge-celu-64x64.ivf` is the Splot conformance vector from commit
`7a1ef0978e69b4a0d06a2c3c3af9c97f28d8c793`. Its manifest records the
AVMenc source and the independent AVM decode results. The bytes have SHA-256
`b25a12e88381d03addd8ff256f7fb65e796996f24615df9ebcf1ef3dd1cac8c8`.

One IVF packet contains two temporal delimiters and three coded frames.
The decoder returns two pictures. The fixture checks that each identity
remains distinct and that the bridge frame stays in the index.

## Layer and metadata fixtures

The layer and metadata fixtures come from the Splot conformance corpus at
commit `7a1ef0978e69b4a0d06a2c3c3af9c97f28d8c793`. The corpus manifest records
their AVM provenance and expected decode or validation results.

- `syn-output-ci-2frame-64x64.ivf` checks the CI origin and the frame that
  establishes the effective metadata. SHA-256
  `77456c5add802db377847a55bef1955a1234df4f2324de4bd3dd109d2e0be4ff`.
- `syn-xlayer3-7-global-lcr-10bit.obu` checks global LCR scope and distinct
  sequences for extended layers 3 and 7. SHA-256
  `1ef9f61f15d1cbd29695158f318de46667ddfad53fa07a94c99dd26bb358597e`.
- `syn-lcr-64x64.ivf` declares a local LCR whose global parent is external.
  SHA-256 `057711a91d705882b4406c8e53062f004fca4f43d674e33af543dc4501b58f64`.
- `syn-ci-inheritance-reset.obu` uses bytes 0 through 524 of
  `syn-mlayer2-reproducible-8bit.obu` twice. Insert IVF bytes 70 through 81
  from `syn-output-ci-2frame-64x64.ivf` before byte 33 of the first copy.
  The first coded sequence inherits CI from embedded layer 0. The second
  sequence establishes unspecified values. Fixture SHA-256
  `c710fa8caeae5cc288f3aef40b487753eaef048abd575423339e5a69c97cbe38`.
  AVM `a1ec4594d9` decoded four pictures with `--all-layers --rawvideo --i420`
  to 24,576 bytes with SHA-256
  `d9586c8fc72e76c784f1a7f4961a2285a894a06c34a11d0339daffa1268232c3`.

- `syn-ci-default-priority.obu` uses only the first temporal unit of a
  64 × 64, 8-bit, three-embedded-layer stream encoded through the AVM
  `a1ec4594d9` C API. The sequence header makes layer 2 depend on layers 0
  and 1. A CI OBU at byte 35 signals BT.709 for layer 0. Layer 1 has no
  CI OBU. AVM reports CICP primaries 1, 2, and 2 for layers 0, 1, and 2.
  The raw I420 outputs at 1 and 8 threads have the same SHA-256
  `12097703ec09269ef686ae4c30bb85dbcbc0dd9ed3ddfa8ad5d984930ebc6546`.
  The fixture SHA-256 is
  `7f43e7f7d2997ee98b9cd378a10d47719f315d70ce3cc898b809b7392a2eadbc`.

## Block records

The IVF files below come from the Splot conformance corpus at commit
`8e08378f2bbb08213f2fc89685fe471c740e3cbf`. AVM `a1ec4594d9` inspection
provides the expected mode, geometry, quantizer, and motion facts.

- `syn-2tile-zone1-edge-128x64-q192.ivf` has an 8-bit vertical intra block
  at (64, 0), size 64 × 32, in tile 1 with qindex 192.
  SHA-256 `c9fd016f591250511b12d9a28ce2608b28be98035794042b4f04468ab3507475`.
- `syn-mrl-padded-444-67x67-10bit-q120.ivf` has a D203 block at (32, 64)
  with size 16 × 8 and visible size 16 × 3. Its reference line is 1.
  The angle delta is 2 and the qindex is 168.
  SHA-256 `53ff44b12489cfc3e8209df5ab176598163ad553f77063f5d4e3879ff1734e42`.
- `syn-2frame-sameref-compound-64x32-10bit-q150.ivf` has a 64 × 16
  NEW_NEWMV block at (0, 0) in the second coded frame. Its quarter-sample
  vectors are (32, 0) and (-128, 0) in eighth-sample units. Both use slot 0.
  SHA-256 `d0a83ae53e5ef9ff9009a1c1c1679eca71935234601d1ec7752061957b3381d4`.
- `syn-hrect-intra-64x64-q120.ivf` has two 64 × 32 luma blocks and luma
  transforms at (0, 0) and (0, 32). AVM reports `DCT_DCT`, qindex 120 and
  segment 0 for both. SHA-256 `0f85478c9dd5ad50bf4b5523be73b3213c8fd10e0d5b5f1c11997a46e9d21257`.
- `syn-lossless-sdp-nondc-chroma-v-intra-64x64-10bit.ivf` has distinct
  64 × 64 luma and chroma tree leaves at (0, 0).
  SHA-256 `3882ab66f3adc69279d8db24faa0511b850012858d1d6fa473cf6a76c80d190f`.
- `syn-gdf-xtile-lr-512x256-q190.ivf` has four tiles and 1,288 partition tree
  nodes in its first coded frame. It exercises capture limits and point queries.
  SHA-256 `5dd6f512faa08c00a8594e33a94f7484082c99445fdeee484011af7c987e54aa`.
