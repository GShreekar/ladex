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

## Installers

The release workflow builds them from one tag, after checking the tag matches the version in `Cargo.toml` and `ui/package.json`. Each installer carries the `ladex` CLI as well (Tauri's `externalBin`, added by `tauri.release.conf.json` so plain `cargo run` doesn't need it). To build one locally:

```sh
(cd ui && npm ci && npm run build)
cargo build --release -p ladex
mkdir -p crates/ladex-app/binaries
cp target/release/ladex crates/ladex-app/binaries/ladex-$(rustc -vV | sed -n 's/host: //p')
(cd crates/ladex-app && npx @tauri-apps/cli@2.11 build --config tauri.release.conf.json)
```

- **Windows (NSIS):** installs for all users. `windows/installer-hooks.nsh` adds inbound firewall rules for `ladex-app.exe` and `ladex.exe` and puts the install folder on the machine PATH (through `windows/cli-path.ps1`). Uninstalling removes both. There is no MSI because MSI refuses prerelease versions such as `3.0.0-rc.1`.
- **macOS (.dmg):** ad-hoc signed, with the Local Network usage string and the `_ladex._tcp` Bonjour type from `Info.plist`. On launch from Applications, `src/cli_link.rs` links `/usr/local/bin/ladex` to the bundled CLI. It asks for an administrator password once if it needs to, and never replaces a `ladex` that it did not install.
- **Linux:** a `.deb` that installs `ladex-app` and `ladex` into `/usr/bin`, and an AppImage. `install.sh` stays the route for the CLI alone.

## Tray, login item and notifications

- **Tray (menu bar on macOS).** Closing the window hides it; the node keeps sharing. The tray menu shows how many devices are connected and has Open LADEX, Open Downloads Folder, Launch at Login and Quit LADEX. Without a tray (some Linux desktops) closing the window quits, as before.
- **Launch at login** is off until ticked in the tray menu. The login item starts the app with `--background`, so it comes up in the tray without a window.
- **Opening the app again** brings the running window forward instead of starting another.
- **Notifications** are sent only while the window is hidden, minimized or unfocused: a device shares files, a file is offered to you, a download finished. They are raised by the page (`ui/src/state/notifier.ts`) because offers are addressed to the window's device.

## First run

The first time the app opens on a data folder it shows a short wizard before any node starts, so the node never runs open on the network: a device name, a mesh (start a new one with a generated passphrase, or join one by typing its passphrase), where received files go and how much space they may use, and whether to launch at login. The passphrase is kept in the OS keychain (a private file when there is none); the other choices are in `preferences.json` in the data folder. Nothing is asked when a node is already running on the folder or `LADEX_PASSPHRASE` is set.

## Settings, network and command line

- **Settings** (only on the machine running the node): the downloads folder, how files sent to this device are accepted (always ask, from paired devices, or from anyone in the mesh, with an optional size above which it still asks), the storage limit, the mesh passphrase (shown on request, to add another computer), launch at login and the theme. Changing the passphrase takes effect on a restart, which the page offers.
- **Network** runs the same checks as `ladex doctor`: addresses, whether the page answers, whether multicast works, other nodes, storage and disk, with a report to copy into a bug report. It also pairs with another node by comparing six words (`ladex pair`), and lists the trusted nodes with a button to revoke one (`ladex trust`).
- **Command line** lists the `ladex` commands that do what the app does, with the data folder the CLI will use and a copy button for each. Everything except the tray, notifications and the theme has one: `receive` stands in for the window as a device files are sent to, and `autostart` for Launch at login.
- `ladex settings` does the same from a terminal; the daemon and the app read the same files.

## Environment

| Variable | Meaning |
|---|---|
| `LADEX_PASSPHRASE` | Overrides the saved passphrase and skips the wizard. |
| `LADEX_DATA_DIR` | Where the node keeps its files and settings; the same default as `ladex` (`~/.ladex/files`), and `ladex` reads it too, so its commands find the app's node. |

Files saved from the window go to the downloads folder chosen in Settings, else the system downloads folder.
