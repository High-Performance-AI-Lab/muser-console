# Contributing to muser-console

Thanks for helping improve the console. The project has a few hard
constraints worth knowing before you start:

- **No missing-data theater.** The console never replaces unavailable
  measurements with zeroes or demo data. Unavailable stays visibly
  unavailable — preserve that in every view you touch.
- **No CDN, no JS packages, no frontend build step.** The UI is the single
  checked-in `ui/muser-dashboard.html`. Keep it self-contained.
- **The server is one Rust binary.** Avoid new runtime dependencies;
  features must work from a release build with no side services.

## Getting started

```sh
cargo test --workspace --locked     # unit + conformance tests over fixtures/
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Conformance tests replay the recorded captures under `fixtures/`; they run
offline. New engine-facing behavior should come with a fixture or an update
to `schema/metrics-schema.json` when the metric surface changes.

## Pull requests

Keep diffs minimal and focused, and describe the user-visible change. UI
changes should be verified in a real browser, including the iPhone layout
when the pairing flow is affected.

Unless you state otherwise, we assume contributions are dual-licensed under
the repository's `MIT OR Apache-2.0` terms, the same as the project.
