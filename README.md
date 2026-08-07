> [!WARNING]
> This repository was made with substantial AI help. Use and interpret with caution.

This repository drives the production
[o-sfu](https://github.com/ThanhDodeurOdoo/o-sfu) server through its public
HTTP, WebSocket and WebRTC boundaries.

The harness uses separate operating-system processes:

```text
o-sfu-load
├── o-sfu-load-server   production o-sfu process
└── o-sfu-load-rtc      str0m RTC peers on Tokio tasks
```

The process boundary attributes CPU and memory to the SFU or the generator.
Linux CPU sets can place them on different logical CPUs. This reduces
generator interference but does not remove hosted-runner and hypervisor noise.
The server admission cap matches the total synthetic peer count because every
local client shares one loopback origin.

## Workloads

`smoke` keeps the small one-publisher correctness test. `audio-mesh` makes
every peer publish Opus and receive every other peer. `video-gallery` makes a
bounded set of peers publish two-RID VP8 simulcast. `mixed-conference` combines
audio and video on the same participants and RTC connections. Each peer selects
one remote camera as featured when available and receives other cameras as
thumbnails. Mixed publishers use deterministic phase offsets within each media
interval so independent sources do not form synchronized sender bursts.

Reports count each simultaneous source-to-receiver forwarding relationship as
one media consumer. Each audio or camera publication creates one consumer for
every other peer in its room. Publishers do not consume their own publication.
A two-RID simulcast camera remains one consumer per receiver because each
receiver selects one RID. For example, 10 audio publishers among 60 total peers
create `10 × (60 - 1) = 590` consumers. The count would be 600 only for 60
listeners separate from the 10 publishers.

### Media units

The fixed sizes approximate average active-media output. They are deterministic
codec-shaped RTP payloads rather than recorded encoder output. Browser output
varies around the target and may drop below it with VBR, silence, DTX or
congestion.

| Media unit | Deterministic payload model | RTP packets/s | RTP payload bitrate |
| --- | --- | ---: | ---: |
| One Opus audio RTP stream | 80 B every 20 ms | 50 | 32,000 bit/s |
| One VP8 low RID RTP stream | 600 B fragments, 30 fps | 30.5 average | 146,400 bit/s |
| One VP8 high RID RTP stream | 1,100 B fragments, 30 fps | 423 average | 3,722,400 bit/s |
| One VP8 camera publication, two RTP streams | Low plus high RID | 453.5 average | 3,868,800 bit/s |

One participant publishing audio and camera therefore offers 503.5 RTP
packets/s and 3,900,800 bit/s of RTP payload. The audio model represents
continuous active full-band speech. Its 20 ms packetization is the Opus default
and 32 kbit/s is within the 28 to 40 kbit/s range recommended for full-band
speech in [RFC 7587](https://www.rfc-editor.org/rfc/rfc7587.html).

The video averages cover one complete two-second GOP at 30 frames/s. The low
and high rates sit below o-sfu's negotiated
[150 kbit/s low RID](https://github.com/ThanhDodeurOdoo/o-sfu/blob/9cae4cbaa196564fbebee033dc4e9e772b714124/crates/core/src/engine/media_transport/rtc/simulcast/common.rs#L14-L55)
and [4 Mbit/s high RID](https://github.com/ThanhDodeurOdoo/o-sfu/blob/9cae4cbaa196564fbebee033dc4e9e772b714124/crates/core/src/options/media.rs#L274-L290)
defaults. The high RID is a production near-cap stress profile rather than an
ordinary camera average. The fixed GOP adds conservative keyframe pressure.
Both RIDs use VP8 payload descriptors, PictureID, TL0PICIDX and marker
semantics. Each first keyframe fragment carries the complete ten-byte VP8
uncompressed prefix with its sync code and 320 by 180 low or 1,280 by 720 high
dimensions. [RFC 6386](https://www.rfc-editor.org/rfc/rfc6386.html#section-9.1)
defines that prefix. [RFC 7741](https://www.rfc-editor.org/rfc/rfc7741.html#section-4.4)
defines VP8 frame fragmentation across RTP packets. The largest 1,100 B RTP
payload leaves 180 B for IPv6, UDP, RTP, SRTP and negotiated extensions beneath
the 1,280 B IPv6 effective MTU discussed by
[RFC 8085](https://www.rfc-editor.org/rfc/rfc8085.html#section-3.2). Payload
bodies carry deterministic identities rather than decodable video.

Every rate above is RTP payload only. It excludes RTP headers, SRTP, UDP, IP,
RTCP and retransmissions. WebRTC similarly defines `maxBitrate` without IP or
transport-layer overhead in the
[WebRTC specification](https://www.w3.org/TR/webrtc/#dom-rtcrtpencodingparameters-maxbitrate).

Every measured payload carries an immutable source, layer, frame and fragment
identity. Receivers validate each source-to-receiver route independently.
Cross-source arrival may interleave while within-source ordering, exact packet
counts, payload bytes and duplicate freedom remain required. Warmup identities
cannot satisfy measured work.

## Build and run

```bash
cargo build --locked --release --bins

target/release/o-sfu-load \
  --server-binary target/release/o-sfu-load-server \
  --rtc-binary target/release/o-sfu-load-rtc \
  --output artifacts/smoke \
  smoke --receivers 1 --packets 50

target/release/o-sfu-load \
  --server-binary target/release/o-sfu-load-server \
  --rtc-binary target/release/o-sfu-load-rtc \
  --output artifacts/audio \
  audio-mesh --rooms 1 --peers 8 --seconds 30

target/release/o-sfu-load \
  --server-binary target/release/o-sfu-load-server \
  --rtc-binary target/release/o-sfu-load-rtc \
  --output artifacts/video \
  video-gallery --rooms 1 --peers 12 --publishers 4 --seconds 30

target/release/o-sfu-load \
  --server-binary target/release/o-sfu-load-server \
  --rtc-binary target/release/o-sfu-load-rtc \
  --output artifacts/mixed \
  mixed-conference --rooms 1 --peers 20 \
    --audio-publishers 5 --video-publishers 4 --seconds 10
```

Linux runs can isolate the o-sfu process from the RTC generator:

```bash
target/release/o-sfu-load \
  --server-binary target/release/o-sfu-load-server \
  --rtc-binary target/release/o-sfu-load-rtc \
  --server-cpus 0 \
  --rtc-cpus 1-3 \
  audio-mesh --rooms 1 --peers 8 --seconds 30
```

Each output directory contains the typed scenario, exact result, child logs and
one-second telemetry samples. Linux telemetry records separate SFU and RTC CPU
plus RSS. It also records o-sfu RTP counters and worker pressure diagnostics.

`o-sfu-load-report` combines one or more results into GitHub-flavored Markdown:

```bash
target/release/o-sfu-load-report \
  --input artifacts/audio \
  --input artifacts/video \
  --output artifacts/summary.md
```

The report embeds native Mermaid line charts for observed delivery rate,
scheduled sender RTP payload versus receiver-observed payload plus SFU CPU
average, peak and timeline data. Category charts contain at most four scenarios
per panel and use compact labels. Scenarios within each workload family are
ordered by planned load. Every panel declares its independent y-axis scale.
Single-scenario category charts are omitted because their exact table value
does not define a line. Telemetry timelines remain available when at least two
buckets were observed.
The CPU timeline groups real samples into at most 32 equal elapsed-time buckets
then applies a centered five-bucket moving average. The bucket count shrinks
when needed so a scrape gap is not filled with invented data. Bucket values and
the unsmoothed sampled peak remain in the report.
Tables retain the authoritative counters, packet discrepancies and process
metrics. Raw JSONL and logs remain available as workflow artifacts.

The comparison mode pairs the same scenario from two revisions:

```bash
target/release/o-sfu-load-report \
  --baseline-input artifacts/baseline/audio \
  --comparison-input artifacts/comparison/audio \
  --output artifacts/summary.md
```

It requires identical scenario, profile, server policy and workload plans.
Each comparison graph draws baseline and comparison as two lines on one
scenario axis. Tables show comparison-minus-baseline deltas beside exact
delivery evidence. The report includes a label legend. For example
`video-gallery-1x64-10p-60s` means one room with 64 peers, 10 video publishers
per room and a 60 second workload.

## CPU profiling

The nightly workflow profiles the passing 28-peer audio workload with 756 total
media consumers after the ordinary measurements. Linux `perf` samples only the
o-sfu process with the software `cpu-clock` event at a requested 99 Hz. The
profiling server is rebuilt with debug information and forced frame pointers.
The RTC generator remains a separate process on CPUs 1 through 3.

`o-sfu-load-profile` collapses the captured stacks and uses
[Inferno](https://github.com/jonhoo/inferno) to generate an interactive SVG
flamegraph. The job summary reports thread share, kernel share, unresolved leaf
and partially symbolized stack shares plus hottest leaf symbols, hottest
inclusive frames and hottest stack paths. Inclusive rows overlap by definition
and must not be summed.
The postprocessor expects `perf`, `inferno-collapse-perf` and
`inferno-flamegraph` on `PATH`. The nightly workflow pins Inferno 0.12.8.

The flamegraph, `perf.data`, folded stacks, profiled server binary and raw perf
reports are retained in the workflow artifact. The profile summary also records
the runner CPU model, logical CPU count, kernel, tool versions and maximum stack
depth. A separate publisher job validates each PNG preview and uploads it as a
run-specific asset on the `load-test-assets` prerelease. This gives the job
summary a public image URL without granting write access to the load process.
Later publisher runs remove preview assets older than 30 days. The complete
workflow artifact retains the interactive SVG, ranked breakdown and raw
evidence. If the hosted runner denies performance-counter access then the
ordinary load report remains valid and the profile section records that
profiling was unavailable.

Profiling is a qualitative diagnostic replay. Debug information plus frame
pointers and sampling affect that replay so its timings are excluded from the
authoritative nightly measurements. Manual revision comparisons profile both
revisions after their ordinary scenarios have completed.

## GitHub Actions

CI runs the bounded smoke. The nightly workflow runs these fixed profiles
using four assigned logical CPUs:

| Profile | Role | Publications/room | Consumers/source | Total media consumers | Deliveries/s | Exact deliveries |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 room × 8 audio peers × 30 s | Exact gate | 8 | 7 | 56 | 2,800 | 84,000 |
| 2 rooms × 12 audio peers × 60 s | Exact gate | 12 | 11 | 264 | 13,200 | 792,000 |
| 3 rooms × 12 audio peers × 120 s | Exact gate | 12 | 11 | 396 | 19,800 | 2,376,000 |
| 1 room × 28 audio peers × 120 s | Exact gate | 28 | 27 | 756 | 37,800 | 4,536,000 |
| 1 room × 12 peers × 4 cameras × 30 s | Exact gate | 4 | 11 | 44 | 6,052 | 181,560 |
| 1 room × 64 peers × 10 cameras × 60 s | Exact gate | 10 | 63 | 630 | 44,335 | 2,660,100 |

The 28-peer audio room exercises 756 total media consumers. The 64-peer video
room exercises 10 simulcast publishers and 630 total media consumers. The six
profiles gate 10,629,660 exact forwarded deliveries. Their job summary renders
the graphs directly without requiring an artifact download. Artifacts retain
the detailed evidence.

Scheduled runs and manual runs without comparison inputs keep the ordinary
single-version behavior. A manual comparison accepts `comparison_revision` as
one full o-sfu commit SHA. `baseline_revision` accepts another full SHA or
defaults to the o-sfu `master` commit resolved at the start of the job. A
baseline without a comparison is invalid. Both values are syntax-checked then
verified against the fixed o-sfu GitHub repository before Cargo receives them.

Both binary sets are built before measurement with separate Cargo locks and
target directories. The fixed scenario suite then runs sequentially on the
same `ubuntu-24.04` virtual machine. Both revisions assign the SFU to CPU 0 and
the RTC generator to CPUs 1 through 3. Each baseline scenario runs immediately
before its matching comparison scenario. Comparison artifacts retain both
result trees plus both lock files. The qualitative profiler replays the 28-peer
audio workload for each revision.

Comparison graphs cover receiver deliveries, receiver-observed RTP payload,
SFU CPU time per million deliveries, generator send lag, packet-loop delay,
SFU CPU average and SFU peak RSS. Tables also retain sampled CPU peaks, RTC CPU,
RTC RSS, SFU forwarding rate and sampled egress payload. CPU time per million
uses process CPU ticks across the telemetry window. That window includes setup,
warmup, measured work and drain.

Shared GitHub runners make CPU, RSS and rate measurements trend data. Exact
packet delivery and clean process shutdown are deterministic gates. The nightly
workflow resolves the current o-sfu `master` revision and records its full Git
commit in every result.

The summary marks a performance sample invalid when generator send lag exceeds
one audio packet or video frame interval. That warning does not turn shared-runner
timing into a deterministic gate.

## Verification

```bash
cargo +nightly fmt --all -- --check
cargo check --locked
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --release
```
