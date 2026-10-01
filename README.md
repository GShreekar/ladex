# LADEX - Local Area Data Exchange

A fast and secure peer-to-peer file transfer tool built with Rust that enables seamless file sharing over local networks without requiring internet connectivity. LADEX provides a beautiful web interface with passphrase protection for secure transfers.

## Features

- **Passphrase Protection**: One passphrase (generated or your own) gates both the browser login and joining the mesh
- **Zero Configuration**: No complex setup required - just run and share
- **Local Network Only**: All transfers happen over your local network, ensuring privacy and speed
- **Real-time Transfer**: WebSocket-based communication for instant file transfers
- **Chunked Transfer**: Efficient handling of large files with progress tracking
- **Text Messaging**: Send quick text messages between connected peers
- **Folder Support**: Transfer entire directories with automatic compression
- **Cross-Platform**: Works on Linux, macOS, and Windows
- **Web Interface**: Modern, responsive web UI accessible from any browser
- **Session Security**: Server restart invalidates old authentication cookies
- **Brute-Force Resistant**: Wrong guesses are throttled per device, and the passphrase is never sent over the network

## Supported Platforms

- Linux (x86_64, aarch64)
- macOS (x86_64, Apple Silicon)
- Windows (x86_64)

## Installation

### Quick Install

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

### Security Model

- **Mesh handshake**: nodes prove they know the passphrase with a SPAKE2 password-authenticated key exchange. The passphrase, and anything derived from it, never crosses the network, so recording traffic or discovery announcements reveals nothing that can be cracked offline. An attacker gets at most one guess per connection.
- **Man-in-the-middle protection**: the handshake is bound to the TLS certificate each node actually connected to. Someone relaying or terminating the connection with their own certificate can't complete it without the passphrase.
- **Wrong guesses are throttled** at both the browser login and the mesh handshake.
- **Each browser connection is one device.** A connection can only act as the device it joined as, so one signed-in device can't delete another's files or answer another's downloads. The node, not the browser, decides who uploaded a file and who hosts it.
- **Nothing from another device is trusted.** File names, sizes and types are cleaned on the node and again on the receiving browser (no path tricks, no look-alike extensions via invisible characters, no Windows device names). A sender can't write more data than it announced, a received folder never overwrites existing files in the folder you chose (clashes become `name (1).ext`), and a resumed download can only continue from bytes you really have.
- **Clocks can be wrong.** Shared files and devices are ordered with logical clocks, so a device with a wrong clock can't win conflicts or bring back something that was unshared. Updates stamped more than an hour ahead of a device's clock are ignored, and LADEX warns when a peer's clock is more than two minutes off.
- **Open mode** (no passphrase) has none of these protections. Use it only on networks you trust.
- **`--no-tls`** turns off encryption and the man-in-the-middle protection; the passphrase itself is still never sent.

### Basic Operations

1. **Open your browser** and navigate to the server address
2. **Enter the passphrase** (if one is set)
3. **Connect peers** by sharing the URL and passphrase with other devices
4. **Send files** by dragging and dropping or using the file picker
5. **Send folders** by selecting entire directories (automatically zipped)
6. **Send messages** using the text input field
7. **Monitor transfers** with the real-time progress indicators
8. **Logout** when finished (passphrase mode only)

## Testing

```bash
cargo test             # unit tests (merging, sessions, handshake, validation, ...)
node --test tests/js/  # browser-side receive policy; shares test cases with the Rust side
```

## Command Line Options

```bash
ladex [PASSPHRASE]     # Launch with your own passphrase
ladex -s, --secure     # Launch with a generated passphrase
ladex                  # Launch without a passphrase (open access, prints a warning)
ladex --local-port N   # Port for the localhost-only HTTP listener (default: port + 1)
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

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/new-feature`)
3. Commit your changes (`git commit -m 'Add some new feature'`)
4. Push to the branch (`git push origin feature/new-feature`)
5. Open a Pull Request

## License

This project is licensed under the Apache License 2.0 - see the [LICENSE](LICENSE) file for details.

## Author

**G Shreekar** - [GitHub Profile](https://github.com/GShreekar)

---

For more information, visit the [GitHub repository](https://github.com/GShreekar/ladex) or check out the [latest releases](https://github.com/GShreekar/ladex/releases).