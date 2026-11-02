## Summary

<!-- What does this PR change, and why? -->

## Lossless impact

<!-- Does this touch demuxing, cut planning or muxing? If yes, which
     equivalence tests cover it? Any change in exactness per format? -->

## Test plan

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo doc --workspace --no-deps` without warnings
- [ ] New/changed public items are documented

## Limitations / follow-ups

<!-- Known gaps, deliberately left out, or none. -->
