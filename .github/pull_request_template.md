## Summary

<!-- What changes and why? Link the issue with "Fixes #123" when there is one. -->

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Refactor
- [ ] Docs
- [ ] Packaging or CI

## Testing

<!-- Which commands did you run, and on which platform? Delete what does not apply. -->

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo clippy --locked --all-targets --no-default-features -- -D warnings
cargo test --locked --all-features
cargo test --locked --no-default-features
cargo test --locked --release --all-features --test dod_verification
```

Platform(s) tested:

## Checklist

- [ ] New tests are hermetic: no real services, no network, temp `LYRACHI_HOME`
- [ ] UI snapshots updated when layout changed (`INSTA_UPDATE=always cargo test --test ui_snapshots`)
- [ ] User docs updated when behavior changed (README.md, docs/config.md, docs/keybindings.md)
- [ ] Commits follow Conventional Commits with a module scope
- [ ] No new `unsafe` (src/lib.rs denies it; the WinRT MTA init is the only exception)
- [ ] `tests/dod_verification.rs` still passes unchanged
