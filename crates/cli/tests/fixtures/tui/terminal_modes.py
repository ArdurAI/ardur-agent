"""Hermetic Unix PTY proof: activation, raw mode, orderly cleanup."""
import errno
import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
before = termios.tcgetattr(slave)
child = subprocess.Popen([sys.argv[1], "chat"], stdin=slave, stdout=slave, stderr=slave)
output = bytearray()

def read_until(predicate, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        ready, _, _ = select.select([master], [], [], 0.05)
        if ready:
            try:
                data = os.read(master, 65536)
            except OSError as exc:
                if exc.errno == errno.EIO:
                    break
                raise
            if not data:
                break
            output.extend(data)
    assert predicate(), "expected terminal transition absent: " + repr(bytes(output[-1200:]))

try:
    read_until(lambda: b"\x1b[?1049h" in output and b"CHAT" in output)
    assert b"offline stub" in output, "offline mode must be visible even at 80 columns"
    raw = termios.tcgetattr(slave)
    assert not raw[3] & (termios.ICANON | termios.ECHO), "full-screen input must be raw"
    os.write(master, b"/help\r")
    read_until(lambda: b"Approval prompts only" in output)
    os.write(master, b"local denied turn\r")
    read_until(lambda: b"Denied: policy" in output)
    os.write(master, b"\x04")
    read_until(lambda: b"\x1b[?1049l" in output and b"\x1b[?2004l" in output)
    assert child.wait(timeout=10) == 0
    assert termios.tcgetattr(slave) == before, "must restore original terminal attributes"
    for sequence in [b"\x1b[?1049h", b"\x1b[?1049l", b"\x1b[?2004h", b"\x1b[?2004l", b"\x1b[?25l", b"\x1b[?25h"]:
        assert sequence in output, repr(sequence)
    print("terminal modes restored")
finally:
    if child.poll() is None:
        child.kill()
        child.wait(timeout=10)
    os.close(master)
    os.close(slave)
