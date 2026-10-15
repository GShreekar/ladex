# LADEX desktop app

A Tauri 2 window around a node running in the same process. The UI is the one in `ui/`; here it uses
`IpcClient`, which reaches the node through the commands in `src/main.rs` instead of HTTP, so there is no
port, certificate or login between the window and its node. The commands are thin: what they do is in
`ladex_core::node::Bridge`, which has its own tests (`crates/ladex-core/tests/bridge.rs`).

The node is also reachable from the network as usual: other devices and browsers use the same ports as
`ladex`, and `ladex send`, `get`, `ls`, `devices` and `invite` work against the running app.

## Build

The app embeds the built web UI, so build that first. On Linux install the webview libraries
(`libwebkit2gtk-4.1-dev`, `libxdo-dev`, `libssl-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`).

```sh
(cd ui && npm ci && npm run build)
cargo run -p ladex-app
```

## Settings until the first-run wizard

| Variable | Meaning |
|---|---|
| `LADEX_PASSPHRASE` | Passphrase the node joins the mesh with and requires of browsers; unset means an open node. |
| `LADEX_DATA_DIR` | Where the node keeps its files and settings; the same default as `ladex` (`~/.ladex/files`). |

Files saved from the window are written to the system downloads folder.
