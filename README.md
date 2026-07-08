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

## License

Agent crates: [AGPL-3.0-only](LICENSE). Shared crates (`gsa-core`,
`gsa-protocol`, `gsa-transport`, `gsa-client-core`, `gsa-client-dev`,
`xtask`): [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE) — see each
crate's `Cargo.toml`. Contributions require a CLA (see
[CONTRIBUTING.md](CONTRIBUTING.md)).
