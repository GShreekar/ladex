# LADEX - Local Area Data Exchange

A fast, secure, serverless file-sharing tool built with Rust for local networks, with no internet connection needed. Every device that runs LADEX is an equal peer: it keeps the files shared through it on its own disk and hands them to the other peers directly, several at a time. A web interface (any browser, including phones) is how you share and download.

## Features

- **Passphrase Protection**: One passphrase (generated or your own) gates both the browser login and joining the mesh
- **Zero Configuration**: No complex setup required - just run and share
- **Local Network Only**: All transfers happen over your local network, ensuring privacy and speed
- **Files outlive tabs**: shared files are stored on the node, so closing the tab (or locking the phone) that shared one doesn't make it disappear
- **Swarm downloads**: a node fetches a file in 1 MiB pieces from every node that has it, at once, and each piece is checked against its hash. A node starts passing pieces on as soon as it has them, so a popular file gets faster, not slower, the more devices want it. Nodes that go away mid-download, send bad data or go silent are simply skipped
- **Resumable and streaming**: uploads continue where they stopped after a dropped connection; downloads are plain HTTP, so every browser (including iPhone Safari) saves them straight to disk with no memory limit, and a node that doesn't have a file yet streams it through while fetching it
- **Text Messaging**: Send quick text messages between connected peers
- **Folder Support**: share a whole folder; download it into a real folder (Chrome, Edge) or as a streamed zip (everything else)
- **Cross-Platform**: Works on Linux, macOS, and Windows
- **Web Interface**: Modern, responsive web UI accessible from any browser
- **Session Security**: every device gets its own random, revocable login session; logging out ends only that device's session
- **Brute-Force Resistant**: Wrong guesses are throttled per device, and the passphrase is never sent over the network

## Supported Platforms

- Linux (x86_64, aarch64)
- macOS (x86_64, Apple Silicon)
- Windows (x86_64)

## Installation

### Desktop app

Every [GitHub Release](https://github.com/GShreekar/ladex/releases) has installers for the app, and each one also installs the `ladex` command:

- **Windows:** `LADEX_*_x64-setup.exe`. It installs for all users, puts `ladex` on PATH and adds the Windows Firewall rules.
- **macOS:** `LADEX_*_aarch64.dmg` (Apple silicon) or `LADEX_*_x64.dmg` (Intel). Drag LADEX to Applications. On first run it offers to link `ladex` into `/usr/local/bin`.
- **Linux:** `LADEX_*_amd64.deb` (puts `ladex` in `/usr/bin`) or `LADEX_*_amd64.AppImage` (the app only).

The installers are not signed yet. The release notes say how to get past SmartScreen and Gatekeeper.

### Quick Install (command line only)

**Linux/macOS:**
```bash
curl -sSL https://raw.githubusercontent.com/GShreekar/ladex/main/install.sh | sh
```

**Windows:**
Download the latest release from [GitHub Releases](https://github.com/GShreekar/ladex/releases) and extract the binary to a directory in your PATH.

### Manual Installation

1. Download the appropriate binary for your platform from [GitHub Releases](https://github.com/GShreekar/ladex/releases)
2. Extract the archive
3. Move the binary to a directory in your PATH (e.g., `/usr/local/bin` on Linux/macOS)
4. Make it executable: `chmod +x ladex` (Linux/macOS only)

## Usage

### Starting the Server

#### Open Access (No Passphrase)
Anyone on the network can open the web UI and join the mesh. LADEX prints a warning:
```bash
ladex
```

#### With a Passphrase (Recommended)
Generate a random passphrase (printed in the terminal, e.g. `k7mp-x2qd-r9wt`):
```bash
ladex -s
# or
ladex --secure
```

Or choose your own. Use something long: digits-only or short passphrases trigger a warning.
```bash
ladex "correct horse battery staple"
```

Other devices need the same passphrase, both to log in from a browser and to join the mesh. Devices running a different passphrase (or none) never join your mesh.

The server prints two addresses:

- **On this machine:** `http://localhost:8081` (the main port + 1, or `--local-port`). Browsers treat `localhost` as secure even without TLS, so there is **no certificate warning**.
- **From other devices:** `https://192.168.1.100:8080`. Other devices' browsers warn that the certificate is self-signed. Check that the SHA-256 fingerprint in the browser's certificate details matches the one LADEX printed before you accept it. The certificate is kept between runs, so each device only needs to accept it once (it is regenerated only when this machine gets an address it doesn't cover yet).

### Authentication Flow

When a passphrase is set:
1. **Server shows the passphrase**: for `-s`, the terminal prints it once
2. **Users enter it**: first-time visitors must enter it on the login page
3. **Throttling**: after 5 wrong guesses an address is locked out for 30 seconds, doubling with each further failure up to 1 hour. There is also a cap across all addresses, so guessing from several devices doesn't help
4. **Per-device sessions**: every device that logs in gets its own session (24 hours, or until the node restarts). Logging out ends only that device's session
5. **Signed-in devices**: the shield button lists the devices that are signed in. Any device can sign itself out; the machine running LADEX (opened at `http://localhost`) can sign out any of them, which also closes their open connection

### Security Status

LADEX is built for networks you partly trust, such as home, a classroom or an office, and not for the open internet. What has been done:

- A passphrase, numeric or not, always means a browser login. Only a node started with no passphrase is open.
- A device on the LAN that records all traffic can't join the mesh or use the web UI. The passphrase is never sent, and the mesh handshake is a SPAKE2 exchange that gives an attacker one guess per connection and nothing to crack offline.
- Wrong login guesses are throttled per address with exponential lockout: 1,000 wrong guesses from one address take over a month.

What to keep in mind:

- Without a passphrase (open mode), anyone on the network can read or change what you share; with `--no-tls`, so can anyone who can watch the traffic between a browser and its node.
- The passphrase protects joining. Everyone who has it can see and download everything shared on the mesh.
- The TLS certificate is self-signed, so the first visit from each browser shows a warning.
- LADEX has not had an independent security audit.

### Security Model

- **Mesh handshake**: nodes prove they know the passphrase with a SPAKE2 password-authenticated key exchange. The passphrase, and anything derived from it, never crosses the network, so recording traffic or discovery announcements reveals nothing that can be cracked offline. An attacker gets at most one guess per connection. Each node also proves it holds the private key behind its node id, and a node that has been revoked is refused even if it knows the passphrase.
- **Pairing**: two nodes can also be paired without sharing a passphrase. Open *Pair a device* (the link icon) on both machines at `http://localhost`, enter one device's address on the other, and confirm only if both screens show the same six words. A device in the middle would make the words differ. Paired nodes then reconnect on their keys alone; incoming pairings are accepted only while the panel is open.
- **Revoking a node**: *Trusted nodes* (the key icon, at `http://localhost`) lists the nodes this one trusts. *Revoke* disconnects a lost or stolen device and sends a revocation, signed with this node's key, to the whole mesh; every node then refuses that key, and nodes that join later learn of it when they connect. A node trusted only by passphrase cannot revoke one that was paired.
- **Man-in-the-middle protection**: nodes talk over QUIC and prove their keys in the TLS handshake, and the mesh handshake is bound to that very session. A node reached through discovery must hold the key behind the id it announced, and someone relaying or terminating the connection with their own key can't complete the handshake without the passphrase.
- **Wrong guesses are throttled** at both the browser login and the mesh handshake.
- **Each browser connection is one device.** A connection can only act as the device it joined as, so one signed-in device can't delete another's files or answer another's downloads. The node, not the browser, decides who uploaded a file and who hosts it.
- **Nothing from another device is trusted.** File names, sizes and types are cleaned on the node and again on the receiving browser (no path tricks, no look-alike extensions via invisible characters, no Windows device names). A sender can't write more data than it announced, a received folder never overwrites existing files in the folder you chose (clashes become `name (1).ext`), and a resumed download can only continue from bytes you really have.
- **Clocks can be wrong.** Shared files and devices are ordered with logical clocks, so a device with a wrong clock can't win conflicts or bring back something that was unshared. Updates stamped more than an hour ahead of a device's clock are ignored, and LADEX warns when a peer's clock is more than two minutes off.
- **Open mode** (no passphrase) has none of these protections. Use it only on networks you trust.
- **`--no-tls`** turns off encryption between browsers and nodes. Nodes always talk to each other over mutually authenticated QUIC, and the passphrase itself is never sent.

### Basic Operations

1. **Open your browser** and navigate to the server address
2. **Enter the passphrase** (if one is set)
3. **Connect peers** by sharing the URL and passphrase with other devices
4. **Send files** by dragging and dropping or using the file picker
5. **Send folders** by selecting entire directories
6. **Send a file to one device** by clicking that device (or dropping a file on it). It is asked to accept or decline first, and the file is never listed for anyone else
7. **Send messages** using the text input field
8. **Monitor transfers** with the real-time progress indicators
9. **Logout** when finished (passphrase mode only)

## Where files are kept

Shared files are stored on the node they were shared through, in `~/.ladex/files` (change with `--data-dir`), up to 20 GiB (`--storage-limit-gb`). A node also keeps a copy of any file it fetches for a download, so that it can serve it to others; when the limit is reached, the copies it fetched are removed starting with the least recently used, but files shared from the node are never removed to make room. Unsharing a file deletes every copy. Files left half-received are deleted after a day. Each node has its own key, made on first run, and its node ID is derived from it; the key is kept in the OS keychain (macOS Keychain, Windows Credential Manager, the Secret Service on Linux), or in `identity.key` in the same folder where there is none. The node also remembers its chat history, logged-in devices and the nodes it was connected to in `node.state` in the same folder, so a restart doesn't sign everyone out or make the node look new to the mesh; changing the passphrase signs everyone out. Don't share with LADEX what you wouldn't want copied onto the machines of the people on your network: anyone signed in can download any shared file, which stores it on their node.

## Commands

With a node running, the same `ladex` program controls it from another terminal on the same machine:

```bash
ladex send report.pdf photos/        # share files and folders with everyone
ladex send report.pdf --to Bob       # send to one device; it is asked to accept first
ladex ls                             # list what is shared (--all also lists files inside folders)
ladex get report.pdf -o ~/Downloads  # download by name or id (default: the current directory)
ladex seed photos/ --watch           # share a folder, and share it again whenever it changes
ladex devices                        # list connected devices and nodes
ladex invite                         # a one-time browser invitation as a link, a typed code and a QR code
ladex invite --permission upload     # also read-only (default) or upload-with-approval
ladex doctor                         # check that the node is running and reachable (also under Network in the app)
ladex settings                       # show the node's settings; --storage-limit-gb, --auto-accept, --downloads-dir change them
ladex unshare report.pdf             # stop sharing something this node shared
ladex receive                        # be a device others can send files to (--name, -o DIR, --yes)
ladex chat                           # read the room chat; ladex chat "hello" posts to it
ladex guests                         # browser guests and their uploads; sign-out, approve and deny act on them
ladex passphrase                     # show the mesh passphrase; passphrase set [--generate] changes it
ladex autostart on                   # start this node when you log in (off turns it off)
```

Everything the app does can be done this way, with no app installed. On a machine with only the command line:

```bash
ladex passphrase set --generate      # make up a mesh passphrase (or `passphrase set` and type one from another machine)
ladex                                # start the node; it uses the saved passphrase
ladex autostart on                   # and from now on, start it at login
```

- Add `--json` to any command for output a script can read. A command exits with 0 on success and 1 on failure.
- Add `--data-dir DIR` if the node was started with one (or set `LADEX_DATA_DIR`, which the app reads too); that is how a command finds its node. It works the same whether the node is the `ladex` daemon or the app.
- `get` never overwrites: a second copy is saved as `name (1).ext`. A folder is saved as real directories.
- `send` and `seed` copy what they share into the node's store, so a shared file takes its size twice on disk.
- `send --to` takes a device's name or its id from `ladex devices`, and only sends files, not folders. Ctrl-C while it waits leaves the offer open.
- `seed --watch` shares the whole folder afresh when it changes and takes the earlier version down, so downloads of the earlier version stop.
- `receive` keeps running as a device called after the node (or `--name`), so others can send it files with `send --to` or from the app. Files the incoming files rule allows (`ladex settings --auto-accept`) are saved at once; you are asked about the rest, and with no terminal to ask at they are declined unless you pass `--yes`.
- `chat` posts as this node, under its name.
- `passphrase` works with or without a running node: with one, a new passphrase takes effect when the node restarts; without one, it is saved in the data folder for the next `ladex` or app start.
- `autostart` uses a systemd user service on Linux (`systemctl --user`; add `loginctl enable-linger` to start it at boot without logging in), a launch agent on macOS, and a Run entry on Windows, whose console window starts minimized. It runs `ladex --data-dir DIR` with the passphrase and storage limit saved in that folder.
- The passphrase, storage limit and downloads folder chosen in the app are kept in the data folder, so `ladex` on the same folder uses them: it takes the saved passphrase unless you pass one, and the saved storage limit unless you pass `--storage-limit-gb`.
- Commands reach the node through a port on `127.0.0.1` and a token in `control.json` in the data folder, which only your user can read.
- A data folder belongs to one node at a time, held with a lock on `node.lock` in it. Starting `ladex` or the app on a folder that is already in use is refused, with the running node's name and port; the commands above keep working with it.
- A command and the node it talks to must be the same version; otherwise the command stops with "the running node is X, this command is Y — update one of them".

## Testing

```bash
cargo test                         # unit tests: storage, scheduling, merging, sessions, handshake, zip, ...
(cd ui && npm test)                # the web UI
cargo build && for t in data folder restart push cli parity; do node tests/e2e/$t.test.js; done   # real nodes, real HTTP
```

## Command Line Options

```bash
ladex [PASSPHRASE]     # Launch with your own passphrase
ladex -s, --secure     # Launch with a generated passphrase
ladex                  # Launch without a passphrase (open access, prints a warning)
ladex --local-port N   # Port for the localhost-only HTTP listener (default: port + 1)
ladex --data-dir DIR   # Where shared files are stored (default: $LADEX_DATA_DIR, else ~/.ladex/files)
ladex <command> --help # send, get, seed, ls, devices, invite, doctor (see Commands)
ladex --storage-limit-gb N   # Most disk this node may use for shared files (default: 20)
ladex --no-keychain    # Keep the node's key in a file in the data folder, not the OS keychain
```

## Build from Source

### Prerequisites

- [Rust](https://rustup.rs/) (latest stable version)

### Building

1. Clone the repository:
```bash
git clone https://github.com/GShreekar/ladex.git
cd ladex
```

2. Build the project:
```bash
cargo build --release
```

3. The binary will be available at `target/release/ladex`

### Development

For development with auto-reload:
```bash
cargo run
```

Run tests:
```bash
cargo test
```

## Contributing

Contributions are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers setting up, the checks CI runs and how the tests are organised. For a large change, open an issue first. Please report security problems privately, as described in [SECURITY.md](SECURITY.md).

## License

This project is licensed under the Apache License 2.0 - see the [LICENSE](LICENSE) file for details.

## Author

**G Shreekar** - [GitHub Profile](https://github.com/GShreekar)

---

For more information, visit the [GitHub repository](https://github.com/GShreekar/ladex) or check out the [latest releases](https://github.com/GShreekar/ladex/releases).