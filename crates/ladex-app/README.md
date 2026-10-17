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

`ui/dist` is shared: the app embeds it directly, and `ladex` stages a copy of it from `crates/ladex-cli/build.rs`, so one `npm run build` serves both.

## Tray, login item and notifications

- **Tray (menu bar on macOS).** Closing the window hides it; the node keeps sharing. The tray menu shows how many devices are connected and has Open LADEX, Open Downloads Folder, Launch at Login and Quit LADEX. Without a tray (some Linux desktops) closing the window quits, as before.
- **Launch at login** is off until ticked in the tray menu. The login item starts the app with `--background`, so it comes up in the tray without a window.
- **Opening the app again** brings the running window forward instead of starting another.
- **Notifications** are sent only while the window is hidden, minimized or unfocused: a device shares files, a file is offered to you, a download finished. They are raised by the page (`ui/src/state/notifier.ts`) because offers are addressed to the window's device.

## Settings until the first-run wizard

| Variable | Meaning |
|---|---|
| `LADEX_PASSPHRASE` | Passphrase the node joins the mesh with and requires of browsers; unset means an open node. |
| `LADEX_DATA_DIR` | Where the node keeps its files and settings; the same default as `ladex` (`~/.ladex/files`), and `ladex` reads it too, so its commands find the app's node. |

Files saved from the window are written to the system downloads folder.
