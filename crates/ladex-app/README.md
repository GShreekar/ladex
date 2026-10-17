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

## First run

The first time the app opens on a data folder it shows a short wizard before any node starts, so the node never runs open on the network: a device name, a mesh (start a new one with a generated passphrase, or join one by typing its passphrase), where received files go and how much space they may use, and whether to launch at login. The passphrase is kept in the OS keychain (a private file when there is none); the other choices are in `preferences.json` in the data folder. Nothing is asked when a node is already running on the folder or `LADEX_PASSPHRASE` is set.

## Settings, network and command line

- **Settings** (only on the machine running the node): the downloads folder, how files sent to this device are accepted (always ask, from paired devices, or from anyone in the mesh, with an optional size above which it still asks), the storage limit, the mesh passphrase, launch at login and the theme. Changing the passphrase takes effect on a restart, which the page offers.
- **Network** runs the same checks as `ladex doctor`: addresses, whether the page answers, whether multicast works, other nodes, storage and disk, with a report to copy into a bug report.
- **Command line** lists the `ladex` commands that do what the app does, with the data folder the CLI will use and a copy button for each.
- `ladex settings` does the same from a terminal; the daemon and the app read the same files.

## Environment

| Variable | Meaning |
|---|---|
| `LADEX_PASSPHRASE` | Overrides the saved passphrase and skips the wizard. |
| `LADEX_DATA_DIR` | Where the node keeps its files and settings; the same default as `ladex` (`~/.ladex/files`), and `ladex` reads it too, so its commands find the app's node. |

Files saved from the window go to the downloads folder chosen in Settings, else the system downloads folder.
