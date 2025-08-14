#!/bin/sh
set -e

# Installer for the 'ladex' command-line utility.
# Usage: curl -sSL https://raw.githubusercontent.com/GShreekar/ladex/main/install.sh | sh

# --- Configuration ---
REPO="GShreekar/ladex"
APP_NAME="ladex"
INSTALL_DIR="/usr/local/bin"

# --- Helper Functions ---
echo_color() {
    printf '\033[%sm%s\033[0m\n' "$1" "$2"
}

fail() {
    echo_color "31" "Error: $1" >&2
    exit 1
}

success() {
    echo_color "32" "$1"
}

# --- Main Logic ---
main() {
    # Check for required commands
    command -v curl >/dev/null || fail "Required command 'curl' is not installed."
    command -v tar >/dev/null || fail "Required command 'tar' is not installed."

    printf "Installing %s...\n" "$APP_NAME"

    # Detect OS and Architecture
    os_name=$(uname -s | tr '[:upper:]' '[:lower:]')
    arch=$(uname -m)
    case "$os_name" in
        linux) os_suffix="linux" ;;
        darwin) os_suffix="macos" ;;
        *) fail "Unsupported operating system: $os_name" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch_suffix="x86_64" ;;
        aarch64 | arm64) arch_suffix="aarch64" ;;
        *) fail "Unsupported architecture: $arch" ;;
    esac
    
    target_suffix="${os_suffix}-${arch_suffix}"
    success "Detected system: ${target_suffix}"

    # Get the latest version tag from GitHub API
    api_url="https://api.github.com/repos/${REPO}/releases/latest"
    latest_version_tag=$(curl -sSL "$api_url" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
    [ -z "$latest_version_tag" ] && fail "Could not fetch the latest version tag from GitHub."
    success "Latest version is ${latest_version_tag}"

    # Construct download URL and download
    archive_name="${APP_NAME}-${latest_version_tag}-${target_suffix}.tar.gz"
    download_url="https://github.com/${REPO}/releases/download/${latest_version_tag}/${archive_name}"
    
    printf "Downloading from %s\n" "$download_url"
    tmp_dir=$(mktemp -d)
    curl -fSL "$download_url" -o "${tmp_dir}/${archive_name}" || fail "Failed to download the binary."

    # Install the binary
    SUDO=""
    [ ! -w "$INSTALL_DIR" ] && SUDO="sudo"
    
    printf "Extracting and installing to %s...\n" "$INSTALL_DIR"
    tar -xzf "${tmp_dir}/${archive_name}" -C "$tmp_dir" || fail "Failed to extract the archive."
    
    $SUDO mv "${tmp_dir}/${APP_NAME}" "${INSTALL_DIR}/${APP_NAME}" || fail "Failed to move binary to ${INSTALL_DIR}."
    $SUDO chmod +x "${INSTALL_DIR}/${APP_NAME}" || fail "Failed to set executable permissions."

    # Clean up and verify
    rm -rf "$tmp_dir"
    success "${APP_NAME} was installed successfully to ${INSTALL_DIR}/${APP_NAME}"
    printf "You can now run '%s' from your terminal.\n" "$APP_NAME"
}

main