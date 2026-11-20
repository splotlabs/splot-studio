# Third-party notices

Original Splot Studio material is licensed under the PolyForm Noncommercial License 1.0.0,
copyright (c) 2026 Bartosz Tomczyk (see `LICENSE.md`). That licence does not cover third-party
components, which keep their own licences and copyright notices.

## Direct dependencies

Declared licence expressions below are taken from each crate's `Cargo.toml` metadata as
resolved in `Cargo.lock`, not from a licence audit. Transitive dependencies are not listed;
`cargo metadata` lists them with their declared licences.

| Crate | Version | Declared licence | Used by |
|---|---|---|---|
| `serde` | 1.0.229 | MIT OR Apache-2.0 | studio-domain |
| `serde_json` | 1.0.154 | MIT OR Apache-2.0 | studio-domain, studio-engine, xtask |
| `thiserror` | 2.0.21 | MIT OR Apache-2.0 | studio-domain, studio-engine |
| `slint` | 1.19.0 | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 | studio-app |
| `slint-build` | 1.19.0 | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 | studio-app (build) |
| `rfd` | 0.18.0 | MIT | studio-app |
| `tempfile` | 3.27.0 | MIT OR Apache-2.0 | studio-engine |
| `splot-encode`, `splot-decode`, `splot-validate`, `splot-core` | git `main`, commit in `Cargo.lock` | PolyForm-Noncommercial-1.0.0 (same owner) | studio-engine |

## Slint

Splot Studio uses Slint under the **Slint Royalty-free License 2.0** option, which requires
attribution. Project policy is to show the official `AboutSlint` widget in an About screen
reachable from the top-level menu; `STATUS.md` records whether that has been verified in the
running app. The licence terms are Slint's; read them rather than any summary here:

- <https://github.com/slint-ui/slint/blob/v1.19.0/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md>
- <https://slint.dev>

The same `LICENSES/` directory is shipped inside the `slint` crate.
