#!/usr/bin/env python3
"""Run a command on a pseudo-terminal and type one answer at its prompt (tests only).

The Proof Kit asks for consent on the terminal itself (/dev/tty), never on
standard input, so CI needs a real terminal to say yes or no:

    python3 scripts/proof-kit/pty_answer.py --answer yes -- arc-modern proof --submit ...

Echoes everything the command prints, types ANSWER and Enter once the text
"Type yes to send" appears, and exits with the command's exit code. Linux
and macOS only (Python's pty module).
"""

from __future__ import annotations

import argparse
import os
import pty
import sys
from typing import List

PROMPT = b"Type yes to send"


def main(argv: List[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--answer", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("no command given")
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(command[0], command)
    seen = b""
    answered = False
    while True:
        try:
            data = os.read(fd, 4096)
        except OSError:
            break
        if not data:
            break
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()
        seen = (seen + data)[-4096:]
        if not answered and PROMPT in seen:
            os.write(fd, args.answer.encode() + b"\n")
            answered = True
    _, status = os.waitpid(pid, 0)
    code = os.WEXITSTATUS(status) if os.WIFEXITED(status) else 1
    if not answered:
        sys.stderr.write("pty_answer: the consent prompt never appeared\n")
    return code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
