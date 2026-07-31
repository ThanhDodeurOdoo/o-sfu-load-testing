# o-sfu load testing

Use Rust 1.95 with edition 2024.

Keep the benchmarked o-sfu process separate from RTC generator processes.
Do not add `o-sfu-tests` to the dependency graph because it enables
`testing-transport`.

Preserve exact fixed-work correctness checks. Hosted runner timing is trend
data until a dedicated runner provides a controlled performance testbed.

Use the workspace lint policy without broad overrides. Every override and
every `unsafe` block requires a specific justification.

Run these checks after code changes:

```text
cargo +nightly fmt --all -- --check
cargo check --locked
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --release
```

Run the release smoke scenario after changes to process control, signaling or
RTC behavior.
