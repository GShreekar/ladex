#!/usr/bin/env python3
"""Runs after the join test on the emulator: the phone's checks on the Network page, sharing in the background under a
notification, the node outliving a window Android closed, and the notification's Stop button."""

import json
import subprocess
import sys
import time
from collections.abc import Callable
from pathlib import Path

from android_ui import (
    POLL_SECONDS,
    adb,
    bounds_of,
    find,
    keep_what_was_on_screen,
    screen_nodes,
    screenshot,
    scroll_to_the_top,
    tap,
)

PACKAGE = "io.github.gshreekar.ladex"
KEY_APP_SWITCH = "187"
# Recents animates in before its buttons can be tapped.
RECENTS_SECONDS = 3
WAIT_SECONDS = 60
GOODBYE_SECONDS = 15
# Longer than the app waits off Wi-Fi before pausing, so a lost connection would show as a paused notification.
SETTLE_SECONDS = 15


class CheckFailed(Exception):
    pass


def wait_until(condition: Callable[[], bool], what: str, seconds: int = WAIT_SECONDS) -> None:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if condition():
            return
        time.sleep(POLL_SECONDS)
    raise CheckFailed(f"timed out waiting for {what}")


def expect(is_true: bool, what: str) -> None:
    if not is_true:
        raise CheckFailed(what)


def host_node_count(ladex: str, host_data_dir: str) -> int:
    reply = subprocess.run([ladex, "--data-dir", host_data_dir, "devices", "--json"], capture_output=True, text=True)
    if reply.returncode != 0:
        return 0
    return len(json.loads(reply.stdout)["nodes"])


def is_running() -> bool:
    return subprocess.run(["adb", "shell", "pidof", PACKAGE], capture_output=True).returncode == 0


def is_sharing_in_the_foreground() -> bool:
    services = adb("shell", "dumpsys", "activity", "services", PACKAGE)
    return "SharingService" in services and "isForeground=true" in services


def notifications() -> str:
    return adb("shell", "dumpsys", "notification", "--noredact")


def ladex_log() -> str:
    return adb("logcat", "-d", "-s", "LADEX")


def check_the_network_page(out: Path) -> None:
    tap(text="Network", is_in_bottom_bar=True)
    # The join test left this page scrolled down to the address field.
    scroll_to_the_top()
    tap(text="Check again")
    # The details of the phone's own checks, the Wi-Fi and the multicast lock.
    find(text="connected")
    find(text="held, so Android lets discovery through")
    screenshot(out, "7-network-checks")


def turn_on_sharing_in_the_background(out: Path) -> None:
    tap(text="Settings", is_in_bottom_bar=True)
    scroll_to_the_top()
    tap(text="Keep sharing in background")
    # Asked for at the moment the notification is first needed.
    tap(element_id="permission_allow_button")
    wait_until(is_sharing_in_the_foreground, "the sharing service")
    wait_until(lambda: "Sharing with 1 device" in notifications(), "the notification to show the other node")
    screenshot(out, "8-sharing")


# Clear all in Recents removes the app's task, as swiping its card away does, which is how a person closes an app.
# Its card is not always centred, so the button is found by its id rather than swiping at a guessed spot.
def close_from_recents(out: Path) -> None:
    adb("shell", "input", "keyevent", KEY_APP_SWITCH)
    time.sleep(RECENTS_SECONDS)
    screenshot(out, "9-recents")
    # Recents has no bottom navigation to keep clear of.
    tap(element_id="clear_all", is_in_bottom_bar=True)


def close_the_window_while_sharing(ladex: str, host_data_dir: str, out: Path) -> None:
    close_from_recents(out)
    wait_until(lambda: "keeps sharing in the background" in ladex_log(), "the node to outlive its window")
    time.sleep(SETTLE_SECONDS)
    expect(is_running(), "LADEX stopped after Android closed its window")
    expect(is_sharing_in_the_foreground(), "the sharing service stopped after Android closed the window")
    expect(host_node_count(ladex, host_data_dir) > 0, "the other node lost the phone after Android closed the window")
    adb("shell", "am", "start", "-n", f"{PACKAGE}/.MainActivity")
    find(text="Network", is_in_bottom_bar=True)
    screenshot(out, "10-reopened")


# A notification can show collapsed, with its buttons hidden; its own Expand button opens it.
def expand_the_notification() -> None:
    _, title_top, _, title_bottom = bounds_of(find(text="LADEX"))
    for node in screen_nodes():
        _, top, _, bottom = bounds_of(node)
        is_in_its_row = top <= title_top and title_bottom <= bottom
        if node.get("content-desc") == "Expand" and is_in_its_row:
            left, top, right, bottom = bounds_of(node)
            adb("shell", "input", "tap", str((left + right) // 2), str((top + bottom) // 2))
            time.sleep(POLL_SECONDS)
            return


def stop_from_the_notification(ladex: str, host_data_dir: str, out: Path) -> None:
    adb("shell", "cmd", "statusbar", "expand-notifications")
    time.sleep(POLL_SECONDS)
    expand_the_notification()
    screenshot(out, "11-notification")
    # The notification shade has no bottom navigation to keep clear of.
    tap(text="Stop", is_in_bottom_bar=True)
    wait_until(lambda: not is_running(), "LADEX to quit")
    wait_until(lambda: host_node_count(ladex, host_data_dir) == 0, "the other node to be told goodbye", GOODBYE_SECONDS)


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: android-background-test.py <ladex> <host data dir> <output folder>", file=sys.stderr)
        return 2
    ladex, host_data_dir, out_folder = sys.argv[1:]
    out = Path(out_folder)
    out.mkdir(parents=True, exist_ok=True)
    try:
        check_the_network_page(out)
        turn_on_sharing_in_the_background(out)
        close_the_window_while_sharing(ladex, host_data_dir, out)
        stop_from_the_notification(ladex, host_data_dir, out)
    except (TimeoutError, CheckFailed) as failure:
        print(f"::error::{failure}")
        keep_what_was_on_screen(out)
        (out / "services.txt").write_text(adb("shell", "dumpsys", "activity", "services", PACKAGE))
        (out / "notifications.txt").write_text(notifications())
        return 1
    finally:
        (out / "logcat-background.log").write_text(ladex_log())
    print("The phone reported its network, shared in the background, outlived its window and quit from the notification")
    return 0


if __name__ == "__main__":
    sys.exit(main())
