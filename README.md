> [!WARNING]
> This repository was made with substantial AI help. Use and interpret with caution.

This repository drives the production
[o-sfu](https://github.com/ThanhDodeurOdoo/o-sfu) server through its public
HTTP, WebSocket and WebRTC boundaries.

The foundation uses three binaries:

```text
o-sfu-load
├── o-sfu-load-server   production o-sfu process
└── o-sfu-load-rtc      str0m RTC peers on Tokio tasks
```

The process boundary keeps SFU CPU and memory attribution separate from the
RTC generator. Optional Linux CPU sets can place both processes on different
logical CPUs. This reduces generator interference but does not remove
GitHub-hosted runner or hypervisor noise.

The initial scenario creates one audio publisher and one or more receivers. It
uses the production room API, signaling protocol, ICE, DTLS, SRTP and UDP
packet path. The run fails unless every receiver observes every fixed-work RTP
payload in order.

## Build and run

```bash
cargo build --locked --release --bins
target/release/o-sfu-load \
  --server-binary target/release/o-sfu-load-server \
  --rtc-binary target/release/o-sfu-load-rtc \
  --receivers 1 \
  --packets 50
```

Linux runs can partition CPU time:

```bash
target/release/o-sfu-load \
  --server-binary target/release/o-sfu-load-server \
  --rtc-binary target/release/o-sfu-load-rtc \
  --server-cpus 0,1 \
  --rtc-cpus 2,3
```

Results and child logs are written under `artifacts/`. `result.json` records
exact delivery, achieved delivery rate and payload digests. The sender offers
one packet every 20 ms so this foundation measures whether o-sfu keeps up with
the requested work. It does not claim to measure saturation throughput.
`Cargo.lock` pins the exact o-sfu commit resolved from the Git dependency.

## Verification

```bash
cargo +nightly fmt --all -- --check
cargo check --locked
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --release
```

Performance comparisons on shared GitHub runners are diagnostic. Exact packet
delivery and clean process shutdown are required.
