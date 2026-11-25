# Contributing to Lyrachi

Thanks for looking at the code. This page covers what a contribution needs; the issue and pull request templates cover the rest.

## Getting started

You need Rust 1.89 or newer (edition 2024).

```sh
git clone https://github.com/ikhsan3adi/lyrachi
cd lyrachi
cargo build
cargo run --release
```

The test suite is offline by default. Provider tests run against local mock servers, live-network tests are marked `#[ignore]`, and integration tests point `LYRACHI_HOME` at a temporary directory, so nothing in the suite touches real services.

## The gate

Run all of this before opening a pull request. CI runs the same commands on Linux and Windows plus a macOS check, so a green local gate means no surprises:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo clippy --locked --all-targets --no-default-features -- -D warnings
cargo test --locked --all-features
cargo test --locked --no-default-features
cargo test --locked --release --all-features --test dod_verification
```

## Tests

- Unit tests live in `src/` modules, integration tests in `tests/`. `cargo test <name>` filters across both.
- Keep new tests hermetic: no real services, no network. `media::fake` is the reference media backend for tests.
- Time-dependent code takes `now: Instant` as a parameter. Advance the instant in tests instead of sleeping.
- `tests/dod_verification.rs` encodes release behaviors. Do not weaken or delete those tests.
- UI snapshots render fixed 80x24 and 120x40 terminals. If your change moves pixels, run `INSTA_UPDATE=always cargo test --test ui_snapshots`, then read the `.snap` diff before committing it.

## Code conventions

- `src/lib.rs` has `#![deny(unsafe_code)]`. The one exception is the WinRT MTA init in `src/media/smtc/com.rs`. Do not introduce unsafe anywhere else.
- Use the width helpers in `core::text` (`display_width`, `wrap_text`, `grapheme_count`) for layout, not `str::len` or `chars().count`.
- One `tokio::select!` loop in `src/app/` owns UI state; media and provider work happens in tasks that send `AppEvent` over `mpsc`. Keep `Arc<Mutex<...>>` off the render path.
- Input limits are product behavior: HTTP bodies 512 KiB, local lyric files 256 KiB, 2,000 lines, 16,000 word tokens, TTML depth 32. Return a clear provider error instead of truncating.
- Never cache provider errors. Only positive answers and "not found" are persisted, and per-track offsets live in `offsets.json`, not the SQLite cache.
- Each track change bumps a resolution epoch and drops results from stale epochs. Rapid track changes must never render the previous track's lyrics.

## Commits and pull requests

- Follow Conventional Commits with a module scope: `feat(ui):`, `fix(media):`, `fix(timeline):`.
- Keep one topic per pull request. Open an issue first for anything that changes behavior or adds a backend.
- Update the user docs (`README.md`, `docs/config.md`, `docs/keybindings.md`) in the same pull request when behavior changes.
- Pushes that only touch docs skip CI, so those pull requests carry no checks; a reviewer reads them directly.
- By opening a pull request you agree that your contribution is licensed under GPL-3.0-only, same as the project.

## Platforms

MPRIS on Linux/BSD is the primary target. Windows SMTC, MPD, WebNowPlaying, and the two macOS backends also ship. The macOS backends are compile-tested only, so runtime reports from macOS users are especially welcome.

## Reporting problems

Bug reports and feature requests go through the issue templates. Anything you suspect is a security problem goes to the private channel described in [SECURITY.md](SECURITY.md), never a public issue. Behavior is covered by the [Code of Conduct](CODE_OF_CONDUCT.md).
