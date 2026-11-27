"""Inspect or invoke the native AT-SPI tree of one named process."""
import argparse
import json
import sys
import time

import pyatspi
from gi.repository import GLib


def walk(node, depth=0):
    yield node, depth
    for child in node:
        yield from walk(child, depth + 1)


def describe(node, depth):
    entry = {
        "depth": depth,
        "role": node.getRoleName(),
        "name": node.name or "",
        "description": node.description or "",
        "focused": node.getState().contains(pyatspi.STATE_FOCUSED),
    }
    try:
        entry["attributes"] = node.getAttributes()
    except (RuntimeError, NotImplementedError):
        entry["attributes"] = []
    return entry


def matching_nodes(pid):
    for app in pyatspi.Registry.getDesktop(0):
        if app.get_process_id() == pid:
            yield from walk(app)


def activate(node):
    try:
        actions = node.queryAction()
    except NotImplementedError:
        return False
    for index in range(actions.nActions):
        if actions.getName(index).lower() in {"click", "press", "activate"}:
            return actions.doAction(index)
    return False


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("pid", type=int)
parser.add_argument("action", choices=["dump", "focus", "find", "click"])
parser.add_argument("name", nargs="?", default="")
parser.add_argument("--role")
parser.add_argument("--occurrence", type=int, default=1)
parser.add_argument("--timeout", type=float, default=7)
args = parser.parse_args()
deadline = time.monotonic() + args.timeout
while True:
    try:
        nodes = list(matching_nodes(args.pid))
        if args.action == "dump" and nodes:
            print(json.dumps([describe(node, depth) for node, depth in nodes], indent=2))
            break
        occurrence = 0
        for node, depth in nodes:
            if args.role and args.role != node.getRoleName():
                continue
            if args.action == "focus" and not node.getState().contains(pyatspi.STATE_FOCUSED):
                continue
            if args.action in ("find", "click") and not (node.name or "").startswith(args.name):
                continue
            occurrence += 1
            if occurrence != args.occurrence:
                continue
            if args.action == "click" and not activate(node):
                continue
            if args.action != "dump":
                print(json.dumps(describe(node, depth)))
                sys.exit(0)
    except GLib.Error as error:
        if "Unknown object" not in str(error):
            raise
    if time.monotonic() >= deadline:
        sys.exit(f"{args.action}: no matching node for PID {args.pid} ({args.name!r})")
    time.sleep(0.15)
