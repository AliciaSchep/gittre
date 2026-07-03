"""Drive a TUI in a pty and snapshot its screen with pyte.

Usage: uv run --with pyte dev/tui_drive.py <scenario.json>

scenario.json:
{
  "cmd": ["target/debug/gittre", "-u"],   # argv to spawn in the pty
  "cwd": "/path/to/test/repo",
  "cols": 100, "rows": 30,                # terminal size
  "outdir": "/tmp/snaps",                 # where screen dumps are written
  "env": {"EDITOR": "..."},               # optional extra environment
  "rawlog": "/tmp/raw.bin",               # optional: raw pty bytes (grep for
                                          # ESC[38;2; to prove truecolor)
  "startup_wait": 1.0,                    # optional; seconds before step 1
  "steps": [
    {"send": "jjj", "wait": 0.3, "snap": "01-scrolled"},
    {"send": "<ESC>"},                    # tokens: <ESC>, <C-c>
    {"send_raw": "<ESC>[<0;5;10M"},       # atomic write; SGR mouse events:
                                          # <ESC>[<0;COL;ROWM = click
                                          # (1-based), button 64/65 = wheel
    {"shell": "git commit -qam wip"},     # mutate the repo mid-run
                                          # (exercises auto-reload)
    {"resize": [60, 20], "wait": 0.5}
  ]
}

Each step runs shell -> resize -> send_raw -> send (whichever are present),
then waits (default 0.3s), drains output into the emulator, and writes a
plain-text screen dump if "snap" is set. "send" is typed character by
character (20ms apart); use "send_raw" for escape sequences that must arrive
in one write.

Known pyte limits: no alternate-screen buffer (suspend/resume flows like the
$EDITOR handoff show ghost rows), text dumps carry no styling, and some emoji
widths crash it — one reason gittre's UI sticks to single-width markers.
"""

import fcntl
import json
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time

import pyte


RAWLOG = None


def drain(master, stream, timeout=0.05):
    while True:
        r, _, _ = select.select([master], [], [], timeout)
        if not r:
            return
        try:
            data = os.read(master, 65536)
        except OSError:
            return
        if not data:
            return
        if RAWLOG:
            RAWLOG.write(data)
        stream.feed(data)


def set_winsize(fd, cols, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def main():
    with open(sys.argv[1]) as f:
        scenario = json.load(f)
    if scenario.get("rawlog"):
        global RAWLOG
        RAWLOG = open(scenario["rawlog"], "wb")

    cols = scenario.get("cols", 100)
    rows = scenario.get("rows", 30)
    outdir = scenario["outdir"]
    os.makedirs(outdir, exist_ok=True)

    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)

    master, slave = pty.openpty()
    set_winsize(slave, cols, rows)
    proc = subprocess.Popen(
        scenario["cmd"],
        cwd=scenario.get("cwd"),
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env={**os.environ, "TERM": "xterm-256color", **scenario.get("env", {})},
        start_new_session=True,
    )
    os.close(slave)

    time.sleep(scenario.get("startup_wait", 1.0))
    drain(master, stream)

    for step in scenario["steps"]:
        if "shell" in step:
            subprocess.run(step["shell"], shell=True, cwd=scenario.get("cwd"), check=True)
        if "resize" in step:
            c, r = step["resize"]
            set_winsize(master, c, r)
            screen.resize(r, c)
            proc.send_signal(signal.SIGWINCH)
        if step.get("send_raw"):
            # One atomic write, for escape sequences (mouse events etc.).
            os.write(master, step["send_raw"].replace("<ESC>", "\x1b").encode())
        if step.get("send"):
            for ch in step["send"].replace("<C-c>", "\x03").replace("<ESC>", "\x1b"):
                os.write(master, ch.encode())
                time.sleep(0.02)
        time.sleep(step.get("wait", 0.3))
        drain(master, stream)
        if step.get("snap"):
            with open(os.path.join(outdir, step["snap"] + ".txt"), "w") as f:
                f.write("\n".join(screen.display))

    time.sleep(0.3)
    drain(master, stream)
    exited = proc.poll()
    if exited is None:
        proc.kill()
        proc.wait()
        print("RESULT: process still running at end (killed)")
    else:
        print(f"RESULT: exit code {exited}")
    os.close(master)


if __name__ == "__main__":
    main()
