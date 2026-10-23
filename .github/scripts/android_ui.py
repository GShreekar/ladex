"""Drives LADEX on the emulator through adb and uiautomator, as a person would tap it."""

import re
import subprocess
import time
import xml.etree.ElementTree as ElementTree
from pathlib import Path

SCREEN_TIMEOUT_SECONDS = 60
POLL_SECONDS = 2
KEY_ENTER = "66"
KEY_HOME = "3"
OFF_SCREEN = "[0,0][0,0]"
# The app's bottom navigation covers the lowest part of the screen, so something there cannot be tapped yet.
REACHABLE_SHARE_OF_SCREEN = 0.85
# From the lower middle of a 320x640 screen up to the upper middle, over 300 ms; scrolling up is the reverse.
SCROLL_DOWN_SWIPE = ("160", "450", "160", "200", "300")
SCROLL_UP_SWIPE = ("160", "200", "160", "450", "300")
# Enough swipes to bring any of the app's pages back to its top.
SWIPES_TO_THE_TOP = 6


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


# `find` only scrolls down, so a page another step left scrolled down is brought back to its top first.
def scroll_to_the_top() -> None:
    for _ in range(SWIPES_TO_THE_TOP):
        adb("shell", "input", "swipe", *SCROLL_UP_SWIPE)


def tap(text: str | None = None, element_id: str | None = None, is_in_bottom_bar: bool = False) -> None:
    left, top, right, bottom = bounds_of(find(text, element_id, is_in_bottom_bar))
    adb("shell", "input", "tap", str((left + right) // 2), str((top + bottom) // 2))


def type_into(element_id: str, text: str) -> None:
    tap(element_id=element_id)
    adb("shell", "input", "text", text.replace(" ", "%s"))


def press_enter() -> None:
    adb("shell", "input", "keyevent", KEY_ENTER)


def press_home() -> None:
    adb("shell", "input", "keyevent", KEY_HOME)


def screenshot(out: Path, name: str) -> None:
    (out / f"{name}.png").write_bytes(subprocess.run(["adb", "exec-out", "screencap", "-p"], check=True, capture_output=True).stdout)


def keep_what_was_on_screen(out: Path) -> None:
    screenshot(out, "stuck")
    adb("shell", "uiautomator", "dump", "/sdcard/screen.xml")
    (out / "stuck-screen.xml").write_text(adb("shell", "cat", "/sdcard/screen.xml"))
    (out / "logcat-when-stuck.log").write_text(adb("logcat", "-d"))
