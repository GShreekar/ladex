#!/usr/bin/env python3
"""Drives a freshly installed LADEX on the emulator through the first-run wizard, joins a mesh, and adds a node by address."""

import json
import subprocess
import sys
import time
from pathlib import Path

from android_ui import POLL_SECONDS, adb, find, keep_what_was_on_screen, press_enter, screenshot, tap, type_into

# The emulator reaches the machine it runs on at this address.
HOST_FROM_EMULATOR = "10.0.2.2"
JOIN_TIMEOUT_SECONDS = 90


def host_sees_a_node(ladex: str, host_data_dir: str) -> bool:
    reply = subprocess.run([ladex, "--data-dir", host_data_dir, "devices", "--json"], capture_output=True, text=True)
    if reply.returncode != 0:
        return False
    return len(json.loads(reply.stdout)["nodes"]) > 0


def finish_the_wizard(out: Path, passphrase: str) -> None:
    find(text="Welcome to LADEX")
    type_into("device-name", "CI phone")
    screenshot(out, "1-welcome")
    press_enter()
    tap(text="Join an existing mesh")
    type_into("mesh-passphrase", passphrase)
    screenshot(out, "2-mesh")
    press_enter()
    find(text="Saving files")
    screenshot(out, "3-files")
    tap(text="Continue")
    find(text="Start LADEX")
    screenshot(out, "4-finish")
    tap(text="Start LADEX")


def add_host_by_address(out: Path, host_port: str) -> None:
    # The window reloads onto the running app once the wizard has started the node.
    tap(text="Network", is_in_bottom_bar=True)
    type_into("peer-address", f"{HOST_FROM_EMULATOR}:{host_port}")
    press_enter()
    time.sleep(POLL_SECONDS)
    screenshot(out, "5-network")


def wait_for_the_join(ladex: str, host_data_dir: str) -> bool:
    deadline = time.monotonic() + JOIN_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        if host_sees_a_node(ladex, host_data_dir):
            return True
        time.sleep(POLL_SECONDS)
    return False


def main() -> int:
    if len(sys.argv) != 6:
        print("usage: android-join-test.py <ladex> <host data dir> <host port> <passphrase> <output folder>", file=sys.stderr)
        return 2
    ladex, host_data_dir, host_port, passphrase, out_folder = sys.argv[1:]
    out = Path(out_folder)
    out.mkdir(parents=True, exist_ok=True)
    try:
        finish_the_wizard(out, passphrase)
        add_host_by_address(out, host_port)
    except TimeoutError:
        # What was on screen when a step was not found is what tells why.
        keep_what_was_on_screen(out)
        raise
    has_joined = wait_for_the_join(ladex, host_data_dir)
    screenshot(out, "6-after-join")
    (out / "logcat-after-join.log").write_text(adb("logcat", "-d", "-s", "LADEX"))
    if not has_joined:
        print("::error::The phone finished the wizard but never joined the node it was given by address")
        return 1
    print("The phone finished the wizard and joined the mesh by address")
    return 0


if __name__ == "__main__":
    sys.exit(main())
