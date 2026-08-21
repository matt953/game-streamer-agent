# game-streamer-agent

A pure-Rust game streaming agent for Windows, Linux, and macOS: hardware
capture/encode, QUIC transport, virtual displays, and a modular source
system (desktop, headless, emulators). Cloud-first design; single binary
per OS.

## Status

Early development — skeleton + loopback pipeline: test-pattern
source → H.264 → QUIC → decoding client, with latency instrumentation.

## Quickstart

```sh
cargo run -p gsa-agent -- run          # terminal 1: the agent
cargo run -p gsa-client-dev            # terminal 2: watch the stream
```

Headless stats (used by CI):

```sh
cargo run -p gsa-client-dev -- --headless --frames 300 --json
cargo run -p gsa-agent -- status --json
```

Requires only rustup + your platform's standard C toolchain. Tests:
`cargo test --workspace`; full e2e: `cargo xtask ci-e2e`.

## Stream health figures

The client instruments every stage it can see, on every backend, with one
vocabulary. Two ground rules across all of it:

- **A figure that was never measured shows as `—`, never `0`.** A zero
  reads as "instant" or "perfect", which is a claim.
- **Nothing depends on synchronized clocks.** Every number is either a
  duration measured on a single clock, or a measured round trip — which is
  what lets a backend with no clock sync (Moonlight) still report honest
  latency. Where a backend *does* sync clocks (the gsa agent), its measured
  total replaces the composed one.

### The latency chain

Reported as **p50 / p95 / p99** per stage, in ms, on the stream overlays
(apps and dev client) and in the dev client's `latency=` log field.

| figure | what it measures | how |
|---|---|---|
| **rtt** | the wire, both directions | the control link's round-trip time (ENet's smoothed estimate, republished ~1/s). A round trip on one clock — no sync involved. |
| **host** | capture → encode complete, on the host | measured *by the host* and carried per frame in the video frame header (0.1 ms units). A duration on the host's clock alone. `—` on backends that don't send it. |
| **decode** | one access unit through the client's decoder | timed around the decode call, client clock. `—` where the platform decodes out of our sight (apps' hardware paths). |
| **hold** | de-jitter pacing delay per released frame | how long the session chose to hold the frame to smooth delivery. Frames held for 0 count as 0, so the percentiles describe all frames, not just the held minority. |
| **present** | decoded → actually on screen | the display-side wait, where a presenter measures it (dev client). `—` in the apps, whose platform layers display immediately. |
| **total** | effective capture-to-display latency | `host + rtt/2 + decode + hold + present`, summed per percentile — the same arithmetic other streaming clients' overlays are read with. Requires at least the wire and the host stages, otherwise a client-only sum would masquerade as end-to-end. Excludes the host's capture wait and the panel's scanout, which nobody can measure. On the gsa backend the total is instead **measured outright** against synced clocks and includes everything. |

### Delivery and pacing

| figure | what it measures |
|---|---|
| **jitter in → out** | spread of frame transit drift as the link delivered it, and the same spread after pacing (p90 − p10 over a rolling window). A pair on purpose: the paced figure alone can't tell good pacing from a link that was never troubled. Both sides use the same window length and the same population of frames (pacing path only, content pauses excluded) — measured asymmetrically, a drifting baseline reads as spread and indicts the smoother for latency it never touched. |
| **mean_hold** | average pacing delay across held frames — the latency half of the smoothness trade. |
| **dejitter_duty** | frames paced vs skipped-for-backlog: whether the smoother actually ran. |
| **latency_growth** | change in transit drift since the session's first frame, in real ms. The one figure a *steady* backlog shows up in: a queue that fills once and never drains has no spread at all. Small and stable (either sign) is healthy; large and climbing is a filling queue. |
| **superseded** | frames decoded and then discarded unseen under the drop policy (still decoded — the reference chain needs them). |
| **content_pauses** | frames released immediately because their capture gap was the host's own idle time (change-driven encoding pausing on a still screen). Kept out of the jitter signal and the hold budget; the count proves the exclusion ran rather than merely that nothing went wrong. |
| **dropped / recovered** | frames lost to the wire vs rebuilt from FEC parity. |
| **recv_mbps** | rolling received goodput — what actually arrived and survived, vs what was requested. |

### Cadence (where a stutter was born)

| figure | what it measures |
|---|---|
| **stutters** | presented-cadence breaks: a frame gap over 2× the rolling median (min 40 ms). |
| **src_stutters** | breaks already present in the host's own capture stamps — the game or capture hitched; the stream merely carried it. |
| **cadence: captured-late / delivered-late** | each arrival-cadence break classified: the host never produced a frame in that window (nothing downstream could help), vs the host produced on time and the frame reached us late (network or client). Capture is checked first, so a late-made frame that travelled fast isn't blamed on the wire. |
| **freezes** | gaps over 250 ms regardless of cadence. |
| **present_fps / low1_fps** | decoded-frame rate at the client, average and 1% low. Counts frames as *arrived* — under a host that re-encodes unchanged screens this can exceed the game's own render rate. |

### Presentation and display

| figure | what it measures |
|---|---|
| **wait p50/p99** | how long a decoded frame waited for a refresh slot — the display tax invisible to every upstream measurement. |
| **frame_spread** | p99 − p1 of the gaps between *distinct* frames reaching the screen: the cadence a viewer perceives. Judder is spread, not rate. |
| **repeats / unshown** | refreshes that showed nothing new, and decoded frames never shown — the two halves of a rate mismatch. |
| **grid (pinned / adapting)** | whether present intervals sit on a fixed refresh grid (residual → 0) or scatter off it (→ 0.25): the only figure that can tell a VRR display adapting from one holding frames for whole refreshes. Only meaningful for content whose rate doesn't divide the refresh rate. |
| **hdr_out** | whether PQ is actually reaching the panel, as opposed to being requested and tone-mapped away. |
| **hdr: mastering / MaxCLL (wire)** | whether the stream carries *usable* HDR static metadata: `yes` means real numbers arrived; `—` covers both absent and present-but-zeroed (the host saying *unknown*), because either way there is nothing a display could use. The zeroed-vs-absent distinction, and what each platform's delivery path did with the payloads (Android's decoder forwards them; Apple's drops them and the client re-attaches), are logged, not shown — overlay rows describe what the user sees, logs carry the mechanics. |
| **hdr: HDR10+ (wire)** | whether ST 2094-40 per-frame dynamic metadata rode the stream. |

## Benchmarks

Live field sessions, 2026-07-17. iPhone client (VideoToolbox HEVC decode)
streaming from a Windows 11 / Linux (Ubuntu 26.04) agent (NVENC HEVC, 2560×1600@60), adaptive bitrate
in Auto mode (protocol ceiling 150 Mb/s). Agent is connected to LAN via Wifi 7 and iPhone client is connected by either Wifi 7 or remotley over 5G via VPN. This is intended to server an indication of the potential game streaming experience in a real-world environment. Figures are percentiles of the
agent's 1 Hz telemetry across each multi-minute session:

- **link / ping / jitter** — Open Speed Test download, ping, and jitter
  over the same path, run before the session. WiFi 7 LAN measures above
  the 150 Mb/s ceiling (never bound).
- **est** — the transport's live capacity estimate: the highest rate with
  no queueing delay accumulating on the path.
- **emit** — the encoder's actual output on the wire.
- **latency** — capture-to-present frame latency measured at the client's
  display handoff.
- **frames / intr** — frames fully delivered (complete vs received) and
  stream interruptions per minute (freeze + keyframe recovery cycles).
- **client fps** — frames decoded per second at the client, average / 1% low
  (capture runs at 60). End-to-end: bounded by the game's own render rate
  on the host, not a pure streaming metric. 1% lows pending the
  stream-health instrumentation.

> **IMPORTANT:** While testing is conducted with the agent connected via WiFi 7, this was
> done to present honest benchmarks captured under a non-optimal condition.
> We strongly recommend connecting the Game Streaming Agent host to your
> LAN directly over cable for normal operation and the best possible
> experience — likely yielding even better results than the benchmarks
> below. 

**Windows 11**

| Scenario | link (Mb/s) | ping / jitter (ms) | latency p50 / p95 / p99 (ms) | client fps avg / 1% low | est p50 / p90 (Mb/s) | emit p50 / p90 (Mb/s) | frames | intr/min |
|---|---|---|---|---|---|---|---|---|
| Game content, WiFi 7 LAN | 150+ (unbound) | 7 / 0.5 | 31.5 / 52.0 / 69.5 | 56.2 / 19.0 | 125 / 201 | 101 / 138 | 100% | 0 |
| Game content, 5G + VPN | 36 | 27 / 2 | 46.7 / 70.8 / 90.3 | 55.4 / 15.9 | 29.8 / 36.4 | 23.7 / 28.9 | 99.7% | 5.5 |
| Light desktop, WiFi 7 LAN | 150+ (unbound) | 7 / 0.5 | 37.0 / 66.8 / 159.6 | 24.1 / 9.0 | 144.9 / 190.7 | 45.5 / 51.1 | 99.9% | 1.3 |
| Light desktop, 5G + VPN | 36 | 27 / 2 | 45.5 / 81.9 / 151.4 | 23.5 / 6.8 | 23.9 / 34.2 | 6.6 / 8.7 | 99.3% | 7.2 |

**Linux (Ubuntu 26.04)**

TBD

**Methodology.** Game content: Diablo 4 gameplay. Light content: desktop
with a VLC 4K video (24 fps source) playing in fullscreen. VPN: WireGuard. 5G: Telstra (Australia), phone
side. Agent-side ISP: Aussie Broadband Fibre NBN (Australia). LAN: WiFi 7,
same site. Client: debug build of the Apple client application on iOS 27
running on iPhone 15 Pro Max with Xbox One controller paired to iPhone by
Bluetooth for input. Results do not include input latency from the
controller. Telemetry is collected by the built-in dev log sink
(`GSA_LOG_SINK`).

## License

Agent crates: [AGPL-3.0-only](LICENSE). Shared crates (`gsa-core`,
`gsa-protocol`, `gsa-transport`, `gsa-client-core`, `gsa-client-dev`,
`xtask`): [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE) — see each
crate's `Cargo.toml`. Contributions require a CLA (see
[CONTRIBUTING.md](CONTRIBUTING.md)).
