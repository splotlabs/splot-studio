# Identity index fixture

Copyright (c) 2026 Bartosz Tomczyk. PolyForm Noncommercial 1.0.0.

`syn-bridge-celu-64x64.ivf` is the Splot conformance vector from commit
`7a1ef0978e69b4a0d06a2c3c3af9c97f28d8c793`. Its manifest records the
AVMenc source and the independent AVM decode results. The bytes have SHA-256
`b25a12e88381d03addd8ff256f7fb65e796996f24615df9ebcf1ef3dd1cac8c8`.

One IVF packet contains two temporal delimiters and three coded frames.
The decoder returns two pictures. The fixture checks that each identity
remains distinct and that the bridge frame stays in the index.
