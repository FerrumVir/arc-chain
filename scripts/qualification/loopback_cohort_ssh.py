#!/usr/bin/env python3
"""Create a private, unprivileged loopback SSH endpoint for cohort tests.

Private host/client keys live in an ephemeral mode-0700 directory under the
current user's home directory. Only the host public key is written to the
qualification output directory, as the exact known_hosts pin consumed by the
production SSH client. This helper is Linux-only because its caller runs in CI.
"""
from __future__ import annotations

import os
import argparse
import json
from pathlib import Path
import pwd
import re
import shlex
import shutil
import signal
import socket
import tempfile
import time


class LoopbackCohortSsh:
    def __init__(self, children, output_dir, env):
        self.children = children
        self.output_dir = Path(output_dir).resolve()
        self.base_env = dict(env)
        self.environment = None
        self.ssh_program = None
        self.target = None
        self.known_hosts = None
        self._private_dir = None
        self._agent = None
        self._sshd = None
        self._closed = False
        self._port = None
        self._processes = []
        self._ssh_binary = None
        self._ssh_wrapper = None

    def start(self):
        if self._private_dir is not None:
            raise RuntimeError("loopback SSH helper was already started")
        if os.name != "posix" or not Path("/proc").is_dir():
            raise RuntimeError("loopback SSH qualification requires Linux")
        if not hasattr(os, "geteuid") or os.geteuid() == 0:
            raise RuntimeError("loopback sshd must run as an unprivileged user")
        if not self.output_dir.is_dir():
            raise RuntimeError("qualification output directory must already exist")
        if not Path("/run/sshd").is_dir():
            raise RuntimeError("sshd runtime directory /run/sshd is missing")
        tools = {name: shutil.which(name) for name in ("ssh", "sshd", "ssh-keygen", "ssh-agent", "ssh-add")}
        missing = [name for name, path in tools.items() if path is None]
        if missing:
            raise RuntimeError("missing OpenSSH tools: " + ", ".join(missing))

        account = pwd.getpwuid(os.geteuid())
        home = Path(account.pw_dir).resolve()
        supplied_home = self.base_env.get("HOME")
        if not supplied_home or Path(supplied_home).resolve() != home or not home.is_dir():
            raise RuntimeError("HOME must be the current user's real home directory")
        user = account.pw_name
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", user):
            raise RuntimeError("current account name is not safe for sshd configuration")
        self._private_dir = Path(tempfile.mkdtemp(prefix=".arc-loopback-ssh-", dir=home))
        os.chmod(self._private_dir, 0o700)
        host_key = self._private_dir / "host_ed25519"
        client_key = self._private_dir / "client_ed25519"
        sshd_config = self._private_dir / "sshd_config"
        authorized_keys = self._private_dir / "authorized_keys"
        agent_socket = self._private_dir / "agent.sock"
        if len(os.fsencode(agent_socket)) >= 100:
            raise RuntimeError("private SSH agent socket path is too long")

        self._start_child("loopback-host-keygen", [tools["ssh-keygen"], "-q", "-t", "ed25519",
                           "-N", "", "-f", str(host_key)], self.base_env)
        # Children.wait is deliberately used so tool failures are recorded in
        # the same evidence stream as model/coordinator processes.
        host_key_process = self.children.items[-1][0]
        self.children.wait(host_key_process, 15)
        os.chmod(host_key, 0o600)
        self._start_child("loopback-client-keygen", [tools["ssh-keygen"], "-q", "-t", "ed25519",
                           "-N", "", "-f", str(client_key)], self.base_env)
        client_key_process = self.children.items[-1][0]
        self.children.wait(client_key_process, 15)
        os.chmod(client_key, 0o600)

        public_key = client_key.with_suffix(".pub").read_text(encoding="ascii").strip()
        if not public_key.startswith("ssh-ed25519 ") or "\n" in public_key:
            raise RuntimeError("ssh-keygen produced an invalid client public key")
        authorized_keys.write_text(
            "no-agent-forwarding,no-port-forwarding,no-pty,no-X11-forwarding " + public_key + "\n",
            encoding="ascii",
        )
        os.chmod(authorized_keys, 0o600)

        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reservation:
            reservation.bind(("127.0.0.1", 0))
            self._port = reservation.getsockname()[1]

        config_values = (str(host_key), str(self._private_dir / "sshd.pid"), str(authorized_keys))
        if any(not re.fullmatch(r"[A-Za-z0-9_./-]+", value) for value in config_values):
            raise RuntimeError("private path contains characters unsafe for sshd configuration")
        sshd_config.write_text(
            "\n".join((
                f"Port {self._port}",
                "AddressFamily inet",
                "ListenAddress 127.0.0.1",
                f"HostKey {host_key}",
                f"PidFile {self._private_dir / 'sshd.pid'}",
                f"AuthorizedKeysFile {authorized_keys}",
                "StrictModes yes",
                "PubkeyAuthentication yes",
                "PasswordAuthentication no",
                "KbdInteractiveAuthentication no",
                "PermitRootLogin no",
                "UsePAM no",
                f"AllowUsers {user}",
                "AllowTcpForwarding no",
                "PermitTunnel no",
                "PermitUserEnvironment no",
                "X11Forwarding no",
                "PrintMotd no",
                "LogLevel ERROR",
                "",
            )),
            encoding="ascii",
        )
        os.chmod(sshd_config, 0o600)

        public_host = host_key.with_suffix(".pub").read_text(encoding="ascii").split()
        if len(public_host) < 2 or public_host[0] != "ssh-ed25519":
            raise RuntimeError("ssh-keygen produced an invalid host public key")
        self.known_hosts = self.output_dir / "loopback_known_hosts"
        if self.known_hosts.exists():
            raise RuntimeError("refusing to overwrite loopback_known_hosts")
        if not re.fullmatch(r"/[A-Za-z0-9_./-]+", str(self.known_hosts)):
            raise RuntimeError("known_hosts path contains characters unsafe for OpenSSH options")
        self.known_hosts.write_text(self.known_hosts_record(self._port, " ".join(public_host)), encoding="ascii")
        os.chmod(self.known_hosts, 0o600)

        # Keep the real account HOME. A private wrapper suppresses file-based
        # identities; the isolated SSH_AUTH_SOCK contains only this test key.
        self.environment = dict(self.base_env, SSH_AUTH_SOCK=str(agent_socket))
        self._agent = self._start_child("loopback-ssh-agent",
            [tools["ssh-agent"], "-D", "-a", str(agent_socket)], self.environment)
        self._wait_for_socket(agent_socket, self._agent, 10)
        self._start_child("loopback-ssh-add", [tools["ssh-add"], str(client_key)], self.environment)
        self.children.wait(self.children.items[-1][0], 10)

        self._sshd = self._start_child("loopback-sshd", [tools["sshd"], "-D", "-e", "-f",
                                             str(sshd_config)], self.environment)
        self._wait_for_listener(self._port, self._sshd, 10)
        self._ssh_binary = Path(tools["ssh"]).resolve()
        self._ssh_wrapper = self._private_dir / "ssh-client"
        self._ssh_wrapper.write_text(
            "#!/bin/sh\nexec " + shlex.quote(str(self._ssh_binary))
            + ' -oIdentityFile=none -oIdentitiesOnly=no "$@"\n',
            encoding="ascii",
        )
        os.chmod(self._ssh_wrapper, 0o700)
        self.ssh_program = self._ssh_wrapper
        self.target = f"ssh://{user}@127.0.0.1:{self._port}"
        probe = self._start_child("loopback-ssh-auth-probe", [
            str(self.ssh_program), "-F", "none", "-oBatchMode=yes", "-oStrictHostKeyChecking=yes",
            f"-oUserKnownHostsFile={self.known_hosts}", "-oGlobalKnownHostsFile=/dev/null",
            "-oUpdateHostKeys=no", "-oCheckHostIP=no", "-oControlPath=none",
            "-oPasswordAuthentication=no", "-oConnectTimeout=5", "--", self.target, "true",
        ], self.environment)
        self.children.wait(probe, 10)
        return self

    def _start_child(self, name, command, environment):
        process = self.children.start(name, command, environment)
        self._processes.append(process)
        return process

    @staticmethod
    def _wait_for_socket(path, process, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError("ssh-agent exited before creating its private socket")
            if path.exists():
                return
            time.sleep(0.05)
        raise TimeoutError("ssh-agent did not create its socket before deadline")

    @staticmethod
    def _wait_for_listener(port, process, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError("loopback sshd exited before listening")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    return
            except OSError:
                time.sleep(0.05)
        raise TimeoutError("loopback sshd did not listen before deadline")

    def evidence(self):
        return {
            "mode": "ephemeral-unprivileged-loopback-sshd",
            "bind_address": "127.0.0.1",
            "port": self._port,
            "target": self.target,
            "known_hosts": str(self.known_hosts) if self.known_hosts else None,
            "ssh_program": str(self.ssh_program) if self.ssh_program else None,
            "private_keys_written_to_output": False,
            "private_key_material_recorded": False,
        }

    @staticmethod
    def known_hosts_record(port, public_key):
        parts = public_key.split()
        if (not 1 <= port <= 65535 or len(parts) < 2 or parts[0] != "ssh-ed25519"
                or "\n" in public_key or "\r" in public_key):
            raise ValueError("invalid loopback host key or port")
        return f"[127.0.0.1]:{port} {parts[0]} {parts[1]}\n"

    def close(self):
        if self._closed:
            return
        self._closed = True
        processes = list(self._processes)
        failure = None
        if processes:
            try:
                self.children.stop(processes, timeout=5)
            except Exception as error:  # cleanup must still remove private material
                failure = error
        if any(process.poll() is None for process in processes):
            failure = failure or RuntimeError("loopback SSH child remained alive after bounded cleanup")
        if any(getattr(process, "returncode", None) != 0 for process in processes):
            failure = failure or RuntimeError("loopback SSH daemon required forced or unsuccessful shutdown")
        if self._private_dir is not None:
            shutil.rmtree(self._private_dir, ignore_errors=False)
            self._private_dir = None
        if failure:
            raise failure

    def __enter__(self):
        return self.start()

    def __exit__(self, exc_type, exc, traceback):
        self.close()
        return False


def smoke_main(argv=None):
    """Exercise generated host pin, isolated agent authentication and teardown."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--smoke-output-dir", required=True, type=Path)
    args = parser.parse_args(argv)
    output = args.smoke_output_dir
    if not output.is_absolute():
        parser.error("--smoke-output-dir must be absolute")
    output.mkdir(mode=0o700, parents=True, exist_ok=False)

    # Use the same child supervisor/evidence format as the qualification runner.
    from run_low_residency_conformance import Children, require_clean_exits, write_json

    base_env = dict(os.environ)
    children = Children(output, base_env)
    helper = LoopbackCohortSsh(children, output, base_env)
    result = {"schema": "arc.loopback-cohort-ssh-smoke.v1", "pass": False,
              "scope": "local SSH transport setup/authentication/cleanup only; no model or cohort work"}
    old_handlers = {}

    def interrupted(signum, _frame):
        raise InterruptedError(f"loopback SSH smoke interrupted by signal {signum}")

    try:
        for sig in (signal.SIGTERM, signal.SIGINT):
            old_handlers[sig] = signal.signal(sig, interrupted)
        helper.start()
        result["transport"] = helper.evidence()
        helper.close()
        children.close()
        require_clean_exits(children.records)
        result["children"] = children.records
        result["pass"] = True
    except BaseException as error:
        result["error"] = f"{type(error).__name__}: {error}"
        try:
            helper.close()
        except BaseException as cleanup_error:
            result["cleanup_error"] = f"{type(cleanup_error).__name__}: {cleanup_error}"
        try:
            children.close()
        except BaseException as cleanup_error:
            result["children_cleanup_error"] = f"{type(cleanup_error).__name__}: {cleanup_error}"
        result["children"] = children.records
    finally:
        for sig, handler in old_handlers.items():
            signal.signal(sig, handler)
        write_json(output / "smoke.json", result)
    print(json.dumps({"pass": result["pass"], "error": result.get("error")}))
    return 0 if result["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(smoke_main())
