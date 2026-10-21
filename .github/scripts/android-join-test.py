#!/usr/bin/env python3
"""Drives a freshly installed LADEX on the emulator through the first-run wizard, joins a mesh, and adds a node by address."""

import json
import re
import subprocess
import sys
import time
import xml.etree.ElementTree as ElementTree
from pathlib import Path

# The emulator reaches the machine it runs on at this address.
HOST_FROM_EMULATOR = "10.0.2.2"
SCREEN_TIMEOUT_SECONDS = 60
JOIN_TIMEOUT_SECONDS = 90
POLL_SECONDS = 2
KEY_ENTER = "66"
OFF_SCREEN = "[0,0][0,0]"
# The app's bottom navigation covers the lowest part of the screen, so something there cannot be tapped yet.
REACHABLE_SHARE_OF_SCREEN = 0.85
# From the lower middle of a 320x640 screen up to the upper middle, over 300 ms.
SCROLL_DOWN_SWIPE = ("160", "450", "160", "200", "300")


def adb(*arguments: str) -> str:
    return subprocess.run(["adb", *arguments], check=True, capture_output=True, text=True).stdout


def screen_nodes() -> list[ElementTree.Element]:
    adb("shell", "uiautomator", "dump", "/sdcard/screen.xml")
    return list(ElementTree.fromstring(adb("shell", "cat", "/sdcard/screen.xml")).iter("node"))


# A button's accessible name can carry its subtitle too ("Join an existing mesh Another device is..."), so text matches a start.
def matches(node: ElementTree.Element, text: str | None, element_id: str | None) -> bool:
    if element_id is not None:
        return node.get("resource-id", "").endswith(element_id)
    return any((label or "").startswith(text) for label in (node.get("text"), node.get("content-desc")))


def bounds_of(node: ElementTree.Element) -> tuple[int, int, int, int]:
    left, top, right, bottom = map(int, re.findall(r"\d+", node.get("bounds", "")))
    return left, top, right, bottom


def is_reachable(node: ElementTree.Element, screen_height: int, is_in_bottom_bar: bool) -> bool:
    if node.get("bounds") == OFF_SCREEN:
        return False
    _, top, _, bottom = bounds_of(node)
    return is_in_bottom_bar or (top + bottom) // 2 < screen_height * REACHABLE_SHARE_OF_SCREEN


# Below the fold, or under the bottom navigation, an element cannot be tapped; a person would scroll down to it.
def find(text: str | None = None, element_id: str | None = None, is_in_bottom_bar: bool = False) -> ElementTree.Element:
    deadline = time.monotonic() + SCREEN_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        nodes = screen_nodes()
        screen_height = max(bounds_of(node)[3] for node in nodes)
        found = [node for node in nodes if matches(node, text, element_id)]
        reachable = [node for node in found if is_reachable(node, screen_height, is_in_bottom_bar)]
        if reachable:
            return reachable[0]
        if found:
            adb("shell", "input", "swipe", *SCROLL_DOWN_SWIPE)
        time.sleep(POLL_SECONDS)
    raise TimeoutError(f"nothing on screen matched text={text!r} id={element_id!r}")


def tap(text: str | None = None, element_id: str | None = None, is_in_bottom_bar: bool = False) -> None:
    left, top, right, bottom = bounds_of(find(text, element_id, is_in_bottom_bar))
    adb("shell", "input", "tap", str((left + right) // 2), str((top + bottom) // 2))


def type_into(element_id: str, text: str) -> None:
    tap(element_id=element_id)
    adb("shell", "input", "text", text.replace(" ", "%s"))


def press_enter() -> None:
    adb("shell", "input", "keyevent", KEY_ENTER)


def screenshot(out: Path, name: str) -> None:
    (out / f"{name}.png").write_bytes(subprocess.run(["adb", "exec-out", "screencap", "-p"], check=True, capture_output=True).stdout)


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
        screenshot(out, "stuck")
        (out / "stuck-screen.xml").write_text(adb("shell", "cat", "/sdcard/screen.xml"))
        (out / "logcat-when-stuck.log").write_text(adb("logcat", "-d"))
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
