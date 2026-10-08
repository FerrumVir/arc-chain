#!/usr/bin/env python3
"""Wave 0 Stage B host orchestrator: the whole soak inside a nested KVM x86_64 Ubuntu VM.

THROWAWAY LAB FILE (never merged). Runs on a GitHub-hosted ubuntu-24.04 runner. Everything that
touches the v0.7 install happens INSIDE the guest VM, so a real `systemctl reboot` of the guest is
possible, its disk survives it (qcow2), and the runner agent can never be cut by the iptables rules.

Subcommands
  preflight   fail fast when /dev/kvm is absent; make it usable; install qemu
  run         build the VM, install the stranded v0.7.11 headless baseline, consume the launcher, run the
              battery, the real reboot and the soak, then the stop/rollback test; write the evidence
  collect     best effort: pull the guest evidence into the evidence directory (idempotent; also run
              by the workflow after `run`, even when `run` crashed or timed out)

The evidence directory holds exactly what evaluate_stage_b.py reads: config-effective.json, events.jsonl,
commands.jsonl, checks-live.jsonl, samples.jsonl, invariants-before.json, invariants-after.json, plus the
raw guest evidence under guest/.

Launcher source (config stage_b.launcher_source)
  published   scripts/legacy-bridge/canary-consume.sh UNMODIFIED, `--tag v0.7.12 --expect-sha256 <digest>`:
              dry run first, then --apply, against the real GitHub release (the post-G0 Wave 0)
  artifact    REHEARSAL before G0: the same script with its single download line replaced by a copy of the
              launcher bytes of the digest-checked Stage A handoff artifact; labelled as a rehearsal
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import subprocess
import sys
import time
import traceback
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
LAB_DIR = HERE.parent
ROOT = LAB_DIR.parent
sys.path.insert(0, str(LAB_DIR))
import check_config  # noqa: E402
import fetch_handoff  # noqa: E402
import live_ips  # noqa: E402

GUEST_USER = "arcw0"
GUEST_HOME = "/home/arcw0"
GUEST_ARC = GUEST_HOME + "/.arc"
GUEST_OPT = "/opt/arc-w0"
GUEST_LAB = GUEST_OPT + "/lab"
GUEST_WORK = "/var/lib/arc-w0"
SSH_PORT = 2222
X86 = "arc-node-linux-x86_64"
LEGACY_NODE_SHA256 = "1cfc3039786d023cde24ad0b452f35735b39f9e83aaf293e6ed0bf623a11b20c"
LEGACY_TAG = "v0.7.11"
EFFECTIVE_SCHEMA = "arc.legacy-bridge.wave0-lab.stage-b-config.v1"
SSH_OPTS = [
    "-o", "BatchMode=yes",
    "-o", "StrictHostKeyChecking=no",
    "-o", "UserKnownHostsFile=/dev/null",
    "-o", "LogLevel=ERROR",
    "-o", "ConnectTimeout=15",
    "-o", "ServerAliveInterval=15",
    "-o", "ServerAliveCountMax=6",
]
REQUIRED_LIVE_IDS = (
    "L01-kvm",
    "L02-consume-dry-run",
    "L03-interrupt-took-effect",
    "L04-interrupt-resumes",
    "L05-updater-1-noop",
    "L06-updater-2-noop",
    "L07-kickstart-1",
    "L08-kickstart-2",
    "L09-kickstart-3",
    "L10-reboot-boot-id-changed",
    "L11-stop-rollback",
    "L12-pre-post-boot-logs",
)
PRE_FLIP_NOTE = (
    "expected before the Latest flip (v0.7.11 updater compares =, Latest v0.7.11 has no arc-node-linux-x86_64)"
)
CANARY_DOWNLOAD_LINE = re.compile(r"^curl -fL --proto '=https' --tlsv1\.2 -o \"\$staged\" \"\$url\"$", re.MULTILINE)
UPDATER_UNCHANGED_FIELDS = (
    "bin_arc_node_sha256",
    "version_txt",
    "main_pid",
    "proc_start_epoch",
    "node_exe_sha256",
    "address",
    "bridge_node_address",
    "legacy_fingerprint",
)


class Fatal(Exception):
    """A phase failed in a way that makes every later phase meaningless."""


# ----------------------------------------------------------------------------------------------
# Pure helpers (unit-tested offline)
# ----------------------------------------------------------------------------------------------

def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def effective_config(cfg: dict, pins: dict) -> dict:
    sb = cfg["stage_b"]
    profile = sb["profiles"][sb["profile"]]
    node_tag = pins["node_release"]["tag"]
    return {
        "schema": EFFECTIVE_SCHEMA,
        "profile": sb["profile"],
        "launcher_source": sb["launcher_source"],
        "tag": sb["tag"],
        "expected_launcher_sha256": sb["expect_sha256"],
        "node_tag": node_tag,
        "node_sha256": pins["node_release"]["assets"][X86]["sha256"],
        "live_network": sb["live_network"],
        "sample_interval_s": profile["sample_interval_s"],
        "min_total_s": profile["min_total_s"],
        "min_steady_s": profile["min_steady_s"],
        "min_steady_samples": profile["min_steady_samples"],
        "post_reboot_healthy_s": profile["post_reboot_healthy_s"],
        "max_gap_factor": profile["max_gap_factor"],
        "reboot_recovery_deadline_s": profile["reboot_recovery_deadline_s"],
        "forced_grace_s": profile["forced_grace_s"],
        "updater_runs": profile["updater_runs"],
        "kickstarts": profile["kickstarts"],
    }


def interrupt_quota(network_launcher_bytes: int, node_bytes: int) -> int:
    """Inbound TLS bytes to let through so the cut lands inside the node download.

    In published mode canary-consume.sh fetches the launcher first (about 1.04 times its size on the wire);
    in the artifact rehearsal the launcher is a local copy and network_launcher_bytes is 0. The node asset
    follows. The cut must land between 15 % and 75 % of the node asset."""
    quota = int(network_launcher_bytes * 1.06) + 12_000_000 + 400_000
    fraction = (quota - network_launcher_bytes * 1.04) / node_bytes
    if not 0.15 <= fraction <= 0.75:
        raise ValueError(f"the quota {quota} would cut the node download at {fraction:.0%}, outside 15 %..75 %")
    return quota


def resume_bound(network_launcher_bytes: int, asset_bytes: int, partial_bytes: int, attempts: int = 1) -> int:
    """Upper bound of inbound TLS bytes for a download that RESUMES: everything minus what the .partial holds."""
    full = network_launcher_bytes + asset_bytes
    return int((full - partial_bytes) * 1.06) + 2_000_000 * max(1, attempts)


def canary_replay(original: str) -> str:
    """The rehearsal script: canary-consume.sh with its ONE download line replaced by a local copy."""
    matches = CANARY_DOWNLOAD_LINE.findall(original)
    if len(matches) != 1:
        raise ValueError(f"expected exactly one download line in canary-consume.sh, found {len(matches)}")
    replaced = CANARY_DOWNLOAD_LINE.sub('cp "${ARC_W0_LAUNCHER_SRC:?}" "$staged"', original)
    lines = replaced.split("\n")
    banner = (
        "# GENERATED BY wave0-lab (rehearsal): scripts/legacy-bridge/canary-consume.sh with its single download line\n"
        "# replaced by a copy of the Stage A handoff launcher bytes. NOT the published-tag run."
    )
    lines.insert(1, banner)
    return "\n".join(lines)


def cloud_init_user_data(public_key: str) -> str:
    return "\n".join(
        [
            "#cloud-config",
            "hostname: arc-wave0",
            "manage_etc_hosts: true",
            "users:",
            f"  - name: {GUEST_USER}",
            "    gecos: ARC Wave 0 lab",
            "    shell: /bin/bash",
            '    sudo: "ALL=(ALL) NOPASSWD:ALL"',
            "    lock_passwd: true",
            "    ssh_authorized_keys:",
            f"      - {public_key.strip()}",
            "ssh_pwauth: false",
            "package_update: true",
            "package_upgrade: false",
            "packages:",
            "  - jq",
            "  - curl",
            "  - iptables",
            "  - ca-certificates",
            "  - python3",
            "  - openssl",
            "  - xxd",
            "  - procps",
            "runcmd:",
            "  - \"systemctl disable --now apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service snapd.refresh.timer motd-news.timer fwupd-refresh.timer || true\"",
            "  - \"mkdir -p /var/log/journal && systemd-tmpfiles --create --prefix /var/log/journal && systemctl restart systemd-journald\"",
            f"  - \"install -d -o {GUEST_USER} -g {GUEST_USER} {GUEST_WORK} {GUEST_WORK}/snapshots {GUEST_WORK}/baseline\"",
            f"  - \"install -d {GUEST_OPT}\"",
            f"  - \"touch {GUEST_WORK}/cloud-init-done\"",
            "",
        ]
    )


def qemu_argv(work: Path, cpus: int, memory_mb: int) -> list[str]:
    return [
        "qemu-system-x86_64",
        "-name", "arc-wave0",
        "-machine", "q35,accel=kvm",
        "-cpu", "host",
        "-smp", str(cpus),
        "-m", str(memory_mb),
        "-drive", f"file={work / 'disk.qcow2'},if=virtio,format=qcow2",
        "-drive", f"file={work / 'seed.iso'},if=virtio,format=raw,readonly=on",
        # ipv6=off: the hosted runner has no IPv6 route; without it the guest tries IPv6 first and every download
        # would wait for a connect timeout, far beyond the v0.7 updater's 30-second health window.
        "-netdev", f"user,id=n0,ipv6=off,hostfwd=tcp:127.0.0.1:{SSH_PORT}-:22",
        "-device", "virtio-net-pci,netdev=n0",
        "-object", "rng-random,filename=/dev/urandom,id=rng0",
        "-device", "virtio-rng-pci,rng=rng0",
        "-rtc", "base=utc",
        "-display", "none",
        "-serial", f"file:{work / 'console.log'}",
        "-monitor", f"unix:{work / 'monitor.sock'},server,nowait",
        "-daemonize",
        "-pidfile", str(work / "qemu.pid"),
    ]


def state_changes(pre: dict, post: dict, fields: tuple[str, ...] = UPDATER_UNCHANGED_FIELDS) -> list[str]:
    changed = [f"{name}: {pre.get(name)!r} -> {post.get(name)!r}" for name in fields if pre.get(name) != post.get(name)]
    if post.get("auto_update_rolled_back", 0) != pre.get("auto_update_rolled_back", 0):
        changed.append("a ROLLED BACK line appeared in auto-update.log")
    if not post.get("health_ok"):
        changed.append("the node is not healthy after the run")
    return changed


def kickstart_problems(pre: dict, post: dict, address: str | None) -> list[str]:
    problems = []
    if post.get("main_pid") in (None, 0) or post.get("main_pid") == pre.get("main_pid"):
        problems.append(f"the restart did not replace the process (pid {pre.get('main_pid')} -> {post.get('main_pid')})")
    if not post.get("health_ok"):
        problems.append("the node is not healthy after the restart")
    for name in ("address", "bridge_node_address"):
        if post.get(name) != pre.get(name):
            problems.append(f"identity changed: {name} {pre.get(name)!r} -> {post.get(name)!r}")
    if address and post.get("address") != address:
        problems.append(f"identity differs from the first bridged run: {post.get('address')!r} != {address!r}")
    for name in ("bin_arc_node_sha256", "version_txt", "node_exe_sha256", "legacy_fingerprint"):
        if post.get(name) != pre.get(name):
            problems.append(f"{name} changed: {pre.get(name)!r} -> {post.get(name)!r}")
    if post.get("bridge_log_reuse_count", 0) != pre.get("bridge_log_reuse_count", 0) + 1:
        problems.append(
            "the verified release cache was not reused exactly once "
            f"(reuse lines {pre.get('bridge_log_reuse_count')} -> {post.get('bridge_log_reuse_count')})"
        )
    return problems


def parse_apply_output(out: str) -> dict:
    begin = re.search(r"^APPLY_BEGIN ([0-9.]+)", out, re.MULTILINE)
    end = re.search(r"^APPLY_END ([0-9.]+) rc=(\d+)", out, re.MULTILINE)
    return {
        "begin": float(begin.group(1)) if begin else None,
        "end": float(end.group(1)) if end else None,
        "rc": int(end.group(2)) if end else None,
    }


def last_int(text: str, default: int = 0) -> int:
    """The last whole-number token of a command's output (tolerates sudo or shell chatter around it)."""
    found = re.findall(r"(?<![\w.])\d+(?![\w.])", text)
    return int(found[-1]) if found else default


def last_json_line(text: str) -> dict | None:
    for line in reversed(text.strip().splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                return json.loads(line)
            except ValueError:
                continue
    return None


# ----------------------------------------------------------------------------------------------
# The lab
# ----------------------------------------------------------------------------------------------

@dataclass
class Result:
    rc: int
    out: str

    @property
    def ok(self) -> bool:
        return self.rc == 0


class Lab:
    def __init__(self, cfg: dict, evidence: Path, work: Path):
        self.cfg = cfg
        self.sb = cfg["stage_b"]
        self.profile = self.sb["profiles"][self.sb["profile"]]
        self.source = self.sb["launcher_source"]
        self.tag = self.sb["tag"]
        self.expect = self.sb["expect_sha256"]
        self.live_allowed = self.sb["live_network"] == "allowed"
        self.pins = json.loads((ROOT / "crates/arc-legacy-bridge/pins/active.json").read_text(encoding="utf-8"))
        self.node_tag = self.pins["node_release"]["tag"]
        self.node_sha = self.pins["node_release"]["assets"][X86]["sha256"]
        self.bridge_version = self.pins["bridge_version"]
        self.evidence = evidence
        self.work = work
        evidence.mkdir(parents=True, exist_ok=True)
        work.mkdir(parents=True, exist_ok=True)
        self.key = work / "id_ed25519"
        self.t_start = time.time()
        self.deadline = self.t_start + self.profile["deadline_min"] * 60
        self.seq = 0
        self.recorded: set[str] = set()
        self.interrupt_mode = "quota"
        self.launcher_bytes = 0
        self.t0_address: str | None = None
        self.t0_guest_epoch: float | None = None
        self.last_forced_end_guest: float | None = None
        self.partial_bytes = 0

    # --- evidence files ----------------------------------------------------------------------
    def _append(self, name: str, record: dict) -> None:
        with (self.evidence / name).open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(record, sort_keys=True) + "\n")
            handle.flush()
            os.fsync(handle.fileno())

    def say(self, message: str) -> None:
        elapsed = int(time.time() - self.t_start)
        print(f"[{elapsed // 3600:d}:{elapsed % 3600 // 60:02d}:{elapsed % 60:02d}] {message}", flush=True)

    def event(self, name: str, forced: bool = False, guest_epoch: float | None = None, **detail) -> None:
        self.seq += 1
        if set(detail) == {"detail"} and isinstance(detail["detail"], dict):
            detail = detail["detail"]  # event("x", detail={...}) and event("x", a=1) both give a flat detail object
        if guest_epoch is None and forced:
            guest_epoch = self.guest_epoch()
        self._append(
            "events.jsonl",
            {"seq": self.seq, "name": name, "forced": forced, "host_epoch": round(time.time(), 3), "guest_epoch": guest_epoch, "detail": detail},
        )
        self.say(f"event {name}{' (forced)' if forced else ''} {json.dumps(detail, sort_keys=True)[:300]}")

    def check(self, check_id: str, title: str, result: str, detail: str) -> None:
        assert result in ("PASS", "FAIL", "INFO")
        self.recorded.add(check_id)
        self._append("checks-live.jsonl", {"id": check_id, "title": title, "result": result, "detail": detail[:3000]})
        self.say(f"check {check_id}: {result} - {detail[:300]}")

    # --- guest access --------------------------------------------------------------------------
    def ssh_argv(self, remote_cmd: str) -> list[str]:
        return ["ssh", "-i", str(self.key), "-p", str(SSH_PORT), *SSH_OPTS, f"{GUEST_USER}@127.0.0.1", "bash -c " + shlex.quote(remote_cmd)]

    def guest(self, cmd: str, timeout: float = 120, retries: int = 0, log: bool = True) -> Result:
        attempt = 0
        while True:
            try:
                done = subprocess.run(self.ssh_argv(cmd), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout, check=False)
                rc, out = done.returncode, done.stdout.decode("utf-8", "replace")
            except subprocess.TimeoutExpired as error:
                rc, out = 124, (error.stdout or b"").decode("utf-8", "replace") + "\n[host] timed out"
            if log:
                self._append("commands.jsonl", {"host_epoch": round(time.time(), 3), "cmd": cmd[:600], "rc": rc})
            if rc == 255 and attempt < retries:
                attempt += 1
                time.sleep(5)
                continue
            return Result(rc, out)

    def guest_epoch(self) -> float | None:
        result = self.guest("date +%s.%N", timeout=30, retries=2, log=False)
        try:
            return float(result.out.strip().splitlines()[-1])
        except (ValueError, IndexError):
            return None

    def put_tree(self, local: Path, remote: str) -> None:
        tar = subprocess.Popen(["tar", "-C", str(local), "-cf", "-", "."], stdout=subprocess.PIPE)
        assert tar.stdout is not None
        done = subprocess.run(
            self.ssh_argv(f"sudo mkdir -p {shlex.quote(remote)} && sudo tar -C {shlex.quote(remote)} -xf - --no-same-owner"),
            stdin=tar.stdout, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=300, check=False,
        )
        tar.stdout.close()
        tar.wait()
        if done.returncode != 0 or tar.returncode != 0:
            raise Fatal(f"copying {local} to the guest failed: {done.stdout.decode('utf-8', 'replace')[:500]}")

    def get_file(self, remote: str, local: Path, timeout: float = 300) -> bool:
        try:
            done = subprocess.run(self.ssh_argv(f"sudo cat {shlex.quote(remote)}"), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=timeout, check=False)
        except subprocess.TimeoutExpired:
            return False
        if done.returncode != 0:
            return False
        local.parent.mkdir(parents=True, exist_ok=True)
        local.write_bytes(done.stdout)
        return True

    def wait_health(self, seconds: int) -> bool:
        script = f"for i in $(seq 1 {seconds}); do curl -sf -m 2 http://127.0.0.1:9944/health >/dev/null && exit 0; sleep 1; done; exit 1"
        return self.guest(script, timeout=seconds + 60).ok

    def capture(self) -> dict:
        result = self.guest(f"python3 {GUEST_LAB}/capture_state.py --arc-dir {GUEST_ARC}", timeout=90, retries=2)
        state = last_json_line(result.out)
        if state is None:
            raise Fatal(f"capture_state.py gave no JSON (rc {result.rc}): {result.out[:300]}")
        return state

    def samples_tail(self, count: int = 5) -> list[dict]:
        result = self.guest(f"tail -n {count} {GUEST_WORK}/samples.jsonl", timeout=30, retries=1, log=False)
        found = []
        for line in result.out.splitlines():
            try:
                found.append(json.loads(line))
            except ValueError:
                continue
        return found

    def pull_samples(self) -> None:
        self.get_file(f"{GUEST_WORK}/samples.jsonl", self.evidence / "samples.jsonl")

    # --- phases ---------------------------------------------------------------------------------
    def phase(self, name: str, ids: tuple[str, ...], fn, fatal: bool = False) -> bool:
        self.say(f"=== phase {name}")
        try:
            fn()
            return True
        except Exception as error:  # noqa: BLE001 - a crashed phase is a recorded FAIL, not a lost run
            detail = f"{type(error).__name__}: {error}"
            self.say(f"phase {name} crashed: {detail}\n{traceback.format_exc()[-1500:]}")
            for check_id in ids:
                if check_id not in self.recorded:
                    self.check(check_id, f"phase {name}", "FAIL", f"the phase crashed: {detail}")
            self.event(f"phase_crashed_{name}", detail={"error": detail})
            if fatal or isinstance(error, Fatal):
                raise Fatal(f"fatal phase {name}: {detail}") from error
            return False

    def run_process(self, argv: list[str], timeout: float = 600, check: bool = True, **kwargs) -> subprocess.CompletedProcess:
        done = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout, check=False, **kwargs)
        if check and done.returncode != 0:
            raise Fatal(f"{argv[0]} failed ({done.returncode}): {done.stdout.decode('utf-8', 'replace')[-800:]}")
        return done

    # 1. kvm + vm ----------------------------------------------------------------------------------
    def phase_kvm(self) -> None:
        kvm = Path("/dev/kvm")
        if not kvm.exists():
            self.check("L01-kvm", "/dev/kvm usable (nested virtualization)", "FAIL", "/dev/kvm is absent on this runner")
            raise Fatal("/dev/kvm is absent: nested virtualization is not available on this runner")
        try:
            fd = os.open("/dev/kvm", os.O_RDWR)
            os.close(fd)
        except OSError as error:
            self.check("L01-kvm", "/dev/kvm usable (nested virtualization)", "FAIL", f"/dev/kvm exists but cannot be opened: {error}")
            raise Fatal("/dev/kvm cannot be opened") from error
        version = self.run_process(["qemu-system-x86_64", "--version"], check=False).stdout.decode("utf-8", "replace").splitlines()[:1]
        self.check("L01-kvm", "/dev/kvm usable (nested virtualization)", "PASS", f"/dev/kvm opens read-write; {version[0] if version else 'qemu version unknown'}")

    def phase_vm(self) -> None:
        image = self.sb["image"]
        base = self.work / "base.img"
        if not base.exists():
            self.say("downloading the Ubuntu 24.04 cloud image")
            self.run_process(["curl", "-fL", "--retry", "5", "--retry-delay", "5", "--proto", "=https", "--tlsv1.2", "-o", str(base), image["url"]], timeout=900)
        digest = sha256_file(base)
        if digest != image["sha256"] or base.stat().st_size != image["size"]:
            self.check("L17-vm-image-digest", "Ubuntu 24.04 cloud image matches its pinned digest", "FAIL", f"got {digest} / {base.stat().st_size} bytes")
            raise Fatal("the cloud image does not match its pinned digest")
        self.check("L17-vm-image-digest", "Ubuntu 24.04 cloud image matches its pinned digest", "PASS", f"{digest} serial {image['serial']}")
        if not self.key.exists():
            self.run_process(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(self.key)])
        public_key = Path(str(self.key) + ".pub").read_text(encoding="utf-8")
        disk = self.work / "disk.qcow2"
        if not disk.exists():
            self.run_process(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", str(base), str(disk), f"{self.sb['vm']['disk_gb']}G"])
        seed = self.work / "seed"
        seed.mkdir(exist_ok=True)
        (seed / "user-data").write_text(cloud_init_user_data(public_key), encoding="utf-8")
        (seed / "meta-data").write_text("instance-id: arc-wave0-1\nlocal-hostname: arc-wave0\n", encoding="utf-8")
        self.run_process(["genisoimage", "-quiet", "-output", str(self.work / "seed.iso"), "-volid", "cidata", "-joliet", "-rock", "user-data", "meta-data"], cwd=str(seed))
        self.run_process(qemu_argv(self.work, self.sb["vm"]["cpus"], self.sb["vm"]["memory_mb"]))
        self.event("vm_started", detail={"cpus": self.sb["vm"]["cpus"], "memory_mb": self.sb["vm"]["memory_mb"]})
        end = time.time() + 480
        while time.time() < end:
            if self.guest("true", timeout=30, log=False).ok:
                break
            time.sleep(5)
        else:
            self.dump_console()
            raise Fatal("the guest never answered ssh within 8 minutes")
        status = self.guest("cloud-init status --wait", timeout=900)
        ready = self.guest(f"test -f {GUEST_WORK}/cloud-init-done && command -v jq iptables python3 curl openssl >/dev/null", timeout=30)
        if status.rc not in (0, 2) or not ready.ok:
            self.dump_console()
            raise Fatal(f"cloud-init did not finish cleanly (status rc {status.rc}): {status.out[-400:]}")
        info = self.guest("uname -a; systemctl --version | head -1; python3 --version; id -un; sudo -n true && echo sudo-ok", timeout=30)
        (self.evidence / "guest-info.txt").write_text(info.out, encoding="utf-8")
        self.event("vm_ready", detail={"info": info.out.strip()[:400]})

    def dump_console(self) -> None:
        console = self.work / "console.log"
        if console.exists():
            text = console.read_text(encoding="utf-8", errors="replace")
            (self.evidence / "console.log.tail").write_text(text[-20000:], encoding="utf-8")
            self.say("guest console tail:\n" + text[-3000:])

    # 2. ship + baseline ---------------------------------------------------------------------------
    def phase_ship(self) -> None:
        ship = self.work / "ship"
        for sub in ("tests/legacy-bridge/fake-github", "scripts/legacy-bridge", "crates/arc-legacy-bridge/pins", "legacy-source", "lab", "launcher"):
            (ship / sub).mkdir(parents=True, exist_ok=True)

        def copy(rel: str, dest: str | None = None) -> None:
            data = (ROOT / rel).read_bytes()
            target = ship / (dest or rel)
            target.write_bytes(data)
            target.chmod(0o755)

        copy("tests/legacy-bridge/snapshot_tree.py")
        copy("tests/legacy-bridge/fake-github/curl")
        copy("scripts/legacy-bridge/canary-consume.sh")
        copy("crates/arc-legacy-bridge/pins/active.json")
        for path in sorted((HERE / "guest").iterdir()):
            if path.is_file():
                copy(str(path.relative_to(ROOT)), f"lab/{path.name}")
        original = (ROOT / "scripts/legacy-bridge/canary-consume.sh").read_text(encoding="utf-8")
        (ship / "lab/canary-consume-local.sh").write_text(canary_replay(original), encoding="utf-8")
        (ship / "lab/canary-consume-local.sh").chmod(0o755)

        self.run_process(["git", "-C", str(ROOT), "fetch", "--no-tags", "--depth=1", "origin", f"refs/tags/{LEGACY_TAG}:refs/tags/{LEGACY_TAG}"], timeout=180)
        for source, dest in (
            ("scripts/install-community-node.sh", "install-community-node.sh"),
            ("testnet-seeds.txt", "testnet-seeds.txt"),
            ("genesis.toml", "genesis.toml"),
        ):
            data = self.run_process(["git", "-C", str(ROOT), "show", f"{LEGACY_TAG}:{source}"]).stdout
            (ship / "legacy-source" / dest).write_bytes(data)
        (ship / "live-ips.txt").write_text("\n".join(live_ips.load(ROOT)) + "\n", encoding="utf-8")

        if self.source == "artifact":
            verified = fetch_handoff.fetch(self.cfg, self.work / "handoff")
            launcher = self.work / "handoff" / "v0.7.12" / X86
            if sha256_file(launcher) != self.expect:
                raise Fatal("the handoff launcher is not the expected digest")
            (ship / "launcher" / X86).write_bytes(launcher.read_bytes())
            (ship / "launcher" / X86).chmod(0o755)
            self.launcher_bytes = launcher.stat().st_size
            (self.evidence / "handoff-verified.json").write_text(json.dumps(verified, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        else:
            self.phase_published_precheck()
        (ship / "config-effective.json").write_text(json.dumps(effective_config(self.cfg, self.pins), indent=2, sort_keys=True) + "\n", encoding="utf-8")
        self.put_tree(ship, GUEST_OPT)
        self.guest(f"sudo chmod -R a+rX {GUEST_OPT} && sudo chmod +x {GUEST_LAB}/*.sh {GUEST_LAB}/*.py {GUEST_OPT}/scripts/legacy-bridge/canary-consume.sh", timeout=60)
        self.event("files_shipped", detail={"launcher_source": self.source, "launcher_bytes": self.launcher_bytes})

    def phase_published_precheck(self) -> None:
        """Published mode: read the real release (GET only) and prove the launcher digest before touching the guest."""
        repo = self.cfg["repository"]
        raw = fetch_handoff.run_gh(["api", f"repos/{repo}/releases/tags/{self.tag}"])
        release = json.loads(raw.decode("utf-8"))
        assets = {asset["name"]: asset for asset in release.get("assets", [])}
        asset = assets.get(X86)
        problems = []
        if release.get("draft"):
            problems.append("the release is still a draft")
        if asset is None:
            problems.append(f"the release has no {X86}")
        elif asset.get("digest") != f"sha256:{self.expect}":
            problems.append(f"{X86} digest {asset.get('digest')} != expected sha256:{self.expect}")
        (self.evidence / "published-release.json").write_text(
            json.dumps(
                {
                    "tag_name": release.get("tag_name"),
                    "id": release.get("id"),
                    "draft": release.get("draft"),
                    "prerelease": release.get("prerelease"),
                    "immutable": release.get("immutable"),
                    "target_commitish": release.get("target_commitish"),
                    "published_at": release.get("published_at"),
                    "assets": {name: {"size": a.get("size"), "digest": a.get("digest")} for name, a in sorted(assets.items())},
                },
                indent=2, sort_keys=True,
            ) + "\n",
            encoding="utf-8",
        )
        local = self.work / "published-launcher"
        self.run_process(["curl", "-fL", "--retry", "3", "--proto", "=https", "--tlsv1.2", "-o", str(local), f"https://github.com/{repo}/releases/download/{self.tag}/{X86}"], timeout=300)
        if sha256_file(local) != self.expect:
            problems.append("the downloaded published launcher does not hash to the expected digest")
        self.launcher_bytes = local.stat().st_size
        if problems:
            self.check("L15-published-release", "the published release carries the expected launcher", "FAIL", "; ".join(problems))
            raise Fatal("; ".join(problems))
        self.check("L15-published-release", "the published release carries the expected launcher", "PASS", f"{self.tag} {X86} sha256 {self.expect} ({self.launcher_bytes} bytes)")

    def phase_baseline(self) -> None:
        blocked = self.guest(f"sudo bash {GUEST_LAB}/install-units.sh --units live-block", timeout=120)
        if not blocked.ok:
            raise Fatal(f"installing the live-network block failed: {blocked.out[-300:]}")
        self.event("live_network_blocked", detail={"reason": "the v0.7.7 baseline must never touch the live network", "status": blocked.out.strip()[-200:]})
        result = self.guest(f"bash {GUEST_LAB}/baseline.sh", timeout=1200)
        (self.evidence / "baseline.log").write_text(result.out, encoding="utf-8")
        if not result.ok:
            self.check("L13-baseline", "stranded v0.7.11 headless install (real updater, systemd) is up", "FAIL", result.out[-500:])
            raise Fatal("the baseline install failed")
        self.check("L13-baseline", "stranded v0.7.11 headless install (real updater, systemd) is up", "PASS", "unmodified v0.7.11 installer, v0.7.7 node, updater timer active")
        units = self.guest(
            f"sudo bash {GUEST_LAB}/install-units.sh --units sampler --interval {self.profile['sample_interval_s']} --arc-dir {GUEST_ARC} --user {GUEST_USER}",
            timeout=120,
        )
        if not units.ok:
            raise Fatal(f"installing the lab units failed: {units.out[-300:]}")
        self.event("baseline_ready", detail={"live_block": "on"})
        end = time.time() + 120
        while time.time() < end:
            if self.samples_tail(1):
                break
            time.sleep(3)
        else:
            self.check("L20-sampler-running", "the guest sampler writes samples", "FAIL", "no sample appeared within 2 minutes")
            raise Fatal("the sampler is not running")
        self.check("L20-sampler-running", "the guest sampler writes samples", "PASS", f"first sample seen; interval {self.profile['sample_interval_s']} s")
        selftest = self.guest(f"sudo {GUEST_LAB}/interrupt.sh selftest", timeout=60)
        self.interrupt_mode = "quota" if selftest.ok else "watch"
        self.event("interrupt_mode", detail={"mode": self.interrupt_mode, "selftest": selftest.out.strip()[-200:]})
        self.say(f"baseline observation for {self.profile['baseline_s']} s")
        time.sleep(self.profile["baseline_s"])

    # 3. consume ----------------------------------------------------------------------------------
    def network_launcher_bytes(self) -> int:
        """Bytes of the launcher that cross the network during a consume: only the published mode downloads it."""
        return self.launcher_bytes if self.source == "published" else 0

    def apply_cmd(self, dry: bool = False) -> str:
        return f"bash {GUEST_LAB}/apply.sh {self.source} {self.tag} {self.expect}" + (" --dry-run" if dry else "")

    def phase_dry_run(self) -> None:
        result = self.guest(self.apply_cmd(dry=True), timeout=120)
        (self.evidence / "consume-dry-run.txt").write_text(result.out, encoding="utf-8")
        parsed = parse_apply_output(result.out)
        needles = ["Dry run only", f"require SHA-256 {self.expect}", f"releases/download/{self.tag}/{X86}", "currently v0.7.7"]
        missing = [needle for needle in needles if needle not in result.out]
        ok = result.ok and parsed["rc"] == 0 and not missing
        label = "canary-consume.sh unmodified" if self.source == "published" else "rehearsal replay of canary-consume.sh"
        self.check("L02-consume-dry-run", f"dry run of {label} --tag {self.tag} changes nothing and prints the plan", "PASS" if ok else "FAIL",
                   f"rc {result.rc}; missing {missing}; plan names the exact URL, digest and v0.7.7 install")
        if not ok:
            raise Fatal("the consume dry run failed")

    def phase_apply1(self) -> None:
        node_bytes = self.pins["node_release"]["assets"][X86]["size"]
        quota = interrupt_quota(self.network_launcher_bytes(), node_bytes)
        partial = f"{GUEST_ARC}/legacy-bridge/releases/{self.node_tag}/{X86}.partial"
        final = f"{GUEST_ARC}/legacy-bridge/releases/{self.node_tag}/{X86}"
        if self.interrupt_mode == "quota":
            armed = self.guest(f"sudo {GUEST_LAB}/interrupt.sh arm {quota}", timeout=60)
        else:
            threshold = 12_000_000
            armed = self.guest(f"sudo sh -c 'nohup {GUEST_LAB}/interrupt.sh watch {shlex.quote(partial)} {threshold} 150 > {GUEST_WORK}/watch.log 2>&1 &'", timeout=60)
        if not armed.ok:
            raise Fatal(f"arming the interruption failed: {armed.out[-300:]}")
        before = self.capture()
        started = self.guest_epoch()
        result = self.guest(self.apply_cmd(), timeout=300)
        (self.evidence / "consume-apply1.txt").write_text(result.out, encoding="utf-8")
        counters = self.guest(f"sudo {GUEST_LAB}/interrupt.sh counters", timeout=30).out.strip()
        self.guest(f"sudo {GUEST_LAB}/interrupt.sh disarm", timeout=60)
        self.event("apply1", forced=True, guest_epoch=started, rc=parse_apply_output(result.out)["rc"], quota=quota, mode=self.interrupt_mode, inbound_bytes=counters)
        listing = self.guest(f"stat -c '%n %s' {shlex.quote(partial)} {shlex.quote(final)} 2>&1; true", timeout=30).out
        size_match = re.search(re.escape(partial) + r" (\d+)", listing)
        self.partial_bytes = int(size_match.group(1)) if size_match else 0
        final_present = bool(re.search(re.escape(final) + r" \d+", listing))
        healthy_v07 = self.wait_health(90)
        after = self.capture()
        rolled_back = (
            parse_apply_output(result.out)["rc"] == 1
            and "rolling back exactly like the v0.7 updater" in result.out
            and after.get("bin_arc_node_sha256") == LEGACY_NODE_SHA256
            and after.get("version_txt") == "0.7.7"
        )
        partial_ok = 1_000_000 < self.partial_bytes < node_bytes and not final_present
        ok = rolled_back and partial_ok and healthy_v07
        self.check(
            "L03-interrupt-took-effect", "the first uncached download was cut mid-way: partial kept, node not cached, launcher rolled back by the v0.7 health logic",
            "PASS" if ok else "FAIL",
            f"mode {self.interrupt_mode}; rolled_back={rolled_back}; partial {self.partial_bytes} of {node_bytes} bytes; final cached={final_present}; "
            f"v0.7.7 healthy again={healthy_v07}; inbound TLS bytes {counters}; before version {before.get('version_txt')}",
        )
        if not ok:
            raise Fatal("the interrupted first download did not behave as designed")

    def open_live_network(self) -> None:
        """Lift the live-network block, but only once the v0.7.7 process is gone (it would publish its hostname)."""
        if not self.live_allowed:
            return
        gone = self.guest(f"readlink /proc/$(systemctl show -p MainPID --value arc-node)/exe", timeout=30).out.strip()
        if gone.endswith("/bin/arc-node"):
            raise Fatal(f"refusing to open the live network: the running node is still {gone}")
        off = self.guest("sudo systemctl disable --now arc-w0-live-block.service && sudo " + GUEST_LAB + "/live-block.sh status", timeout=120)
        (self.evidence / "live-block-off.txt").write_text(off.out, encoding="utf-8")
        if not off.ok or "blocked" in off.out:
            raise Fatal(f"the live-network block could not be lifted: {off.out[-300:]}")
        self.event("live_network_open", authorization=self.sb["live_network_authorization"])

    def first_bridged_sample(self, since_epoch: float) -> dict | None:
        """The first healthy sample of the v0.8 node (executable under legacy-bridge/releases) at or after since_epoch."""
        query = ".health_ok == true and ((.node_exe // \"\") | contains(\"/legacy-bridge/releases/\")) and .epoch >= $since"
        result = self.guest(f"jq -c --argjson since {since_epoch} 'select({query})' {GUEST_WORK}/samples.jsonl | head -n 1", timeout=60, log=False)
        return last_json_line(result.out)

    def phase_apply2(self) -> None:
        snapshots_before = self.guest(f"ls {GUEST_WORK}/snapshots", timeout=30).out.split()
        node_bytes = self.pins["node_release"]["assets"][X86]["size"]
        sizes = sum(self.pins["node_release"]["assets"][name]["size"] for name in (X86, "arc-cli-linux-x86_64"))
        self.guest(f"sudo {GUEST_LAB}/interrupt.sh count", timeout=30)
        attempts = []
        applied = False
        for attempt in range(1, 4):
            started = self.guest_epoch()
            result = self.guest(self.apply_cmd(), timeout=300)
            parsed = parse_apply_output(result.out)
            attempts.append({"attempt": attempt, "rc": parsed["rc"], "begin": parsed["begin"], "end": parsed["end"]})
            (self.evidence / f"consume-apply2-attempt{attempt}.txt").write_text(result.out, encoding="utf-8")
            if parsed["rc"] == 0:
                applied = True
                self.event("apply2", forced=True, guest_epoch=started, attempt=attempt)
                break
            self.say(f"apply attempt {attempt} did not succeed (rc {parsed['rc']}); the partial download resumes next time")
            self.wait_health(90)
        counters = last_int(self.guest(f"sudo {GUEST_LAB}/interrupt.sh counters", timeout=30).out)
        self.guest(f"sudo {GUEST_LAB}/interrupt.sh disarm", timeout=60)
        if not applied:
            self.check("L04-interrupt-resumes", "after the interruption, the next run resumes the .partial and the bridged node comes up", "FAIL", f"three attempts failed: {attempts}")
            raise Fatal("the bridge could not be consumed after the interruption")
        self.event("attempts_after_interrupt", detail={"attempts": attempts})
        self.check("L16-apply2-attempts", "consume attempts needed after the interruption", "INFO", f"{len(attempts)} attempt(s): {attempts}")
        if not self.wait_health(90):
            raise Fatal("the bridged node is not healthy after the successful consume")
        bound = resume_bound(self.network_launcher_bytes(), sizes, self.partial_bytes, len(attempts))
        state = self.capture()
        final_ok = self.guest(f"sha256sum {GUEST_ARC}/legacy-bridge/releases/{self.node_tag}/{X86} | cut -d' ' -f1", timeout=60).out.strip() == self.node_sha
        partial_gone = not self.guest(f"test -e {GUEST_ARC}/legacy-bridge/releases/{self.node_tag}/{X86}.partial", timeout=30).ok
        resumed = counters <= bound
        ok = final_ok and partial_gone and resumed and self.partial_bytes > 0 and state.get("bin_arc_node_sha256") == self.expect
        self.check(
            "L04-interrupt-resumes", "after the interruption, the next run resumes the .partial and the bridged node comes up", "PASS" if ok else "FAIL",
            f"partial before {self.partial_bytes} bytes; inbound TLS bytes {counters} <= bound {bound} (a restart from zero would need about {self.network_launcher_bytes() + sizes}); "
            f"node cache digest ok={final_ok}; .partial consumed={partial_gone}; installed launcher is the expected digest={state.get('bin_arc_node_sha256') == self.expect}; node_bytes {node_bytes}",
        )
        # The bridged node is up and the v0.7.7 process is gone: now the live network may open, if it is allowed.
        self.open_live_network()
        # Find the t0 sample (the first healthy sample of the v0.8 node) and the hook snapshot taken at the start of this kept run.
        end = time.time() + 6 * self.profile["sample_interval_s"] + 60
        first = None
        while time.time() < end:
            first = self.first_bridged_sample(started)
            if first:
                break
            time.sleep(5)
        if not first:
            raise Fatal("no healthy bridged sample was recorded after the consume")
        self.t0_address = first.get("address")
        self.t0_guest_epoch = first["epoch"]
        self.event("t0_bridged_healthy", guest_epoch=first["epoch"], address=self.t0_address, legacy_fingerprint=first.get("legacy_fingerprint"), public_name=first.get("public_name"))
        listing = self.guest(f"ls {GUEST_WORK}/snapshots", timeout=30).out.split()
        new = sorted(name for name in listing if name not in snapshots_before and name.startswith("start-"))
        chosen = None
        for name in reversed(new):
            blob = self.guest(f"jq -r .binary_sha256 {GUEST_WORK}/snapshots/{name}", timeout=30).out.strip()
            if blob == self.expect:
                chosen = name
                break
        if not chosen:
            raise Fatal(f"no ExecStartPre snapshot with the launcher digest was taken for the kept run (new: {new})")
        made = self.guest(
            f"cp {GUEST_WORK}/snapshots/{chosen} {GUEST_WORK}/before-snapshot.json && python3 {GUEST_LAB}/invariants.py collect --label before --arc-dir {GUEST_ARC} "
            f"--out {GUEST_WORK}/invariants-before.json --legacy-from {GUEST_WORK}/snapshots/{chosen}",
            timeout=120,
        )
        if not made.ok:
            raise Fatal(f"BEFORE invariants failed: {made.out[-300:]}")
        self.get_file(f"{GUEST_WORK}/invariants-before.json", self.evidence / "invariants-before.json")
        self.event("before_invariants", detail={"hook_snapshot": chosen})

    # 4. battery ----------------------------------------------------------------------------------
    def phase_updaters(self) -> None:
        for number in range(1, self.profile["updater_runs"] + 1):
            check_id = f"L0{4 + number}-updater-{number}-noop"
            title = f"real arc-updater run {number} (real GitHub API) changes nothing"
            try:
                pre = self.capture()
                started = self.guest_epoch()
                ran = self.guest("sudo systemctl start arc-updater.service", timeout=240)
                props = self.guest("systemctl show arc-updater.service -p Result -p ExecMainStatus -p ActiveState", timeout=30).out.replace("\n", " ").strip()
                tail = self.guest(f"tail -n 8 {GUEST_ARC}/auto-update.log", timeout=30).out
                time.sleep(3)
                post = self.capture()
                changes = state_changes(pre, post)
                note = ""
                if "new version available" in tail and ran.rc != 0:
                    note = f" Exit status is information only: {PRE_FLIP_NOTE}."
                elif "up to date" in tail:
                    note = " The updater reported up to date."
                self.event(f"updater_run_{number}", forced=True, guest_epoch=started, systemctl_rc=ran.rc, unit=props)
                self.check(check_id, title, "PASS" if not changes else "FAIL",
                           f"unchanged: binary sha, version.txt, pid, process start, node digest, identity, legacy-data fingerprint; changes: {changes or 'none'}; unit: {props}; log tail: {tail.strip()[-300:]}.{note}")
            except Exception as error:  # noqa: BLE001
                self.check(check_id, title, "FAIL", f"{type(error).__name__}: {error}")
            time.sleep(self.profile["kickstart_gap_s"])

    def phase_kickstarts(self) -> None:
        for number in range(1, self.profile["kickstarts"] + 1):
            check_id = f"L{6 + number:02d}-kickstart-{number}"
            title = f"kickstart {number} (systemctl restart arc-node): same identity, verified cache reused"
            try:
                pre = self.capture()
                started = self.guest_epoch()
                self.guest("sudo systemctl restart arc-node", timeout=180)
                healthy = self.wait_health(120)
                post = self.capture()
                problems = kickstart_problems(pre, post, self.t0_address)
                if not healthy:
                    problems.append("not healthy within 120 s")
                self.event(f"kickstart_{number}", forced=True, guest_epoch=started, healthy=healthy)
                self.check(check_id, title, "PASS" if not problems else "FAIL", f"problems: {problems or 'none'}; pid {pre.get('main_pid')} -> {post.get('main_pid')}")
            except Exception as error:  # noqa: BLE001
                self.check(check_id, title, "FAIL", f"{type(error).__name__}: {error}")
            time.sleep(self.profile["kickstart_gap_s"])

    def phase_reboot(self) -> None:
        pre_boot = self.guest("cat /proc/sys/kernel/random/boot_id", timeout=30, retries=2).out.strip()
        pre_logs = self.guest(
            f"sudo journalctl -b 0 --no-pager -o short-iso -u arc-node -u arc-updater.service -u arc-updater.timer -u arc-w0-sampler > {GUEST_WORK}/journal-pre-boot.txt 2>&1; "
            f"tail -n 3000 {GUEST_ARC}/node.log > {GUEST_WORK}/node-log-pre-boot.txt 2>&1; cp {GUEST_ARC}/legacy-bridge/bridge.log {GUEST_WORK}/bridge-log-pre-boot.txt 2>&1; "
            f"wc -c {GUEST_WORK}/journal-pre-boot.txt {GUEST_WORK}/node-log-pre-boot.txt {GUEST_WORK}/bridge-log-pre-boot.txt",
            timeout=120,
        )
        self.pull_samples()
        self.event("reboot_issued", forced=True, boot_id_before=pre_boot)
        issued = time.time()
        self.guest("sudo sh -c '(sleep 2; systemctl reboot) >/dev/null 2>&1 &'", timeout=30)
        post_boot = None
        end = issued + self.profile["reboot_recovery_deadline_s"] + 120
        time.sleep(10)
        while time.time() < end:
            probe = self.guest("cat /proc/sys/kernel/random/boot_id", timeout=25, log=True)
            if probe.ok and probe.out.strip() and probe.out.strip() != pre_boot:
                post_boot = probe.out.strip()
                break
            time.sleep(5)
        ssh_back = round(time.time() - issued, 1)
        if post_boot is None:
            self.dump_console()
            self.check("L10-reboot-boot-id-changed", "the guest really rebooted: the boot ID changed", "FAIL", f"no new boot id after {ssh_back} s (still {pre_boot or 'unknown'})")
            raise Fatal("the guest did not come back with a new boot id")
        recovered = None
        end = issued + self.profile["reboot_recovery_deadline_s"]
        while time.time() < end + 60:
            for sample in reversed(self.samples_tail(6)):
                if sample.get("boot_id") == post_boot and sample.get("health_ok") and "/legacy-bridge/releases/" in str(sample.get("node_exe")):
                    recovered = sample
                    break
            if recovered:
                break
            time.sleep(5)
        post_logs = self.guest(
            f"sudo journalctl -b 0 --no-pager -o short-iso -u arc-node -u arc-updater.service -u arc-updater.timer -u arc-w0-sampler > {GUEST_WORK}/journal-post-boot.txt 2>&1; "
            f"tail -n 3000 {GUEST_ARC}/node.log > {GUEST_WORK}/node-log-post-boot.txt 2>&1; cp {GUEST_ARC}/legacy-bridge/bridge.log {GUEST_WORK}/bridge-log-post-boot.txt 2>&1; "
            f"journalctl --list-boots --no-pager > {GUEST_WORK}/boots.txt 2>&1; wc -c {GUEST_WORK}/journal-post-boot.txt {GUEST_WORK}/node-log-post-boot.txt {GUEST_WORK}/bridge-log-post-boot.txt",
            timeout=120,
        )
        timer = self.guest("systemctl is-active arc-updater.timer; systemctl is-enabled arc-updater.timer", timeout=30).out.split()
        guest_now = self.guest_epoch()
        self.event(
            "reboot_recovered", guest_epoch=recovered["epoch"] if recovered else guest_now,
            boot_id_after=post_boot, first_healthy_sample_epoch=recovered["epoch"] if recovered else None,
            ssh_back_s=ssh_back, timer=timer,
        )
        self.last_forced_end_guest = recovered["epoch"] if recovered else guest_now
        self.check("L10-reboot-boot-id-changed", "the guest really rebooted: the boot ID changed", "PASS" if post_boot != pre_boot else "FAIL",
                   f"boot id {pre_boot} -> {post_boot}; ssh back after {ssh_back} s; node recovered by itself={bool(recovered)} (no start command is issued after the reboot; commands.jsonl is the audit)")
        logs_ok = "journal-pre-boot.txt" in pre_logs.out and "journal-post-boot.txt" in post_logs.out
        self.check("L12-pre-post-boot-logs", "pre- and post-boot logs were captured", "PASS" if logs_ok else "FAIL",
                   f"pre: {pre_logs.out.strip()[-200:]} post: {post_logs.out.strip()[-200:]}")
        if not recovered:
            raise Fatal("the node did not recover by itself after the reboot")

    # 5. steady state ------------------------------------------------------------------------------
    def phase_steady(self) -> None:
        profile = self.profile
        assert self.last_forced_end_guest is not None and self.t0_guest_epoch is not None
        interval = profile["sample_interval_s"]
        target = max(self.last_forced_end_guest + profile["min_steady_s"], self.t0_guest_epoch + profile["min_total_s"]) + 2 * interval
        self.event("steady_begin", guest_epoch=self.last_forced_end_guest, last_forced_event="reboot_issued", target_guest_epoch=target)
        last_pull = 0.0
        last_beat = 0.0
        while True:
            now_guest = self.guest_epoch()
            if now_guest is None:
                time.sleep(15)
                continue
            if now_guest >= target:
                break
            if time.time() > self.deadline - 600:
                self.event("steady_cut_short_by_deadline", guest_epoch=now_guest, target=target)
                break
            if time.time() - last_pull >= profile["pull_samples_s"]:
                self.pull_samples()
                last_pull = time.time()
            if time.time() - last_beat >= profile["heartbeat_s"]:
                tail = self.samples_tail(1)
                sample = tail[-1] if tail else {}
                self.event("heartbeat", detail={
                    "remaining_s": int(target - now_guest), "seq": sample.get("seq"), "health_ok": sample.get("health_ok"),
                    "address": str(sample.get("address"))[:8], "boot": str(sample.get("boot_id"))[:8],
                    "registered": sample.get("coordinators_registered"), "legacy_compare": sample.get("legacy_byte_compare"),
                })
                last_beat = time.time()
            time.sleep(min(60.0, max(1.0, target - now_guest)))
        self.pull_samples()
        self.event("steady_end", guest_epoch=self.guest_epoch())

    # 6. final -----------------------------------------------------------------------------------
    def phase_final(self) -> None:
        after = self.guest(f"python3 {GUEST_LAB}/invariants.py collect --label after --arc-dir {GUEST_ARC} --out {GUEST_WORK}/invariants-after.json", timeout=180)
        if not after.ok:
            raise Fatal(f"AFTER invariants failed: {after.out[-300:]}")
        self.get_file(f"{GUEST_WORK}/invariants-after.json", self.evidence / "invariants-after.json")
        refuse = self.guest(
            f"cd {GUEST_WORK} && printf 'not the model' > fake.gguf; "
            f"{GUEST_ARC}/bin/arc-node --legacy-bridge-compute on --model {GUEST_WORK}/fake.gguf > compute-on.txt 2>&1; echo compute_rc=$?; "
            f"{GUEST_ARC}/bin/arc-node --rpc 127.0.0.1:9944 --p2p-port 9945 --data-dir {GUEST_ARC}/data --validator-seed redacted --stake 5000000 --min-stake 500000 > validator.txt 2>&1; echo validator_rc=$?; "
            f"ls {GUEST_ARC}/legacy-bridge/nodes/headless-*/compute-consent 2>&1 | head -1",
            timeout=120,
        )
        compute_rc = re.search(r"compute_rc=(\d+)", refuse.out)
        validator_rc = re.search(r"validator_rc=(\d+)", refuse.out)
        refused = bool(compute_rc and validator_rc and compute_rc.group(1) == "78" and validator_rc.group(1) == "78")
        self.check("L14-compute-refusal", "an unverified model and a validator command line are refused (exit 78); no consent recorded", "PASS" if refused else "FAIL", refuse.out.strip()[-400:])
        node_address = self.guest(f"jq -r .node_address {GUEST_ARC}/legacy-bridge/nodes/headless-*/bridge-state.json", timeout=30).out.strip()
        self.event("final_stop_begin", forced=True)
        problems = []
        self.guest("sudo systemctl enable --now arc-w0-live-block.service", timeout=60)
        stop = self.guest("sudo systemctl stop arc-node; sleep 2; systemctl is-active arc-node; pgrep -f 'legacy-bridge/releases' || echo no-node-process; curl -sf -m 2 http://127.0.0.1:9944/health >/dev/null && echo rpc-still-open || echo rpc-closed", timeout=120)
        if "inactive" not in stop.out or "no-node-process" not in stop.out or "rpc-closed" not in stop.out:
            problems.append(f"stop was not clean: {stop.out.strip()[-200:]}")
        rollback = self.guest(f"{GUEST_ARC}/bin/arc-node --legacy-bridge-rollback > {GUEST_WORK}/rollback.txt 2>&1; echo rc=$?; sha256sum {GUEST_ARC}/bin/arc-node | cut -d' ' -f1", timeout=120)
        if "rc=0" not in rollback.out or LEGACY_NODE_SHA256 not in rollback.out:
            problems.append(f"rollback did not restore v0.7.7: {rollback.out.strip()[-200:]}")
        started = self.guest("sudo systemctl start arc-node", timeout=60)
        v07_ok = started.ok and self.wait_health(90)
        exe = self.guest("readlink /proc/$(systemctl show -p MainPID --value arc-node)/exe", timeout=30).out.strip()
        if not v07_ok or exe != f"{GUEST_ARC}/bin/arc-node":
            problems.append(f"v0.7.7 did not run on its own data after the rollback (healthy={v07_ok}, exe={exe})")
        rebridge = self.guest(
            f"kept={GUEST_ARC}/legacy-bridge/arc-node-bridge-{self.bridge_version}; sha256sum $kept | cut -d' ' -f1; "
            f"cp $kept {GUEST_ARC}/bin/arc-node.new && mv {GUEST_ARC}/bin/arc-node.new {GUEST_ARC}/bin/arc-node && sudo systemctl restart arc-node",
            timeout=120,
        )
        bridged_ok = rebridge.ok and self.expect in rebridge.out and self.wait_health(90)
        new_address = self.guest(f"jq -r .node_address {GUEST_ARC}/legacy-bridge/nodes/headless-*/bridge-state.json", timeout=30).out.strip()
        if not bridged_ok or new_address != node_address:
            problems.append(f"re-bridging failed or changed the identity (ok={bridged_ok}, {node_address} -> {new_address})")
        self.guest("sudo systemctl stop arc-node", timeout=60)
        final_state = self.guest("systemctl is-active arc-node", timeout=30).out.strip()
        self.event("node_stopped", detail={"state": final_state})
        self.check("L11-stop-rollback", "stop is clean, --legacy-bridge-rollback restores v0.7.7 on its data, re-bridging keeps the identity, node stopped at the end",
                   "PASS" if not problems else "FAIL", f"{problems or 'all steps passed'}; final state {final_state}")

    # 7. collect ---------------------------------------------------------------------------------
    def collect(self, record: bool = True) -> None:
        self.say("collecting guest evidence")
        if not self.guest("true", timeout=30, retries=2, log=False).ok:
            (self.evidence / "COLLECT_FAILED.txt").write_text("the guest did not answer ssh at collection time\n", encoding="utf-8")
            self.dump_console()
            return
        result = self.guest(f"bash {GUEST_LAB}/collect.sh", timeout=300, log=False)
        (self.evidence / "collect.log").write_text(result.out, encoding="utf-8")
        tarball = self.evidence / "guest-evidence.tgz"
        if self.get_file(f"{GUEST_WORK}/evidence.tgz", tarball):
            target = self.evidence / "guest"
            target.mkdir(exist_ok=True)
            self.run_process(["tar", "-xzf", str(tarball), "-C", str(target)], check=False)
        self.pull_samples()
        for name in ("invariants-before.json", "invariants-after.json"):
            if not (self.evidence / name).exists():
                self.get_file(f"{GUEST_WORK}/{name}", self.evidence / name)
        if record:
            verdict = "PASS" if result.ok else "FAIL"
            self.check("L18-redaction", "the v0.7 seed was redacted from the collected evidence", verdict, result.out.strip()[-300:])

    # --- orchestration -------------------------------------------------------------------------
    def run(self) -> int:
        (self.evidence / "config-effective.json").write_text(json.dumps(effective_config(self.cfg, self.pins), indent=2, sort_keys=True) + "\n", encoding="utf-8")
        self.event("run_begin", detail={"profile": self.sb["profile"], "launcher_source": self.source, "deadline_min": self.profile["deadline_min"], "base_commit": self.cfg["base_commit"]})
        crashed = False
        try:
            self.phase("kvm", ("L01-kvm",), self.phase_kvm, fatal=True)
            self.phase("vm", (), self.phase_vm, fatal=True)
            self.phase("ship", (), self.phase_ship, fatal=True)
            self.phase("baseline", (), self.phase_baseline, fatal=True)
            self.phase("dry-run", ("L02-consume-dry-run",), self.phase_dry_run, fatal=True)
            self.phase("apply1", ("L03-interrupt-took-effect",), self.phase_apply1, fatal=True)
            self.phase("apply2", ("L04-interrupt-resumes",), self.phase_apply2, fatal=True)
            self.phase("updaters", ("L05-updater-1-noop", "L06-updater-2-noop"), self.phase_updaters)
            self.phase("kickstarts", ("L07-kickstart-1", "L08-kickstart-2", "L09-kickstart-3"), self.phase_kickstarts)
            self.phase("reboot", ("L10-reboot-boot-id-changed", "L12-pre-post-boot-logs"), self.phase_reboot, fatal=True)
            self.phase("steady", (), self.phase_steady)
            self.phase("final", ("L11-stop-rollback",), self.phase_final)
        except Fatal as error:
            crashed = True
            self.say(f"STOPPED EARLY: {error}")
            self.event("stopped_early", detail={"error": str(error)})
        finally:
            for check_id in REQUIRED_LIVE_IDS:
                if check_id not in self.recorded:
                    self.check(check_id, "not reached", "FAIL", "the run stopped before this check could be made")
            try:
                self.collect()
            except Exception as error:  # noqa: BLE001
                self.say(f"collection crashed: {error}")
        return 1 if crashed else 0


# ----------------------------------------------------------------------------------------------
# Subcommands
# ----------------------------------------------------------------------------------------------

def load_config(path: Path) -> dict:
    cfg = json.loads(path.read_text(encoding="utf-8"))
    errors = check_config.validate(cfg)
    if errors:
        raise SystemExit("invalid lab configuration:\n  " + "\n  ".join(errors))
    return cfg


def cmd_preflight(args: argparse.Namespace) -> int:
    args.evidence.mkdir(parents=True, exist_ok=True)
    if not Path("/dev/kvm").exists():
        message = "/dev/kvm is ABSENT on this runner: nested virtualization is not available, so a real guest reboot is not possible here"
        (args.evidence / "NO_KVM.txt").write_text(message + "\n", encoding="utf-8")
        print(f"::error::{message}")
        return 2
    subprocess.run(["sudo", "chmod", "0666", "/dev/kvm"], check=False)
    subprocess.run(["sudo", "apt-get", "update", "-q"], check=True)
    subprocess.run(["sudo", "apt-get", "install", "-y", "-q", "qemu-system-x86", "qemu-utils", "genisoimage"], check=True)
    print(subprocess.run(["qemu-system-x86_64", "--version"], stdout=subprocess.PIPE, check=True).stdout.decode().splitlines()[0])
    print(subprocess.run(["bash", "-c", "ls -l /dev/kvm; grep -c -E 'vmx|svm' /proc/cpuinfo; nproc; free -m | head -2; df -h / /mnt | cat"], stdout=subprocess.PIPE, check=False).stdout.decode())
    return 0


def cmd_run(args: argparse.Namespace) -> int:
    cfg = load_config(args.config)
    lab = Lab(cfg, args.evidence, args.work)
    return lab.run()


def cmd_collect(args: argparse.Namespace) -> int:
    cfg = load_config(args.config)
    lab = Lab(cfg, args.evidence, args.work)
    lab.collect(record=False)
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    for name, fn in (("preflight", cmd_preflight), ("run", cmd_run), ("collect", cmd_collect)):
        one = sub.add_parser(name)
        one.add_argument("--config", type=Path, default=LAB_DIR / "config.json")
        one.add_argument("--evidence", type=Path, required=True)
        one.add_argument("--work", type=Path, default=Path(os.environ.get("RUNNER_TEMP", "/tmp")) / "wave0-work")
        one.set_defaults(fn=fn)
    args = parser.parse_args(argv)
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
