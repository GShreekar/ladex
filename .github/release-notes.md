## Install

| System | Download | Notes |
|---|---|---|
| Windows | `LADEX_*_x64-setup.exe` | Installs for all users, puts `ladex` on PATH and adds the firewall rules. |
| macOS (Apple silicon) | `LADEX_*_aarch64.dmg` | Drag LADEX to Applications. On first run it offers to put `ladex` on PATH. |
| macOS (Intel) | `LADEX_*_x64.dmg` | As above. |
| Linux (Debian, Ubuntu) | `LADEX_*_amd64.deb` | `sudo apt install ./LADEX_*_amd64.deb`; installs `ladex` too. |
| Linux (any) | `LADEX_*_amd64.AppImage` | `chmod +x` it and run it. |
| Android (most phones) | `ladex-*-android-arm64.apk` | Open it on the phone and allow installing from this source when asked. Android 7 or newer. |
| Android (older phones) | `ladex-*-android-armv7.apk` | For phones whose processor is 32-bit; try arm64 first. |
| Android (emulator) | `ladex-*-android-x86_64.apk` | For the Android emulator on a PC. |
| Command line only | `ladex-*.tar.gz` / `ladex-*.zip` | Or `curl -sSL https://raw.githubusercontent.com/GShreekar/ladex/main/install.sh \| sh`. |

`SHA256SUMS.txt` lists the checksum of every file.

### These builds are not signed yet

- **Windows:** SmartScreen says "Windows protected your PC". Choose **More info**, then **Run anyway**.
- **macOS:** the first open says LADEX "cannot be verified". Open **System Settings → Privacy & Security**, choose **Open Anyway** next to LADEX, and confirm. When asked, allow LADEX to find devices on your local network.
