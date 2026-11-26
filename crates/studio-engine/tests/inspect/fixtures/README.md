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
