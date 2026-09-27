#!/usr/bin/env python3
"""Process regression for streaming small binary replies through the real relay."""
from __future__ import annotations
import argparse
import os
from pathlib import Path
import select
import socket
import subprocess
import tempfile
import time


def read_exact(fd: int, size: int, timeout: float) -> bytes:
    deadline = time.monotonic() + timeout
    result = bytearray()
    while len(result) < size:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
            raise TimeoutError(f"relay stdout did not deliver {size} bytes before socket EOF")
        chunk = os.read(fd, size - len(result))
        if not chunk:
            raise EOFError(f"relay stdout ended after {len(result)} of {size} bytes")
        result.extend(chunk)
    return bytes(result)


def read_socket_exact(stream: socket.socket, size: int, timeout: float) -> bytes:
    stream.settimeout(timeout)
    result = bytearray()
    while len(result) < size:
        chunk = stream.recv(size - len(result))
        if not chunk:
            raise EOFError(f"relay socket ended after {len(result)} of {size} bytes")
        result.extend(chunk)
    return bytes(result)


def exercise(binary: Path) -> None:
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError(f"compiled worker is missing or not executable: {binary}")
    with tempfile.TemporaryDirectory(prefix="arc-row-relay-flush-") as temp:
        socket_path = str(Path(temp) / "relay.sock")
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(socket_path)
        listener.listen(1)
        listener.settimeout(5)
        process = subprocess.Popen(
            [str(binary), "relay", "--socket", socket_path],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        peer = None
        try:
            peer, _ = listener.accept()
            request1 = b"request-one\x00binary"
            response1 = (b"\x89\xab\xcd\xef" * 32)  # 128 bytes, no newline
            process.stdin.write(request1)
            if read_socket_exact(peer, len(request1), 5) != request1:
                raise AssertionError("first request bytes changed in relay")
            peer.sendall(response1)
            if read_exact(process.stdout.fileno(), len(response1), 3) != response1:
                raise AssertionError("first response bytes changed or were buffered until EOF")

            request2 = b"request-two\x00binary"
            response2 = b"\xff\xfe\xfd\xfc" * 32  # second frame, still no newline
            process.stdin.write(request2)
            if read_socket_exact(peer, len(request2), 5) != request2:
                raise AssertionError("second request bytes changed in relay")
            peer.sendall(response2)
            if read_exact(process.stdout.fileno(), len(response2), 3) != response2:
                raise AssertionError("second response bytes changed or were buffered until EOF")

            process.stdin.close()  # relay must half-close the socket write side.
            peer.settimeout(5)
            if peer.recv(1) != b"":
                raise AssertionError("relay did not forward stdin EOF to the socket")
            peer.shutdown(socket.SHUT_WR)
            peer.close()
            peer = None
            if process.wait(timeout=5) != 0:
                stderr = process.stderr.read().decode("utf-8", "replace")[-2000:]
                raise RuntimeError(f"relay exited unsuccessfully: {stderr}")
        finally:
            if peer is not None:
                peer.close()
            listener.close()
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            for pipe in (process.stdin, process.stdout, process.stderr):
                if pipe is not None and not pipe.closed:
                    pipe.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    exercise(args.binary.resolve())
    print("relay flush regression PASS: two small binary replies arrived before socket EOF")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
