# LADEX - Local Area Data Exchange

A fast and secure peer-to-peer file transfer tool built with Rust that enables seamless file sharing over local networks without requiring internet connectivity. LADEX provides a simple command-line interface for transferring files and folders between devices on the same network.

## Features

- **Zero Configuration**: No complex setup required - just run and share
- **Local Network Only**: All transfers happen over your local network, ensuring privacy and speed
- **Real-time Transfer**: WebSocket-based communication for instant file transfers
- **Chunked Transfer**: Efficient handling of large files with progress tracking
- **Text Messaging**: Send quick text messages between connected peers
- **Folder Support**: Transfer entire directories with automatic compression
- **Cross-Platform**: Works on Linux, macOS, and Windows
- **Web Interface**: Modern, responsive web UI accessible from any browser
- **Secure**: All data stays within your local network boundary

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

Launch LADEX on the host machine:

```bash
ladex
```

The server will start on `http://localhost:8080` by default. Other devices on your network can connect using your local IP address (e.g., `http://192.168.1.100:8080`).

### Basic Operations

1. **Open your browser** and navigate to the server address
2. **Connect peers** by opening the same URL on other devices
3. **Send files** by dragging and dropping or using the file picker
4. **Send folders** by selecting entire directories (automatically zipped)
5. **Send messages** using the text input field
6. **Monitor transfers** with the real-time progress indicators

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