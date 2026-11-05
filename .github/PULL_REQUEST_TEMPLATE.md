## Summary

<!-- What changes, and why. Link the issue if there is one. -->

## Output compatibility

<!-- Does this change any bytes a codec produces, or any decoded result? If yes, which test pins it and why is the new output correct? -->

## Checks

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --locked`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] New or changed decoder paths are covered by a test or a fuzz input
- [ ] No `unsafe` added, and no new runtime dependency (or one is justified below)

## Notes for reviewers

<!-- Patent, licensing or dependency implications, if any. -->
