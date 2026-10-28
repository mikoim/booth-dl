# booth-dl

A Rust CLI that bulk-downloads digital content purchased on BOOTH (https://booth.pm/).

## Project rules

- Crate APIs: the crate's source code (e.g. `~/.cargo/registry/src`) is the primary reference. Context7 docs may describe older versions.
- Write all source code and documentation in English.
- License: dual `MIT OR Apache-2.0` (`LICENSE-MIT`, `LICENSE-APACHE`).
- Before every commit, run `cargo fmt` and `cargo clippy --all-targets -- -D warnings`.
- Keep it simple (KISS) and leave touched code cleaner than you found it (boy scout rule).

## Local-only files (gitignored)

- `docs/superpowers/`: design specs and implementation plans. Never commit them.
- `captures/`: mitmproxy recordings and a logged-in Chrome profile. They contain passwords and session cookies, and this repository lives in a Dropbox-synced folder. Never print or copy secret values from them.
