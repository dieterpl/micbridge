## What this changes

<!-- The user-visible effect, in a sentence or two. -->

## Checks

```sh
cargo fmt --all
cargo clippy --all-targets --all-features
cargo test --workspace --all-features
```

- [ ] All three pass
- [ ] Wire-format changes update `docs/protocol.md` and the version constant
- [ ] Nothing new allocates, locks, or blocks in an audio callback
- [ ] New tests record a finding rather than restating the code
