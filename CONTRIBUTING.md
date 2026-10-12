# Contributing to LADEX

Thanks for helping. Bug reports, fixes and features are all welcome. For a large change, open an issue first so we can agree on the approach before you write it.

Security problems are the exception: please don't open a public issue. See [SECURITY.md](SECURITY.md).

## Setting up

You need [Rust](https://rustup.rs/) (stable) and, for the browser and end-to-end tests, Node.js 22 or later.

```bash
git clone https://github.com/GShreekar/ladex.git
cd ladex
cargo build
```

The workspace has two crates: `crates/ladex-core` (everything the node does) and `crates/ladex-cli` (the `ladex` binary, which wires it together). The browser app is in `static/`.

## Before you open a pull request

CI runs all of these, so running them first saves a round trip:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
for t in tests/js/*.test.js; do node "$t"; done
cargo build && for t in data folder client restart push cli; do node tests/e2e/$t.test.js; done
```

`tests/e2e/swarm.test.js` (a source killed mid-transfer, `kill -9` on a receiver, ten receivers) moves a few hundred MiB and takes a couple of minutes, so it is not in that list; run it with `cargo build --release` and `LADEX_BIN=target/release/ladex`. The check that ten nodes get a file within about twice the time one does is `crates/ladex-core/tests/swarm.rs`, where `LinkedMesh::limit_bandwidth` gives each node a network card of fixed speed.

CI also runs `cargo deny check` (licenses, banned and duplicate crates, advisories) and `cargo audit`, and a Linux job that runs real nodes over lossy links (`tests/netem`, needs root).

## Tests

- **Unit tests** live next to the code, in each module's `tests` module.
- **In-process integration tests** are in `crates/ladex-core/tests/`. `ladex_core::testing::spawn_node` starts a real node on `127.0.0.1:0`, and `spawn_mesh(n, passphrase)` connects several; `FaultyLink` puts loss, delay, reordering or a partition between two of them.
- **End-to-end tests** in `tests/e2e/` start the real binary and drive it over HTTP.

A bug fix should come with a test that fails without the fix. Name tests after the behaviour they check, as a sentence: `a_revoked_key_is_rejected_on_reconnect`.

## Code style

- `cargo fmt` decides the layout (`rustfmt.toml`).
- Write code that explains itself. Add a comment only where it can't: one line, in plain English, saying *why*.
- Public items get a one-line `///` docstring.
- Anything that comes from another node or a browser is untrusted: check sizes, lengths and names before using them.

## Commits and pull requests

- Keep each pull request to one change, and explain what it does and why.
- Use a short, imperative commit title, e.g. `fix: resume an upload after the node restarts`.
- Changes to the mesh wire format must bump `PROTOCOL_VERSION` in `mesh.rs`, since nodes on different versions refuse each other.

By contributing, you agree that your work is licensed under the [Apache License 2.0](LICENSE).
