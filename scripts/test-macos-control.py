#!/usr/bin/env python3
"""Live macOS check of rtx-control (#2665) against an owned window (test-control-window.swift).

Protocol checks run on their own: --check, the ready line, permissions (checked, never prompted),
screenshot, status, unknown op and invalid JSON. Input checks then post mouse and keys into the
fixture window only, and only while the fixture is frontmost and key and nobody has touched the
Mac for a minute: click, typing, a shifted key, and that keys and buttons still held are released
when the session ends (stdin closing, `stop`, a failed request). They need Accessibility for the
app running this (System Settings); without it they are skipped, not requested. The clipboard
(paste_text) is not exercised: it would replace the user's clipboard.

Usage: scripts/test-macos-control.py path/to/rtx-control
"""
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SHIFT, LETTER_A, LETTER_B = 0xFFE1, 0x61, 0x62
SHIFT_FLAG = 1 << 17
results = []


def check(name, ok, detail=""):
    results.append((name, ok))
    print(f"{'PASS' if ok else 'FAIL'} {name}{f': {detail}' if detail else ''}", flush=True)


def idle_seconds():
    out = subprocess.run(["ioreg", "-c", "IOHIDSystem"], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if '"HIDIdleTime"' in line:
            return int(line.split("=")[-1]) / 1e9
    return 0.0


class Lines:
    """A child process speaking JSON lines; reads on a thread so waits can time out."""

    def __init__(self, args):
        self.proc = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
        self.lines, self.cond = [], threading.Condition()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            try:
                value = json.loads(line)
            except ValueError:
                value = {"raw": line.rstrip()}
            with self.cond:
                self.lines.append(value)
                self.cond.notify_all()

    def send(self, value):
        self.proc.stdin.write((value if isinstance(value, str) else json.dumps(value)) + "\n")
        self.proc.stdin.flush()

    def wait(self, predicate, timeout=5.0, start=0):
        end = time.time() + timeout
        with self.cond:
            while True:
                for value in self.lines[start:]:
                    if predicate(value):
                        return value
                left = end - time.time()
                if left <= 0:
                    return None
                self.cond.wait(left)


class Control(Lines):
    next_id = 0

    def request(self, op, **fields):
        Control.next_id += 1
        rid = Control.next_id
        self.send({"id": rid, "op": op, **fields})
        return self.wait(lambda v: v.get("id") == rid, timeout=15)


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    binary = os.path.abspath(sys.argv[1])

    check_run = subprocess.run([binary, "--check"], capture_output=True, text=True)
    report = json.loads(check_run.stdout.splitlines()[0])
    check("--check reports rtx-control protocol 1 with input",
          check_run.returncode == 0 and report.get("tool") == "rtx-control"
          and report.get("protocol") == 1 and report.get("input") is True, json.dumps(report))

    control = Control([binary])
    ready = control.wait(lambda v: v.get("ready") is True)
    check("ready line", bool(ready) and ready.get("backend") == "macos", json.dumps(ready))
    perms = control.request("permissions")
    check("permissions answers without prompting", bool(perms) and perms.get("ok"), json.dumps(perms))
    print(f"     responsible app: {perms.get('app')!r}; screenRecording={perms.get('screenRecording')} "
          f"accessibility={perms.get('accessibility')}")
    if perms.get("screenRecording"):
        shot = control.request("screenshot")
        check("screenshot", bool(shot) and shot.get("ok") and shot.get("mime") == "image/jpeg"
              and shot.get("width", 0) > 0, f"{shot.get('width')}x{shot.get('height')}" if shot else "no reply")
    else:
        print("SKIP screenshot: Screen Recording is not granted to the responsible app")
    status = control.request("status")
    check("status before input", bool(status) and status.get("active") is False, json.dumps(status))
    unknown = control.request("no_such_op")
    check("unknown op is an error", bool(unknown) and unknown.get("ok") is False, json.dumps(unknown))
    mark = len(control.lines)
    control.send("{not json")
    bad = control.wait(lambda v: v.get("error") == "Invalid JSON", start=mark)
    check("invalid JSON is an error line", bool(bad))

    if not perms.get("accessibility"):
        print("SKIP input: Accessibility is not granted to the responsible app (not requested)")
        control.proc.stdin.close()
        control.proc.wait(10)
        return finish()
    if idle_seconds() < 60:
        print(f"SKIP input: the Mac was used {idle_seconds():.0f} s ago; input runs only after 60 s idle")
        control.proc.stdin.close()
        control.proc.wait(10)
        return finish()

    with tempfile.TemporaryDirectory() as tmp:
        fixture_bin = os.path.join(tmp, "test-control-window")
        subprocess.run(["swiftc", "-O", "-o", fixture_bin, os.path.join(HERE, "test-control-window.swift")], check=True)
        fixture = Lines([fixture_bin])
        try:
            fixture.wait(lambda v: v.get("type") == "ready", timeout=10)
            run_input(control, fixture)
        finally:
            fixture.send("quit")
            fixture.proc.wait(10)
    return finish()


def owned(fixture):
    """The fixture's state, only if it is frontmost and key: input then reaches it."""
    for _ in range(20):
        mark = len(fixture.lines)
        fixture.send("state")
        state = fixture.wait(lambda v: v.get("type") == "state", start=mark)
        if state and state["active"] and state["key"]:
            return state
        fixture.send("activate")
        time.sleep(0.25)
    return None


def run_input(control, fixture):
    state = owned(fixture)
    if not state:
        return check("fixture frontmost", False, "the test window never became frontmost and key")
    shot = control.request("screenshot", maxWidth=1920)
    start = control.request("start")
    (w, h), (sw, sh) = start["size"], (shot["width"], shot["height"])
    x, y, pw, ph = state["pad"]
    px, py = (x + pw / 2) * sw / w, (y + ph / 2) * sh / h

    # Posted events reset the idle time themselves, so after the start gate ownership is the guard:
    # keys go to the key window, and clicks to the point this test moved to, inside the pad.
    def guarded(op, **fields):
        if not owned(fixture):
            raise SystemExit("ABORT: the test window lost focus; nothing more is posted")
        return control.request(op, **fields)

    def received(predicate, mark):
        return fixture.wait(predicate, timeout=3, start=mark)

    mark = len(fixture.lines)
    guarded("move", x=px, y=py)
    guarded("button", button="left", pressed=True)
    guarded("button", button="left", pressed=False)
    check("click reaches the window", bool(received(lambda v: v.get("type") == "mouseUp", mark)))

    mark = len(fixture.lines)
    guarded("keysym", keysym=LETTER_A, pressed=True)
    guarded("keysym", keysym=LETTER_A, pressed=False)
    down = received(lambda v: v.get("type") == "keyDown", mark)
    check("typing a", bool(down) and down["chars"] == "a" and not down["flags"] & SHIFT_FLAG, json.dumps(down))

    mark = len(fixture.lines)
    guarded("keysym", keysym=SHIFT, pressed=True)
    guarded("keysym", keysym=LETTER_B, pressed=True)
    guarded("keysym", keysym=LETTER_B, pressed=False)
    guarded("keysym", keysym=SHIFT, pressed=False)
    down = received(lambda v: v.get("type") == "keyDown", mark)
    check("shift+b types B", bool(down) and down["chars"] == "B" and down["flags"] & SHIFT_FLAG, json.dumps(down))

    for how in ("stop", "failed request"):
        mark = len(fixture.lines)
        guarded("keysym", keysym=SHIFT, pressed=True)
        held = received(lambda v: v.get("type") == "flags" and v["flags"] & SHIFT_FLAG, mark)
        mark = len(fixture.lines)
        reply = control.request("stop") if how == "stop" else control.request("keysym")
        released = received(lambda v: v.get("type") == "flags" and not v["flags"] & SHIFT_FLAG, mark)
        check(f"held Shift is released on {how}", bool(held) and bool(released), json.dumps(reply))

    # A session that ended (stop, failed request) forgets the pointer: a new one starts at the
    # display's origin, so move into the pad again before pressing a button.
    guarded("move", x=px, y=py)
    guarded("keysym", keysym=SHIFT, pressed=True)
    guarded("button", button="left", pressed=True)
    mark = len(fixture.lines)
    control.proc.stdin.close()
    control.proc.wait(10)
    released = received(lambda v: v.get("type") == "flags" and not v["flags"] & SHIFT_FLAG, mark)
    up = received(lambda v: v.get("type") == "mouseUp", mark)
    check("held Shift and mouse button are released when stdin closes", bool(released) and bool(up))
    check("rtx-control exits 0 after stdin closes", control.proc.returncode == 0, str(control.proc.returncode))


def finish():
    failed = [name for name, ok in results if not ok]
    print(f"{len(results) - len(failed)}/{len(results)} passed" + (f"; failed: {failed}" if failed else ""))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
