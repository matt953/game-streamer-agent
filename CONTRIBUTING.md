# Contributing

Thanks for your interest! A few ground rules:

- **CLA**: contributions require signing a Contributor License Agreement,
  which grants the project maintainer the right to relicense contributed
  code. This is what allows the project to maintain its dual licensing
  structure (AGPL-3.0-only agent, MIT OR Apache-2.0 shared crates) and to
  sustain long-term development. The signing flow will be automated on the
  first PR.
- **Licensing**: agent-side crates are AGPL-3.0-only; shared crates are
  MIT OR Apache-2.0. New dependencies must pass `cargo deny check`.
- **Build rules**: `cargo build` from a fresh clone must always work with
  only rustup + a C toolchain. No shell scripts — repo tooling is
  `cargo xtask`. No build-time bindgen.
- **Before pushing**: `cargo fmt --all`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace`, and
  `cargo xtask ci-e2e` should all pass.
- **Architecture**: significant design changes should be discussed in an
  issue before implementation.
