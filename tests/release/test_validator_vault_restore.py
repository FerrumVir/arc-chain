#!/usr/bin/env python3
"""Hermetic adversarial tests for validator-vault restore and installation."""

from __future__ import annotations

import copy
import base64
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import time
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from types import SimpleNamespace
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[2]
HELPER = REPO_ROOT / "scripts" / "release" / "restore-validator-vault.py"
FREEZE_TEST = REPO_ROOT / "scripts" / "recovery" / "test_recovery_freeze.py"
SOURCE_COMMIT = "a" * 40
NODES = (
    ("NYC", "adf4ff16f997c871c16f3897e67881311d08f975f28ebdcf79e86ea9e3b99d0f", 6_666_667),
    ("LAX", "44d20543df6e76696da2ebbbd79e4243cd41729fa5b890e2618991e489314780", 6_666_667),
    ("AMS", "5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21", 6_666_667),
    ("LHR", "228787281308d6c1a560848c2c168814bde1b6153e9e65a286d7211f04628fdd", 6_666_667),
    ("NRT", "f03cbab49cf553a05541ddebc09b32a4c5507efb157d354b6d7f8c6682c32f5f", 6_666_666),
    ("SGP", "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b", 6_666_666),
)
HOSTS = {
    "NYC": "149.28.32.76",
    "LAX": "140.82.16.112",
    "AMS": "136.244.109.1",
    "LHR": "104.238.171.11",
    "NRT": "202.182.107.41",
    "SGP": "149.28.153.31",
}


def openssl_runtime_paths() -> tuple[Path, Path, Path]:
    executable_name = shutil.which("openssl")
    if executable_name is None:
        raise RuntimeError("OpenSSL is required for the hermetic vault fixture")
    executable = Path(executable_name).resolve(strict=True)
    if sys.platform == "darwin":
        output = subprocess.check_output(["/usr/bin/otool", "-L", str(executable)], text=True)
        dependencies = [line.strip().split(" (", 1)[0] for line in output.splitlines()[1:]]
    elif sys.platform.startswith("linux"):
        output = subprocess.check_output(["/usr/bin/ldd", str(executable)], text=True)
        dependencies = []
        for line in output.splitlines():
            fields = line.strip().split()
            if len(fields) >= 3 and fields[1] == "=>" and fields[2].startswith("/"):
                dependencies.append(fields[2])
    else:
        raise RuntimeError("unsupported test platform")
    libssl = next(Path(value).resolve(strict=True) for value in dependencies if Path(value).name.startswith("libssl"))
    libcrypto = next(Path(value).resolve(strict=True) for value in dependencies if Path(value).name.startswith("libcrypto"))
    return executable, libssl, libcrypto


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def canonical(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def create(path: Path, payload: bytes, mode: int = 0o600) -> None:
    path.write_bytes(payload)
    path.chmod(mode)


def run(command: list[str], *, success: bool = True) -> subprocess.CompletedProcess[str]:
    if len(command) >= 2 and command[0] == "python3" and Path(command[1]) == HELPER:
        helper = load_helper_module()
        stdout = io.StringIO()
        stderr = io.StringIO()
        with mock.patch.object(
            helper.artifact_provenance,
            "pretag_actions_proof",
            side_effect=lambda **kwargs: fake_live_proof(helper, kwargs),
        ), redirect_stdout(stdout), redirect_stderr(stderr):
            returncode = helper.main(command[2:])
        result = subprocess.CompletedProcess(
            command, returncode, stdout.getvalue(), stderr.getvalue()
        )
    else:
        result = subprocess.run(command, text=True, capture_output=True, check=False)
    if success and result.returncode != 0:
        raise AssertionError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


@contextlib.contextmanager
def fake_live_proof(helper, kwargs: dict):
    raw_zip = Path(kwargs["raw_actions_zip"])
    root = raw_zip.parent
    metadata_path = root / "BUILD-METADATA.json"
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    files = metadata["files"]
    artifact_id = kwargs["expected_artifact_id"]
    run_id = kwargs["expected_run_id"]
    run_attempt = kwargs["expected_run_attempt"]
    commit = kwargs["expected_commit"]
    archive_sha = "c" * 64
    zip_sha = digest(raw_zip)
    response_unix = int(time.time())
    request_nonce = f"{time.time_ns() & ((1 << 48) - 1):012X}"
    responses = [
        {
            "label": label,
            "body_sha256": hashlib.sha256(label.encode()).hexdigest(),
            "response_unix": response_unix,
            "request_id": f"{request_nonce}:{index:04X}:1234",
            "cache_control": "public, max-age=60",
            "age": 0,
        }
        for index, label in enumerate(
            ("workflow", "run", "artifact", "protected_main")
        )
    ]
    provenance = {
        "schema": helper.artifact_provenance.PROVENANCE_SCHEMA,
        "live": {
            "repository": helper.REPOSITORY,
            "protected_branch": "main",
            "commit": commit,
            "workflow_id": 42,
            "workflow_path": helper.artifact_provenance.WORKFLOW_PATH,
            "run_id": run_id,
            "run_attempt": run_attempt,
            "artifact_id": artifact_id,
            "artifact_name": (
                f"arc-pretag-headless-linux-x86_64-{commit}-{run_id}-{run_attempt}-{archive_sha}"
            ),
            "artifact_digest": f"sha256:{zip_sha}",
            "artifact_size_in_bytes": raw_zip.stat().st_size,
            "api_verified_at_unix": response_unix,
        },
        "api": {
            "origin": helper.artifact_provenance.API_ORIGIN,
            "anonymous": True,
            "redirects_followed": False,
            "max_age_seconds": helper.artifact_provenance.MAX_API_AGE_SECONDS,
            "curl_sha256": kwargs["curl_sha256"],
            "ca_bundle_sha256": kwargs["ca_bundle_sha256"],
            "responses": responses,
        },
        "artifact": {
            "kind": "headless",
            "platform": "linux-x86_64",
            "version": helper.VERSION,
            "raw_actions_zip_sha256": zip_sha,
            "raw_actions_zip_size": raw_zip.stat().st_size,
            "archive_sha256": archive_sha,
            "build_metadata_sha256": digest(metadata_path),
            "files": files,
        },
    }
    final = copy.deepcopy(provenance)
    for response in final["api"]["responses"]:
        response["request_id"] = response["request_id"].replace("1234", "DCBA")
    proof = SimpleNamespace(
        build_metadata=metadata,
        build_metadata_path=metadata_path,
        payloads={
            "arc-node-linux-x86_64": root / "arc-node-linux-x86_64",
            "arc-cli-linux-x86_64": root / "arc-cli-linux-x86_64",
            "genesis.toml": root / "genesis.toml",
        },
        provenance=provenance,
    )
    proof.recheck = lambda: SimpleNamespace(
        value=final,
        canonical_bytes=helper.canonical_json_bytes(final),
        path=None,
    )
    yield proof


def load_freeze_fixture_module():
    spec = importlib.util.spec_from_file_location("arc_vault_freeze_fixture", FREEZE_TEST)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_helper_module():
    spec = importlib.util.spec_from_file_location("arc_validator_vault_helper_test", HELPER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    import sys

    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class Fixture:
    cert_root: Path
    certificate: Path
    private_key: Path
    openssl: Path
    libssl: Path
    libcrypto: Path

    @classmethod
    def make_certificate(cls) -> None:
        cls.openssl, cls.libssl, cls.libcrypto = openssl_runtime_paths()
        cls.cert_root = Path(tempfile.mkdtemp(prefix="arc-vault-cert-fixture.")).resolve()
        cls.cert_root.chmod(0o700)
        cls.certificate = cls.cert_root / "restore.cert.pem"
        cls.private_key = cls.cert_root / "restore.key.pem"
        run(
            [
                str(cls.openssl),
                "req",
                "-x509",
                "-newkey",
                "rsa:3072",
                "-nodes",
                "-days",
                "2",
                "-subj",
                "/CN=ARC validator vault hermetic fixture",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-addext",
                "keyUsage=critical,keyEncipherment,dataEncipherment",
                "-addext",
                "extendedKeyUsage=emailProtection",
                "-keyout",
                str(cls.private_key),
                "-out",
                str(cls.certificate),
            ]
        )
        cls.certificate.chmod(0o600)
        cls.private_key.chmod(0o600)

    @classmethod
    def remove_certificate(cls) -> None:
        shutil.rmtree(cls.cert_root)

    def __init__(self, root: Path) -> None:
        self.root = root
        self.root.mkdir(parents=True, exist_ok=True)
        self.cms = root / "vault.tar.cms"
        self.rewrap_receipt = root / "REWRAP-RECEIPT.json"
        self.genesis = root / "genesis.toml"
        self.cli = root / "arc-cli-linux-x86_64"
        self.node = root / "arc-node-linux-x86_64"
        self.metadata = root / "BUILD-METADATA.json"
        self.actions_zip = root / "artifact.zip"
        self.output = root / "restored"
        self.archive_members = self.valid_members()
        self.write_cli()
        create(self.node, b"exact pre-tag node fixture\n", 0o500)
        create(self.actions_zip, b"shared verifier raw Actions ZIP fixture\n", 0o400)
        self.write_genesis()
        self.write_cms(self.archive_members)
        self.write_metadata()
        self.write_rewrap_receipt()

    @staticmethod
    def key_json(index: int, address: str) -> bytes:
        # These are deliberately non-production fixture bytes.  The mock exact
        # pre-tag CLI performs the same public-only output contract as `arc
        # keygen --verify-keyfile`; production bytes are never used by tests.
        return canonical(
            {
                "scheme": "ed25519",
                "secret_key": f"{index + 1:064x}",
                "public_key": f"{index + 101:064x}",
                "address": address,
            }
        )

    @classmethod
    def valid_members(cls) -> list[tuple[tarfile.TarInfo, bytes]]:
        result: list[tuple[tarfile.TarInfo, bytes]] = []
        # Deliberately reverse archive order and use opaque filenames.  Mapping
        # must come from CLI-verified public identity, never filename/order.
        for index, (_, address, _) in reversed(list(enumerate(NODES))):
            payload = cls.key_json(index, address)
            member = tarfile.TarInfo(f"private/identity-{index + 1}.json")
            member.mode = 0o600
            member.size = len(payload)
            result.append((member, payload))
        directory = tarfile.TarInfo("private")
        directory.type = tarfile.DIRTYPE
        directory.mode = 0o700
        result.insert(0, (directory, b""))
        return result

    def write_cli(self) -> None:
        script = textwrap.dedent(
            """\
            #!/usr/bin/env python3
            import json, os, stat, sys
            if len(sys.argv) != 4 or sys.argv[1:3] != ["keygen", "--verify-keyfile"]:
                raise SystemExit(90)
            path = sys.argv[3]
            details = os.lstat(path)
            if not stat.S_ISREG(details.st_mode) or stat.S_IMODE(details.st_mode) != 0o600:
                raise SystemExit(91)
            with open(path, encoding="utf-8") as handle:
                value = json.load(handle)
            if set(value) != {"scheme", "secret_key", "public_key", "address"}:
                raise SystemExit(92)
            if value["scheme"] != "ed25519" or any(
                not isinstance(value[name], str) or len(value[name]) != 64
                for name in ("secret_key", "public_key", "address")
            ):
                raise SystemExit(93)
            print(value["address"])
            """
        ).encode()
        create(self.cli, script, 0o700)

    def write_genesis(self) -> None:
        lines = [
            "[chain]",
            'name = "arc-testnet"',
            'chain_id = "0x415243"',
            "validator_set_complete = true",
            "community_rewards_v1_activation_height = 137146",
            "",
        ]
        for _, address, _ in NODES:
            lines.extend(("[[accounts]]", f'address = "{address}"', "balance = 0", ""))
        for _, address, stake in NODES:
            lines.extend(("[[validators]]", f'address = "{address}"', f"stake = {stake}", ""))
        create(self.genesis, "\n".join(lines).encode())

    def write_metadata(self) -> None:
        metadata = {
            "schema": "arc.pretag.artifact.v1",
            "kind": "headless",
            "repository": "FerrumVir/arc-chain",
            "commit": SOURCE_COMMIT,
            "platform": "linux-x86_64",
            "rust_target": "x86_64-unknown-linux-gnu",
            "version": "0.8.0",
            "workflow_run_id": 1234,
            "workflow_run_attempt": 1,
            "files": {
                "arc-node-linux-x86_64": digest(self.node),
                "arc-cli-linux-x86_64": digest(self.cli),
                "genesis.toml": digest(self.genesis),
            },
        }
        create(self.metadata, canonical(metadata))

    def archive(self, members: list[tuple[tarfile.TarInfo, bytes]], *, mode: str = "w:") -> Path:
        path = self.root / "vault.tar"
        with tarfile.open(path, mode=mode, format=tarfile.PAX_FORMAT) as archive:
            for member, payload in members:
                archive.addfile(member, io.BytesIO(payload) if member.isfile() else None)
        path.chmod(0o600)
        return path

    def write_cms(
        self,
        members: list[tuple[tarfile.TarInfo, bytes]],
        *,
        archive_mode: str = "w:",
        cipher: str = "-aes-256-gcm",
    ) -> None:
        plain = self.archive(members, mode=archive_mode)
        command = [
            str(self.openssl),
            "cms",
            "-encrypt",
            "-binary",
            "-outform",
            "DER",
            cipher,
            "-recip",
            str(self.certificate),
            "-keyopt",
            "rsa_padding_mode:oaep",
            "-keyopt",
            "rsa_oaep_md:sha256",
            "-in",
            str(plain),
            "-out",
            str(self.cms),
        ]
        run(command)
        self.cms.chmod(0o600)

    def write_rewrap_receipt(self) -> None:
        receipt = {
            "schema": "arc.validator-vault-rewrap.v1",
            "source_commit": SOURCE_COMMIT,
            "source_ciphertext_sha256": "b" * 64,
            "restore_cert_sha256": digest(self.certificate),
            "cms_sha256": digest(self.cms),
            "key_transport": "RSA-OAEP-SHA256",
            "content_encryption": "AES-256-GCM",
        }
        create(self.rewrap_receipt, canonical(receipt))

    def restore_command(self, *, output: Path | None = None) -> list[str]:
        return [
            "python3",
            str(HELPER),
            "restore",
            "--cms",
            str(self.cms),
            "--expected-cms-sha256",
            digest(self.cms),
            "--rewrap-receipt",
            str(self.rewrap_receipt),
            "--source-main-sha",
            SOURCE_COMMIT,
            *self.proof_arguments(),
            "--restore-certificate",
            str(self.certificate),
            "--restore-private-key",
            str(self.private_key),
            "--openssl",
            str(self.openssl),
            "--openssl-sha256",
            digest(self.openssl),
            "--openssl-libssl",
            str(self.libssl),
            "--openssl-libssl-sha256",
            digest(self.libssl),
            "--openssl-libcrypto",
            str(self.libcrypto),
            "--openssl-libcrypto-sha256",
            digest(self.libcrypto),
            "--output-dir",
            str(self.output if output is None else output),
        ]

    def proof_arguments(self) -> list[str]:
        return [
            "--raw-actions-zip",
            str(self.actions_zip),
            "--pretag-run-id",
            "1234",
            "--pretag-run-attempt",
            "1",
            "--pretag-artifact-id",
            "987654",
            "--curl",
            "/usr/bin/curl",
            "--curl-sha256",
            "e" * 64,
            "--ca-bundle",
            "/private/etc/ssl/cert.pem",
            "--ca-bundle-sha256",
            "f" * 64,
        ]

    def restore(self) -> subprocess.CompletedProcess[str]:
        return run(self.restore_command())

    def write_freeze_inputs(self) -> tuple[Path, str, Path, str, Path, str, Path, str]:
        module = load_freeze_fixture_module()
        value = module.plan_value()
        value["source_commit"] = SOURCE_COMMIT
        for row, (node, _address, _stake) in zip(value["nodes"], NODES):
            row["host"] = HOSTS[node]
        plan = self.root / "freeze-plan.json"
        create(plan, canonical(value), 0o444)
        plan_sha = digest(plan)
        plan_sidecar = self.root / "freeze-plan.json.sha256"
        create(plan_sidecar, f"{plan_sha}  freeze-plan.json\n".encode(), 0o444)
        capture_id = hashlib.sha256(
            b"ARC recovery capture v2\0" + bytes.fromhex(plan_sha)
        ).hexdigest()
        helper = load_helper_module()
        status_root = self.root / "fresh-stopped-status"
        status_root.mkdir(mode=0o700)
        evidence_rows = []
        for index, row in enumerate(value["nodes"], start=1):
            complete_sha = hashlib.sha256(f"complete:{row['name']}:{index}".encode()).hexdigest()
            files_sha = hashlib.sha256(f"files:{row['name']}:{index}".encode()).hexdigest()
            argv = [
                "stopped-status",
                capture_id,
                row["name"],
                plan_sha,
                *(str(row[field]) for field in helper.STOPPED_STATUS_ARGV_FIELDS),
            ]
            status = {
                "capture_id": capture_id,
                "freeze_plan_sha256": plan_sha,
                "node": row["name"],
                "restart_fenced": True,
                "schema": "arc.recovery.offline-stop-status.v1",
                "stake": row["stake"],
                "stop_complete_sha256": complete_sha,
                "stop_files_sha256": files_sha,
                "stop_schema": "arc.recovery.offline-stop.v4",
                "stopped": True,
                "validator_address": row["validator_address"],
            }
            status_raw = canonical(status)
            create(status_root / f"{row['host']}.json", status_raw, 0o600)
            evidence_rows.append(
                {
                    "node": row["name"],
                    "host": row["host"],
                    "validator_address": row["validator_address"],
                    "stake": row["stake"],
                    "stop_complete_sha256": complete_sha,
                    "stop_files_sha256": files_sha,
                    "stopped_status_sha256": hashlib.sha256(status_raw).hexdigest(),
                    "stopped_status_argv_sha256": hashlib.sha256(canonical(argv)).hexdigest(),
                }
            )
        first_quarantine = "2026-08-28T12:00:00Z"
        all_stopped = "2026-08-28T12:02:30Z"
        challenge = "9" * 64
        public_receipt_sha = "8" * 64
        authenticated_rows = []
        for row in value["nodes"]:
            auth_proof = {
                "schema": "arc.recovery.authenticated-legacy-height-bracket.v1",
                "capture_id": capture_id,
                "node": row["name"],
                "freeze_plan_sha256": plan_sha,
                "challenge": challenge,
            }
            authenticated_rows.append(
                {
                    "node": row["name"],
                    "host": row["host"],
                    "proof": auth_proof,
                    "proof_sha256": hashlib.sha256(canonical(auth_proof)).hexdigest(),
                }
            )
        authenticated = {
            "schema": "arc.recovery.authenticated-legacy-height-fleet.v1",
            "source_main_commit": SOURCE_COMMIT,
            "freeze_plan_sha256": plan_sha,
            "capture_id": capture_id,
            "legacy_public_height_receipt_sha256": public_receipt_sha,
            "challenge": challenge,
            "started_at": "2026-08-28T12:00:01Z",
            "completed_at": "2026-08-28T12:00:02Z",
            "conservative_height_floor": 105,
            "nodes": authenticated_rows,
        }
        quarantine_challenge = {
            "schema": "arc.recovery.legacy-network-quarantine-challenge.v1",
            "freeze_plan_sha256": plan_sha,
            "capture_id": capture_id,
            "challenge": challenge,
        }
        stability = {
            "schema": "arc.recovery.legacy-network-quarantine-stability.v1",
            "source_main_commit": SOURCE_COMMIT,
            "freeze_plan_sha256": plan_sha,
            "capture_id": capture_id,
            "challenge": challenge,
            "interval_seconds": 120,
            "sample_count": 2,
            "started_at": "2026-08-28T12:00:03Z",
            "completed_at": "2026-08-28T12:02:03Z",
            "monotonic_elapsed_ns": 120_000_000_000,
            "fleet_heads": [
                {"node": row["name"], "host": row["host"]} for row in value["nodes"]
            ],
            "nodes": [
                {"node": row["name"], "host": row["host"]} for row in value["nodes"]
            ],
            "global_absence_claimed": False,
        }
        inventory = []

        def sealed(value_object: dict, node: str, role: str) -> dict:
            raw = canonical(value_object)
            root = hashlib.sha256(raw).hexdigest()
            inventory.append({"node": node, "role": role, "sha256": root, "size": len(raw)})
            return {"value": value_object, "sha256": root}

        authenticated_sealed = sealed(
            authenticated, "fleet", "authenticated-prefence-height-cross-proof"
        )
        challenge_sealed = sealed(
            quarantine_challenge, "fleet", "network-quarantine-challenge"
        )
        stability_sealed = sealed(
            stability, "fleet", "network-quarantine-stability-proof"
        )
        bundle_nodes = []
        for plan_row, stopped_row in zip(value["nodes"], evidence_rows):
            node = plan_row["name"]
            host = plan_row["host"]
            stopped_status = json.loads((status_root / f"{host}.json").read_text())
            quarantine_status = {
                "schema": "arc.recovery.legacy-network-quarantine-status.v1",
                "capture_id": capture_id,
                "node": node,
                "freeze_plan_sha256": plan_sha,
                "active": True,
                "enabled": True,
            }
            post_status = dict(quarantine_status)
            external = {
                "schema": "arc.recovery.legacy-network-quarantine-external-proof.v1",
                "capture_id": capture_id,
                "node": node,
                "host": host,
                "freeze_plan_sha256": plan_sha,
                "challenge": challenge,
                "global_absence_claimed": False,
            }
            public_cross = {
                "schema": "arc.recovery.legacy-network-quarantine-public-cross-proof.v1",
                "capture_id": capture_id,
                "node": node,
                "freeze_plan_sha256": plan_sha,
                "challenge": challenge,
                "global_absence_claimed": False,
            }
            persisted = {
                "schema": "arc.recovery.persisted-legacy-head.v1",
                "source_main_commit": SOURCE_COMMIT,
                "capture_id": capture_id,
                "node": node,
                "freeze_plan_sha256": plan_sha,
                "writer_stopped": True,
                "restart_barrier_active": True,
                "network_quarantine_active": True,
                "global_absence_claimed": False,
            }
            bundle_nodes.append(
                {
                    "node": node,
                    "host": host,
                    "stopped_status": sealed(stopped_status, node, "stopped-status"),
                    "quarantine_status": sealed(quarantine_status, node, "quarantine-status"),
                    "post_proof_quarantine_status": sealed(
                        post_status, node, "post-proof-quarantine-status"
                    ),
                    "external_quarantine_proof": sealed(
                        external, node, "external-quarantine-proof"
                    ),
                    "public_cross_proof": sealed(public_cross, node, "public-cross-proof"),
                    "persisted_head": sealed(persisted, node, "persisted-head"),
                }
            )
        inventory_root = hashlib.sha256(
            canonical(
                {
                    "schema": "arc.recovery.legacy-maintenance-evidence-inventory.v1",
                    "objects": inventory,
                }
            )
        ).hexdigest()
        bundle = {
            "schema": "arc.recovery.legacy-maintenance-evidence-bundle.v1",
            "source_main_commit": SOURCE_COMMIT,
            "freeze_plan_sha256": plan_sha,
            "capture_id": capture_id,
            "first_quarantine_started_at": first_quarantine,
            "all_controlled_stopped_at": all_stopped,
            "challenge": challenge,
            "authenticated_prefence_height_cross_proof": authenticated_sealed,
            "network_quarantine_challenge": challenge_sealed,
            "quarantine_stability_proof": stability_sealed,
            "nodes": bundle_nodes,
            "object_inventory": inventory,
            "aggregate_root_sha256": inventory_root,
        }
        bundle_path = self.root / "legacy-maintenance-evidence-bundle.json"
        create(bundle_path, canonical(bundle), 0o400)
        bundle_sha = digest(bundle_path)
        bundle_sidecar = self.root / "legacy-maintenance-evidence-bundle.json.sha256"
        create(bundle_sidecar, f"{bundle_sha}  {bundle_path.name}\n".encode(), 0o400)

        head = {"height": 105, "block_hash": "6" * 64, "state_root": "5" * 64}
        boundary_nodes = []
        for origin, bundle_row, authenticated_row in zip(
            ({"node": row["name"], "host": row["host"], "origin": f"http://{row['host']}:9090"}
             for row in value["nodes"]),
            bundle_nodes,
            authenticated_rows,
        ):
            observation = {"tuple": head, "evidence_sha256": bundle_row["public_cross_proof"]["sha256"]}
            boundary_nodes.append(
                {
                    **origin,
                    "public_observation": observation,
                    "authenticated_prefence_proof_sha256": authenticated_row["proof_sha256"],
                    "network_quarantine_receipt_sha256": "4" * 64,
                    "quarantine_status_sha256": bundle_row["quarantine_status"]["sha256"],
                    "post_proof_quarantine_status_sha256": bundle_row["post_proof_quarantine_status"]["sha256"],
                    "external_quarantine_proof_sha256": bundle_row["external_quarantine_proof"]["sha256"],
                    "public_cross_proof_sha256": bundle_row["public_cross_proof"]["sha256"],
                    "initial_post_quarantine_head": {
                        "tuple": head,
                        "evidence_sha256": bundle_row["quarantine_status"]["sha256"],
                    },
                    "post_quarantine_head": observation,
                    "final_persisted_head": {
                        "tuple": head,
                        "evidence_sha256": bundle_row["persisted_head"]["sha256"],
                    },
                }
            )
        boundary = {
            "schema": "arc.recovery.legacy-maintenance-boundary.v1",
            "source_main_commit": SOURCE_COMMIT,
            "freeze_plan_sha256": plan_sha,
            "capture_id": capture_id,
            "first_quarantine_started_at": first_quarantine,
            "all_controlled_stopped_at": all_stopped,
            "created_at": "2026-08-28T12:02:31Z",
            "official_origin_scope": {
                "global_absence_claimed": False,
                "origins": [
                    {"node": row["name"], "host": row["host"], "origin": f"http://{row['host']}:9090"}
                    for row in value["nodes"]
                ],
            },
            "legacy_public_height_receipt": {
                "schema": "arc.recovery.legacy-public-height.v1",
                "sha256": public_receipt_sha,
                "completed_at": "2026-08-28T11:59:59Z",
                "observed_max_height": 100,
            },
            "authenticated_prefence_height_cross_proof_sha256": authenticated_sealed["sha256"],
            "legacy_maintenance_evidence_bundle_sha256": bundle_sha,
            "network_quarantine_stability_proof_sha256": stability_sealed["sha256"],
            "network_quarantine_challenge": challenge,
            "tools": {
                "remote_helper_sha256": value["remote_helper_sha256"],
                "inspector_binary_sha256": "3" * 64,
                "genesis_sha256": "2" * 64,
                "validator_public_keys_sha256": "1" * 64,
                "legacy_validator_set_sha256": "0" * 64,
                "orchestrator_sha256": value["orchestrator_sha256"],
                "rollout_tool_sha256": value["rollout_tool_sha256"],
                "rollout_schema_sha256": value["rollout_schema_sha256"],
            },
            "nodes": boundary_nodes,
            "evidence_heights": [
                {"node": row["name"], "label": "final_persisted_head", "height": 105,
                 "evidence_sha256": bundle_row["persisted_head"]["sha256"]}
                for row, bundle_row in zip(value["nodes"], bundle_nodes)
            ],
            "observed_cutoff_height": 105,
            "continuity_safety_margin": 128,
            "continuity_safety_margin_policy": {
                "prune_depth": 100,
                "commit_rule_rounds": 2,
                "operational_headroom": 26,
                "cryptographic_global_absence_proof": False,
            },
            "legacy_public_max_height": 233,
            "global_absence_claimed": False,
            "reopening_policy": {
                "required_validator_count": 6,
                "height_relation": "strictly-greater-than-legacy_public_max_height",
                "required_equal_fields": ["block_hash", "state_root"],
            },
            "late_fork_circuit": {
                "monitor_scope": "retired-and-community-legacy-sources",
                "trigger": "self-consistent-legacy-fork-candidate-above-observed-cutoff-height",
                "action": "enter-maintenance-preserve-and-offline-validate",
                "rewrite_v3_history_allowed": False,
            },
            "threat_model": {
                "trusted_host_root_required": True,
                "sealed_reviewed_legacy_binary_non_adversarial": True,
                "quarantine_purpose": "operational-network-isolation",
                "hostile_root_containment_claimed": False,
            },
        }
        boundary_path = self.root / "legacy-maintenance-boundary.json"
        create(boundary_path, canonical(boundary), 0o400)
        boundary_sha = digest(boundary_path)
        boundary_sidecar = self.root / "legacy-maintenance-boundary.json.sha256"
        create(boundary_sidecar, f"{boundary_sha}  {boundary_path.name}\n".encode(), 0o400)

        proof = {
            "schema": "arc.validator-vault.offline-stop-evidence.v2",
            "source_main_commit": SOURCE_COMMIT,
            "freeze_plan_sha256": plan_sha,
            "freeze_plan_sidecar_sha256": digest(plan_sidecar),
            "capture_id": capture_id,
            "remote_helper_sha256": value["remote_helper_sha256"],
            "remote_helper_path": (
                f"/root/.arc-recovery-helpers/{value['remote_helper_sha256']}/archive-node.sh"
            ),
            "first_quarantine_started_at": first_quarantine,
            "all_controlled_stopped_at": all_stopped,
            "legacy_height_cross_proof": authenticated,
            "legacy_maintenance_boundary": boundary,
            "legacy_maintenance_boundary_sha256": boundary_sha,
            "legacy_maintenance_evidence_bundle_sha256": bundle_sha,
            "nodes": evidence_rows,
        }
        proof_path = self.root / "offline-stop-evidence.json"
        create(proof_path, canonical(proof), 0o400)
        proof_sha = digest(proof_path)
        create(
            self.root / "offline-stop-evidence.json.sha256",
            f"{proof_sha}  offline-stop-evidence.json\n".encode(),
            0o400,
        )
        return (
            plan, plan_sha, bundle_path, bundle_sha, boundary_path, boundary_sha,
            proof_path, proof_sha,
        )

    def write_transport_mocks(self) -> tuple[Path, Path, Path, Path, Path]:
        remote_root = self.root / "mock-remotes"
        remote_root.mkdir()
        status_root = self.root / "fresh-stopped-status"
        ssh = self.root / "mock-ssh"
        scp = self.root / "mock-scp"
        ssh_script = textwrap.dedent(
            f"""\
            #!/usr/bin/env python3
            import hashlib, os, pathlib, stat, sys
            root=pathlib.Path({str(remote_root)!r})
            status_root=pathlib.Path({str(status_root)!r})
            forbidden=("DYLD_INSERT_LIBRARIES","DYLD_LIBRARY_PATH","LD_PRELOAD","LD_LIBRARY_PATH",
                       "OPENSSL_CONF","OPENSSL_MODULES","OPENSSL_ENGINES","SSH_AUTH_SOCK")
            if any(name in os.environ for name in forbidden): raise SystemExit(79)
            if pathlib.Path(os.environ["PATH"].split(":",1)[0]).name != "transport": raise SystemExit(78)
            required=("BatchMode=yes","StrictHostKeyChecking=yes","PasswordAuthentication=no",
                      "KbdInteractiveAuthentication=no","ForwardAgent=no","ClearAllForwardings=yes",
                      "IdentitiesOnly=yes","IdentityAgent=none","PreferredAuthentications=publickey",
                      "HostKeyAlgorithms=ssh-ed25519","PubkeyAcceptedAlgorithms=ssh-ed25519",
                      "UpdateHostKeys=no")
            joined="\\n".join(sys.argv)
            if any(value not in joined for value in required): raise SystemExit(80)
            if sys.argv.count("-F")!=1 or sys.argv[sys.argv.index("-F")+1]!="/dev/null": raise SystemExit(87)
            if sys.argv.count("-i")!=1: raise SystemExit(88)
            identity=pathlib.Path(sys.argv[sys.argv.index("-i")+1])
            if not identity.is_absolute() or identity.name!="id_ed25519" or stat.S_IMODE(identity.stat().st_mode)!=0o400:
                raise SystemExit(89)
            sh_index=next(index for index,value in enumerate(sys.argv) if pathlib.Path(value).name=="sh")
            host=sys.argv[sh_index-1].split("@",1)[1]
            marker=sys.argv.index("--",sh_index)
            args=sys.argv[marker+1:]
            op=args[0]
            host_root=root/host
            host_root.mkdir(parents=True,exist_ok=True)
            final=host_root/"validator-key.json"
            def hexdigest(path): return hashlib.sha256(path.read_bytes()).hexdigest()
            if op=="stopped-status":
                if args[1] != f"/root/.arc-recovery-helpers/{{args[2]}}/archive-node.sh": raise SystemExit(85)
                if args[3] != "stopped-status": raise SystemExit(86)
                sys.stdout.write((status_root/f"{{host}}.json").read_text())
            elif op=="probe":
                expected=args[2]
                if final.exists():
                    if final.is_symlink() or not final.is_file() or stat.S_IMODE(final.stat().st_mode)!=0o600 or hexdigest(final)!=expected:
                        raise SystemExit(81)
                    print("VERIFIED")
                else: print("MISSING")
            elif op=="prepare":
                expected=args[2]
                temporary=host_root/f".validator-key.upload.{{expected}}.FIXTURE"
                temporary.touch(exist_ok=False); temporary.chmod(0o600)
                print(f"/etc/arc-v3/{{temporary.name}}")
            elif op=="commit":
                temporary=host_root/pathlib.PurePosixPath(args[1]).name
                expected=args[3]
                if hexdigest(temporary)!=expected: raise SystemExit(82)
                try: os.link(temporary,final)
                except FileExistsError:
                    if hexdigest(final)!=expected: raise SystemExit(83)
                temporary.unlink(missing_ok=True); final.chmod(0o600); print("VERIFIED")
            elif op=="cleanup":
                (host_root/pathlib.PurePosixPath(args[1]).name).unlink(missing_ok=True)
            else: raise SystemExit(84)
            """
        )
        scp_script = textwrap.dedent(
            f"""\
            #!/usr/bin/env python3
            import os, pathlib, shutil, sys
            root=pathlib.Path({str(remote_root)!r})
            forbidden=("DYLD_INSERT_LIBRARIES","DYLD_LIBRARY_PATH","LD_PRELOAD","LD_LIBRARY_PATH",
                       "OPENSSL_CONF","OPENSSL_MODULES","OPENSSL_ENGINES","SSH_AUTH_SOCK")
            if any(name in os.environ for name in forbidden): raise SystemExit(89)
            if pathlib.Path(os.environ["PATH"].split(":",1)[0]).name != "transport": raise SystemExit(88)
            joined="\\n".join(sys.argv)
            if "StrictHostKeyChecking=yes" not in joined or "BatchMode=yes" not in joined: raise SystemExit(90)
            if sys.argv.count("-F")!=1 or sys.argv[sys.argv.index("-F")+1]!="/dev/null": raise SystemExit(93)
            if sys.argv.count("-i")!=1 or "IdentityAgent=none" not in joined or "IdentitiesOnly=yes" not in joined:
                raise SystemExit(94)
            if "HostKeyAlgorithms=ssh-ed25519" not in joined or "PubkeyAcceptedAlgorithms=ssh-ed25519" not in joined or "UpdateHostKeys=no" not in joined:
                raise SystemExit(96)
            identity=pathlib.Path(sys.argv[sys.argv.index("-i")+1])
            if not identity.is_absolute() or identity.name!="id_ed25519" or (identity.stat().st_mode & 0o777)!=0o400:
                raise SystemExit(95)
            ssh=pathlib.Path(sys.argv[sys.argv.index("-S")+1])
            if not ssh.is_absolute() or ssh.name!="ssh" or ssh.parent.name!="transport": raise SystemExit(92)
            marker=sys.argv.index("--")
            source=pathlib.Path(sys.argv[marker+1])
            target=sys.argv[marker+2]
            userhost,remote=target.split(":",1)
            host=userhost.split("@",1)[1]
            failure=root/"FAIL-HOST"
            if failure.exists() and failure.read_text().strip()==host: raise SystemExit(91)
            destination=root/host/pathlib.PurePosixPath(remote).name
            with (root/"SOURCES").open("a",encoding="utf-8") as log: log.write(str(source)+"\\n")
            shutil.copyfile(source,destination); destination.chmod(0o600)
            """
        )
        create(ssh, ssh_script.encode(), 0o700)
        create(scp, scp_script.encode(), 0o700)
        known_hosts = self.root / "known_hosts"
        host_lines = []
        for index, host in enumerate(HOSTS.values(), start=1):
            blob = b"\x00\x00\x00\x0bssh-ed25519\x00\x00\x00\x20" + bytes([index]) * 32
            host_lines.append(
                f"{host} ssh-ed25519 {base64.b64encode(blob).decode('ascii')}"
            )
        create(known_hosts, ("\n".join(host_lines) + "\n").encode(), 0o400)
        identity = self.root / "id_ed25519"
        create(
            identity,
            b"-----BEGIN OPENSSH PRIVATE KEY-----\nfixture-only\n-----END OPENSSH PRIVATE KEY-----\n",
            0o400,
        )
        return ssh, scp, known_hosts, identity, remote_root

    def install_command(self, receipt_output: Path) -> tuple[list[str], Path]:
        (
            plan,
            plan_sha,
            maintenance_bundle,
            maintenance_bundle_sha,
            maintenance_boundary,
            maintenance_boundary_sha,
            proof,
            proof_sha,
        ) = self.write_freeze_inputs()
        ssh, scp, known_hosts, identity, remote_root = self.write_transport_mocks()
        command = [
            "python3",
            str(HELPER),
            "install",
            "--restore-receipt",
            str(self.output / "RESTORE-RECEIPT.json"),
            "--source-main-sha",
            SOURCE_COMMIT,
            *self.proof_arguments(),
            "--freeze-plan",
            str(plan),
            "--freeze-plan-sidecar",
            str(plan) + ".sha256",
            "--freeze-plan-sha256",
            plan_sha,
            "--legacy-maintenance-evidence-bundle",
            str(maintenance_bundle),
            "--legacy-maintenance-evidence-bundle-sidecar",
            str(maintenance_bundle) + ".sha256",
            "--legacy-maintenance-evidence-bundle-sha256",
            maintenance_bundle_sha,
            "--legacy-maintenance-boundary",
            str(maintenance_boundary),
            "--legacy-maintenance-boundary-sidecar",
            str(maintenance_boundary) + ".sha256",
            "--legacy-maintenance-boundary-sha256",
            maintenance_boundary_sha,
            "--offline-stop-evidence",
            str(proof),
            "--offline-stop-evidence-sidecar",
            str(proof) + ".sha256",
            "--offline-stop-evidence-sha256",
            proof_sha,
            "--known-hosts",
            str(known_hosts),
            "--known-hosts-sha256",
            digest(known_hosts),
            "--ssh-identity",
            str(identity),
            "--ssh-identity-sha256",
            digest(identity),
            "--ssh",
            str(ssh),
            "--ssh-sha256",
            digest(ssh),
            "--scp",
            str(scp),
            "--scp-sha256",
            digest(scp),
            "--receipt-output",
            str(receipt_output),
        ]
        return command, remote_root


class ValidatorVaultRestoreTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        Fixture.make_certificate()

    @classmethod
    def tearDownClass(cls) -> None:
        Fixture.remove_certificate()

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="arc-vault-restore-test.")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.fixture = Fixture(self.root)

    def rewrite_cms(self, members: list[tuple[tarfile.TarInfo, bytes]], **kwargs) -> None:
        self.fixture.write_cms(members, **kwargs)
        self.fixture.write_rewrap_receipt()

    def assert_restore_rejected(self, expected: str) -> None:
        result = run(self.fixture.restore_command(), success=False)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(expected, result.stderr)
        self.assertFalse(self.fixture.output.exists())

    def test_restore_produces_only_six_keys_public_manifest_and_private_receipt(self) -> None:
        result = self.fixture.restore()
        self.assertIn("six reviewed identities verified", result.stdout)
        self.assertEqual(result.stderr, "")
        self.assertEqual(stat.S_IMODE(self.fixture.output.stat().st_mode), 0o700)
        keys = sorted((self.fixture.output / "keys").iterdir())
        self.assertEqual(
            [path.name for path in keys],
            sorted(f"{name}.validator-key.json" for name, _, _ in NODES),
        )
        self.assertTrue(all(stat.S_IMODE(path.stat().st_mode) == 0o600 for path in keys))
        public_path = self.fixture.output / "validator-public-keys.json"
        public = json.loads(public_path.read_text())
        self.assertEqual(
            [set(row) for row in public],
            [{"address", "public_key", "stake"}] * 6,
        )
        self.assertEqual([row["address"] for row in public], [address for _, address, _ in NODES])
        self.assertEqual(stat.S_IMODE(public_path.stat().st_mode), 0o444)
        receipt = self.fixture.output / "RESTORE-RECEIPT.json"
        self.assertEqual(stat.S_IMODE(receipt.stat().st_mode), 0o600)
        receipt_value = json.loads(receipt.read_text())
        for phase in ("pretag_initial_provenance", "pretag_final_provenance"):
            proof = receipt_value[phase]
            self.assertEqual(
                digest(self.fixture.actions_zip),
                proof["artifact"]["raw_actions_zip_sha256"],
            )
            self.assertEqual(
                ["workflow", "run", "artifact", "protected_main"],
                [row["label"] for row in proof["api"]["responses"]],
            )
            self.assertTrue(all(row["age"] == 0 for row in proof["api"]["responses"]))
        combined = result.stdout + result.stderr
        for index in range(1, 7):
            self.assertNotIn(f"{index:064x}", combined)

    def test_restore_rejects_unsafe_paths_links_duplicates_pax_sparse_and_modes(self) -> None:
        cases: list[tuple[str, tarfile.TarInfo, bytes, str]] = []
        traversal = tarfile.TarInfo("../secret.json"); traversal.mode = 0o600
        traversal_payload = self.fixture.key_json(0, NODES[0][1]); traversal.size = len(traversal_payload)
        cases.append(("traversal", traversal, traversal_payload, "path is unsafe"))
        absolute = tarfile.TarInfo("/secret.json"); absolute.mode = 0o600; absolute.size = len(traversal_payload)
        cases.append(("absolute", absolute, traversal_payload, "path is unsafe"))
        backslash = tarfile.TarInfo("private\\secret.json"); backslash.mode = 0o600; backslash.size = len(traversal_payload)
        cases.append(("backslash", backslash, traversal_payload, "path is unsafe"))
        link = tarfile.TarInfo("private/link.json"); link.type = tarfile.SYMTYPE; link.linkname = "identity-1.json"
        cases.append(("symlink", link, b"", "nonsparse regular file"))
        hardlink = tarfile.TarInfo("private/hard.json"); hardlink.type = tarfile.LNKTYPE; hardlink.linkname = "private/identity-1.json"
        cases.append(("hardlink", hardlink, b"", "nonsparse regular file"))
        sparse = tarfile.TarInfo("private/sparse.json"); sparse.type = tarfile.GNUTYPE_SPARSE; sparse.mode = 0o600; sparse.size = 0
        cases.append(("sparse", sparse, b"", "nonsparse regular file"))
        permissive = tarfile.TarInfo("private/permissive.json"); permissive.mode = 0o644; permissive.size = len(traversal_payload)
        cases.append(("permissive", permissive, traversal_payload, "private permissions"))
        pax = tarfile.TarInfo("private/pax.json"); pax.mode = 0o600; pax.size = len(traversal_payload); pax.pax_headers = {"comment": "forbidden"}
        cases.append(("pax", pax, traversal_payload, "PAX metadata"))
        for label, bad, payload, expected in cases:
            with self.subTest(label=label):
                fixture = Fixture(self.root / label)
                fixture.root.mkdir(exist_ok=True) if not fixture.root.exists() else None
                # Fixture initialization needs its directory up front; rebuild
                # the case beneath a dedicated temporary root instead.
                members = copy.deepcopy(fixture.valid_members())
                members[-1] = (bad, payload)
                fixture.write_cms(members)
                fixture.write_rewrap_receipt()
                result = run(fixture.restore_command(), success=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)

    def test_restore_rejects_casefold_duplicates_oversize_wrong_count_and_json_shape(self) -> None:
        members = copy.deepcopy(self.fixture.valid_members())
        duplicate = copy.deepcopy(members[1][0]); duplicate.name = members[1][0].name.upper()
        members.append((duplicate, members[1][1]))
        self.rewrite_cms(members)
        self.assert_restore_rejected("duplicates another path")

        self.fixture = Fixture(self.root / "oversize")
        self.fixture.root.mkdir(exist_ok=True) if not self.fixture.root.exists() else None
        members = copy.deepcopy(self.fixture.valid_members())
        payload = b"x" * (16 * 1024 + 1)
        members[1][0].size = len(payload); members[1] = (members[1][0], payload)
        self.rewrite_cms(members)
        self.assert_restore_rejected("size is outside")

        self.fixture = Fixture(self.root / "five")
        self.fixture.root.mkdir(exist_ok=True) if not self.fixture.root.exists() else None
        self.rewrite_cms(self.fixture.valid_members()[:-1])
        self.assert_restore_rejected("exactly six")

        self.fixture = Fixture(self.root / "json")
        self.fixture.root.mkdir(exist_ok=True) if not self.fixture.root.exists() else None
        members = copy.deepcopy(self.fixture.valid_members())
        value = json.loads(members[1][1]); value["seed"] = "DO_NOT_PRINT_PRIVATE_FIXTURE"
        payload = canonical(value); members[1][0].size = len(payload); members[1] = (members[1][0], payload)
        self.rewrite_cms(members)
        result = run(self.fixture.restore_command(), success=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("DO_NOT_PRINT_PRIVATE_FIXTURE", result.stdout + result.stderr)

    def test_restore_rejects_non_gcm_profile_wrong_certificate_key_and_all_hash_bindings(self) -> None:
        self.fixture.write_cms(self.fixture.valid_members(), cipher="-aes-256-cbc")
        self.fixture.write_rewrap_receipt()
        self.assert_restore_rejected("AES-256-GCM")

        self.fixture = Fixture(self.root / "wrong-key")
        self.fixture.root.mkdir(exist_ok=True) if not self.fixture.root.exists() else None
        other = self.root / "other-key.pem"
        shutil.copyfile(Fixture.private_key, other); other.chmod(0o600)
        # A copied key still matches; generate an intentionally different one.
        run([str(Fixture.openssl), "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:3072", "-out", str(other)])
        other.chmod(0o600)
        command = self.fixture.restore_command()
        command[command.index("--restore-private-key") + 1] = str(other)
        result = run(command, success=False)
        self.assertIn("does not match", result.stderr)

        for option, expected in (
            ("--expected-cms-sha256", "CMS ciphertext bytes differ"),
            ("--source-main-sha", "rewrap receipt is not bound"),
        ):
            with self.subTest(option=option):
                command = self.fixture.restore_command()
                command[command.index(option) + 1] = ("c" * 40 if option == "--source-main-sha" else "c" * 64)
                result = run(command, success=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)

        self.fixture.cli.write_bytes(self.fixture.cli.read_bytes() + b"# changed\n")
        self.fixture.cli.chmod(0o700)
        self.assert_restore_rejected("ARC CLI bytes differ")
        self.fixture.write_cli()
        self.fixture.genesis.write_bytes(self.fixture.genesis.read_bytes() + b"# changed\n")
        self.assert_restore_rejected("genesis bytes differ")

    def test_restore_is_create_only(self) -> None:
        self.fixture.output.mkdir(mode=0o700)
        marker = self.fixture.output / "owner"
        marker.write_text("preserve")
        result = run(self.fixture.restore_command(), success=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("replacement and merge are forbidden", result.stderr)
        self.assertEqual(marker.read_text(), "preserve")

    def test_create_only_publication_completes_partial_writes_and_removes_zero_progress_file(self) -> None:
        helper = load_helper_module()
        payload = b"canonical receipt bytes must never truncate\n" * 40
        destination = self.root / "partial-write.json"
        real_write = helper.os.write

        def partial_write(descriptor: int, value: bytes) -> int:
            return real_write(descriptor, value[: max(1, min(7, len(value)))])

        with mock.patch.object(helper.os, "write", side_effect=partial_write):
            helper.create_file(destination, payload, 0o444)
        self.assertEqual(destination.read_bytes(), payload)
        self.assertEqual(stat.S_IMODE(destination.stat().st_mode), 0o444)

        stalled = self.root / "zero-progress.json"
        with mock.patch.object(helper.os, "write", return_value=0):
            with self.assertRaises(helper.VaultError):
                helper.create_file(stalled, payload, 0o444)
        self.assertFalse(stalled.exists())

    def test_cleanup_unlinks_symlink_without_touching_external_target(self) -> None:
        helper = load_helper_module()
        cleanup_root = self.root / "private-cleanup"
        cleanup_root.mkdir(mode=0o700)
        nested = cleanup_root / "nested"
        nested.mkdir(mode=0o700)
        (nested / "private.json").write_bytes(b"private fixture\n")
        external = self.root / "external-evidence.json"
        external.write_bytes(b"must remain exact\n")
        external.chmod(0o400)
        (cleanup_root / "escape").symlink_to(external)
        hardlink_target = self.root / "hardlink-target.json"
        hardlink_target.write_bytes(b"hardlink must remain exact\n")
        hardlink_target.chmod(0o600)
        os.link(hardlink_target, cleanup_root / "hardlink.json")

        helper.secure_cleanup(cleanup_root)

        self.assertFalse(cleanup_root.exists())
        self.assertEqual(b"must remain exact\n", external.read_bytes())
        self.assertEqual(0o400, stat.S_IMODE(external.stat().st_mode))
        self.assertEqual(b"hardlink must remain exact\n", hardlink_target.read_bytes())
        self.assertEqual(1, hardlink_target.stat().st_nlink)

    def test_external_cli_mutation_of_pinned_key_is_detected_before_output(self) -> None:
        script = textwrap.dedent(
            """\
            #!/usr/bin/env python3
            import json, sys
            path=sys.argv[3]
            with open(path,encoding="utf-8") as handle: value=json.load(handle)
            with open(path,"ab") as handle: handle.write(b" ")
            print(value["address"])
            """
        )
        create(self.fixture.cli, script.encode(), 0o700)
        self.fixture.write_metadata()
        result = run(self.fixture.restore_command(), success=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("changed during exact CLI verification", result.stderr)
        self.assertFalse(self.fixture.output.exists())

    def test_restore_rejects_unreviewed_openssl_hash_and_symlink_path_before_decrypt(self) -> None:
        wrong_hash = self.fixture.restore_command()
        wrong_hash[wrong_hash.index("--openssl-sha256") + 1] = "d" * 64
        result = run(wrong_hash, success=False)
        self.assertIn("differs from its reviewed SHA-256", result.stderr)
        self.assertFalse(self.fixture.output.exists())

        openssl_link = self.root / "operator-selected-openssl"
        openssl_link.symlink_to(Fixture.openssl)
        linked = self.fixture.restore_command()
        linked[linked.index("--openssl") + 1] = str(openssl_link)
        result = run(linked, success=False)
        self.assertIn("single-link, non-writable regular file", result.stderr)
        self.assertFalse(self.fixture.output.exists())

    def test_install_rejects_transport_hash_and_wrong_host_before_any_remote_operation(self) -> None:
        self.fixture.restore()
        command, remote_root = self.fixture.install_command(self.root / "INSTALL-RECEIPT.json")
        command[command.index("--ssh-sha256") + 1] = "d" * 64
        result = run(command, success=False)
        self.assertIn("SSH executable changed or differs", result.stderr)
        self.assertEqual([], list(remote_root.iterdir()))

        command[command.index("--ssh-sha256") + 1] = digest(
            Path(command[command.index("--ssh") + 1])
        )
        command[command.index("--scp-sha256") + 1] = "e" * 64
        result = run(command, success=False)
        self.assertIn("SCP executable changed or differs", result.stderr)
        self.assertEqual([], list(remote_root.iterdir()))

        evidence_path = Path(command[command.index("--offline-stop-evidence") + 1])
        evidence = json.loads(evidence_path.read_text())
        evidence["nodes"][0]["host"] = "203.0.113.10"
        evidence_path.chmod(0o600)
        create(evidence_path, canonical(evidence), 0o400)
        evidence_sha = digest(evidence_path)
        evidence_sidecar = Path(
            command[command.index("--offline-stop-evidence-sidecar") + 1]
        )
        evidence_sidecar.chmod(0o600)
        create(
            evidence_sidecar,
            f"{evidence_sha}  {evidence_path.name}\n".encode(),
            0o400,
        )
        command[command.index("--offline-stop-evidence-sha256") + 1] = evidence_sha
        command[command.index("--scp-sha256") + 1] = digest(
            Path(command[command.index("--scp") + 1])
        )
        result = run(command, success=False)
        self.assertIn("fixed production host/key topology", result.stderr)
        self.assertEqual([], list(remote_root.iterdir()))

    def test_install_rejects_wildcard_reordered_duplicate_or_unprotected_ssh_anchors(self) -> None:
        self.fixture.restore()
        command, remote_root = self.fixture.install_command(self.root / "INSTALL-RECEIPT.json")
        known_hosts = Path(command[command.index("--known-hosts") + 1])
        identity = Path(command[command.index("--ssh-identity") + 1])
        original_hosts = known_hosts.read_bytes()
        original_identity = identity.read_bytes()

        def replace(path: Path, payload: bytes, mode: int = 0o400) -> None:
            path.chmod(0o600)
            path.write_bytes(payload)
            path.chmod(mode)

        host_lines = original_hosts.decode("ascii").splitlines()
        malformed = []
        wildcard = list(host_lines)
        wildcard[0] = "* " + wildcard[0].split(" ", 1)[1]
        malformed.append(("wildcard", wildcard, "fixed production IP"))
        reordered = list(host_lines)
        reordered[0], reordered[1] = reordered[1], reordered[0]
        malformed.append(("reordered", reordered, "fixed production IP"))
        duplicate_key = list(host_lines)
        duplicate_key[-1] = duplicate_key[-1].split(" ", 2)[0] + " " + host_lines[0].split(" ", 1)[1]
        malformed.append(("duplicate-key", duplicate_key, "reuses an Ed25519 host key"))
        for label, lines, expected in malformed:
            with self.subTest(label=label):
                replace(known_hosts, ("\n".join(lines) + "\n").encode("ascii"))
                command[command.index("--known-hosts-sha256") + 1] = digest(known_hosts)
                result = run(command, success=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)
                self.assertEqual([], list(remote_root.iterdir()))

        replace(known_hosts, original_hosts, mode=0o600)
        command[command.index("--known-hosts-sha256") + 1] = digest(known_hosts)
        wrong_mode = run(command, success=False)
        self.assertIn("pinned known-hosts file must be mode 0400", wrong_mode.stderr)
        replace(known_hosts, original_hosts)
        command[command.index("--known-hosts-sha256") + 1] = digest(known_hosts)
        host_link = self.root / "known-hosts-second-link"
        os.link(known_hosts, host_link)
        linked = run(command, success=False)
        self.assertIn("pinned known-hosts file must have exactly one hard link", linked.stderr)
        host_link.unlink()

        replace(identity, original_identity, mode=0o600)
        command[command.index("--ssh-identity-sha256") + 1] = digest(identity)
        identity_mode = run(command, success=False)
        self.assertIn("explicit SSH identity must be mode 0400", identity_mode.stderr)
        replace(identity, original_identity)
        command[command.index("--ssh-identity-sha256") + 1] = digest(identity)
        identity_link = self.root / "identity-second-link"
        os.link(identity, identity_link)
        identity_linked = run(command, success=False)
        self.assertIn("explicit SSH identity must have exactly one hard link", identity_linked.stderr)
        identity_link.unlink()
        self.assertEqual([], list(remote_root.iterdir()))

    def test_transport_options_disable_config_agent_and_default_identities(self) -> None:
        helper = load_helper_module()
        known_hosts = Path("/private/known_hosts")
        identity = Path("/private/id_ed25519")
        options = helper.strict_transport_options(known_hosts, identity)
        self.assertEqual(["-F", "/dev/null", "-i", str(identity)], options[:4])
        self.assertEqual(1, options.count("-F"))
        self.assertEqual(1, options.count("-i"))
        joined = "\n".join(options)
        self.assertIn("IdentityAgent=none", joined)
        self.assertIn("IdentitiesOnly=yes", joined)
        self.assertIn("GlobalKnownHostsFile=/dev/null", joined)
        self.assertIn("HostKeyAlgorithms=ssh-ed25519", joined)
        self.assertIn("PubkeyAcceptedAlgorithms=ssh-ed25519", joined)
        self.assertIn("UpdateHostKeys=no", joined)
        self.assertNotIn("~/.ssh", joined)

    def test_install_rejects_cached_or_cross_bound_provenance_receipt(self) -> None:
        self.fixture.restore()
        receipt_path = self.fixture.output / "RESTORE-RECEIPT.json"
        original = receipt_path.read_bytes()
        command, remote_root = self.fixture.install_command(self.root / "INSTALL-RECEIPT.json")

        value = json.loads(original)
        value["pretag_final_provenance"]["api"]["responses"][-1]["age"] = 1
        receipt_path.write_bytes(canonical(value))
        receipt_path.chmod(0o600)
        cached = run(command, success=False)
        self.assertIn("cached or untimestamped", cached.stderr)
        self.assertEqual([], list(remote_root.iterdir()))

        value = json.loads(original)
        initial_time = max(
            row["response_unix"]
            for row in value["pretag_initial_provenance"]["api"]["responses"]
        )
        for row in value["pretag_final_provenance"]["api"]["responses"]:
            row["response_unix"] = initial_time - 1
        value["pretag_final_provenance"]["live"]["api_verified_at_unix"] = (
            initial_time - 1
        )
        receipt_path.write_bytes(canonical(value))
        receipt_path.chmod(0o600)
        reordered = run(command, success=False)
        self.assertIn("predates the initial proof", reordered.stderr)
        self.assertEqual([], list(remote_root.iterdir()))

        value = json.loads(original)
        for final, initial in zip(
            value["pretag_final_provenance"]["api"]["responses"],
            value["pretag_initial_provenance"]["api"]["responses"],
        ):
            final["request_id"] = initial["request_id"]
        receipt_path.write_bytes(canonical(value))
        receipt_path.chmod(0o600)
        replayed = run(command, success=False)
        self.assertIn("fresh API requests", replayed.stderr)
        self.assertEqual([], list(remote_root.iterdir()))

        value = json.loads(original)
        value["pretag_final_provenance"]["artifact"]["archive_sha256"] = "d" * 64
        receipt_path.write_bytes(canonical(value))
        receipt_path.chmod(0o600)
        cross_bound = run(command, success=False)
        self.assertIn("raw ZIP/name tuple differs", cross_bound.stderr)
        self.assertEqual([], list(remote_root.iterdir()))

    def test_install_is_partial_resume_safe_create_only_and_receipt_contains_no_private_bytes(self) -> None:
        self.fixture.restore()
        install_receipt = self.root / "INSTALL-RECEIPT.json"
        command, remote_root = self.fixture.install_command(install_receipt)
        hosts = list(HOSTS.values())
        (remote_root / "FAIL-HOST").write_text(hosts[4])
        first = run(command, success=False)
        self.assertNotEqual(first.returncode, 0)
        self.assertFalse(install_receipt.exists())
        for host in hosts[:4]:
            self.assertTrue(
                (remote_root / host / "validator-key.json").is_file(),
                first.stdout + first.stderr,
            )
        for host in hosts[4:]:
            self.assertFalse((remote_root / host / "validator-key.json").exists())
        (remote_root / "FAIL-HOST").unlink()
        second = run(command)
        self.assertIn("six create-only remote identities verified", second.stdout)
        self.assertEqual(stat.S_IMODE(install_receipt.stat().st_mode), 0o444)
        receipt = json.loads(install_receipt.read_text())
        self.assertEqual([row["state"] for row in receipt["validators"]], ["verified"] * 6)
        self.assertEqual(
            receipt["legacy_maintenance_evidence_bundle_sha256"],
            command[command.index("--legacy-maintenance-evidence-bundle-sha256") + 1],
        )
        self.assertEqual(
            receipt["legacy_maintenance_boundary_sha256"],
            command[command.index("--legacy-maintenance-boundary-sha256") + 1],
        )
        self.assertEqual(
            digest(self.fixture.actions_zip),
            receipt["pretag_initial_provenance"]["artifact"]["raw_actions_zip_sha256"],
        )
        self.assertEqual(
            receipt["pretag_initial_provenance"]["artifact"],
            receipt["pretag_final_provenance"]["artifact"],
        )
        combined = install_receipt.read_text() + second.stdout + second.stderr
        for index in range(1, 7):
            self.assertNotIn(f"{index:064x}", combined)
        # A complete exact-match resume accepts all six without SCP mutation.
        receipt_before_resume = install_receipt.read_bytes()
        third = run(command)
        self.assertEqual(third.returncode, 0)
        self.assertEqual(receipt_before_resume, install_receipt.read_bytes())
        uploaded_sources = (remote_root / "SOURCES").read_text().splitlines()
        self.assertEqual(len(uploaded_sources), 6)
        self.assertTrue(all("arc-validator-vault-install." in path for path in uploaded_sources))
        self.assertTrue(all("/restored/keys/" not in path for path in uploaded_sources))

        # Exact bytes with a mutable mode are not a valid receipt resume.
        install_receipt.chmod(0o600)
        wrong_mode = run(command, success=False)
        self.assertNotEqual(wrong_mode.returncode, 0)
        self.assertIn("must be mode 0444", wrong_mode.stderr)

    def test_install_rejects_unsealed_replayed_or_cross_bound_maintenance_roots(self) -> None:
        self.fixture.restore()
        command, remote_root = self.fixture.install_command(self.root / "INSTALL-RECEIPT.json")
        bundle = Path(command[command.index("--legacy-maintenance-evidence-bundle") + 1])
        bundle_sidecar = Path(
            command[command.index("--legacy-maintenance-evidence-bundle-sidecar") + 1]
        )
        boundary = Path(command[command.index("--legacy-maintenance-boundary") + 1])
        boundary_sidecar = Path(
            command[command.index("--legacy-maintenance-boundary-sidecar") + 1]
        )
        evidence = Path(command[command.index("--offline-stop-evidence") + 1])
        evidence_sidecar = Path(command[command.index("--offline-stop-evidence-sidecar") + 1])

        bundle.chmod(0o600)
        mutable = run(command, success=False)
        self.assertIn("legacy maintenance evidence bundle must be mode 0400", mutable.stderr)
        bundle.chmod(0o400)

        second_link = self.root / "bundle-second-link.json"
        os.link(bundle, second_link)
        linked = run(command, success=False)
        self.assertIn("legacy maintenance evidence bundle must have exactly one hard link", linked.stderr)
        second_link.unlink()

        original_bundle_sidecar = bundle_sidecar.read_bytes()
        bundle_sidecar.chmod(0o600)
        bundle_sidecar.write_bytes(boundary_sidecar.read_bytes())
        bundle_sidecar.chmod(0o400)
        swapped = run(command, success=False)
        self.assertIn("legacy maintenance evidence bundle checksum sidecar differs", swapped.stderr)
        bundle_sidecar.chmod(0o600)
        bundle_sidecar.write_bytes(original_bundle_sidecar)
        bundle_sidecar.chmod(0o400)

        boundary_sidecar.chmod(0o600)
        wrong_mode = run(command, success=False)
        self.assertIn("legacy maintenance boundary checksum sidecar must be mode 0400", wrong_mode.stderr)
        boundary_sidecar.chmod(0o400)

        original_evidence = evidence.read_bytes()
        original_evidence_sidecar = evidence_sidecar.read_bytes()
        original_evidence_sha = command[command.index("--offline-stop-evidence-sha256") + 1]

        def rewrite_evidence(value: dict) -> None:
            evidence.chmod(0o600)
            evidence.write_bytes(canonical(value))
            evidence.chmod(0o400)
            rewritten_sha = digest(evidence)
            evidence_sidecar.chmod(0o600)
            evidence_sidecar.write_bytes(
                f"{rewritten_sha}  {evidence.name}\n".encode("ascii")
            )
            evidence_sidecar.chmod(0o400)
            command[command.index("--offline-stop-evidence-sha256") + 1] = rewritten_sha

        forged = json.loads(original_evidence)
        forged["schema"] = "arc.validator-vault.offline-stop-evidence.v1"
        rewrite_evidence(forged)
        legacy = run(command, success=False)
        self.assertIn("source/freeze/helper binding differs", legacy.stderr)

        forged = json.loads(original_evidence)
        forged["legacy_maintenance_boundary"]["created_at"] = "2026-08-28T12:02:32Z"
        rewrite_evidence(forged)
        cross_bound = run(command, success=False)
        self.assertIn("maintenance bundle/boundary binding differs", cross_bound.stderr)

        evidence.chmod(0o600)
        evidence.write_bytes(original_evidence)
        evidence.chmod(0o400)
        evidence_sidecar.chmod(0o600)
        evidence_sidecar.write_bytes(original_evidence_sidecar)
        evidence_sidecar.chmod(0o400)
        command[command.index("--offline-stop-evidence-sha256") + 1] = original_evidence_sha
        self.assertEqual([], list(remote_root.iterdir()))

    def test_install_refuses_remote_mismatch_and_unsealed_or_incomplete_freeze(self) -> None:
        self.fixture.restore()
        command, remote_root = self.fixture.install_command(self.root / "INSTALL-RECEIPT.json")
        proof_path = Path(command[command.index("--offline-stop-evidence") + 1])
        proof_path.chmod(0o600)
        mutable_proof = run(command, success=False)
        self.assertIn("must be mode 0400", mutable_proof.stderr)
        proof_path.chmod(0o400)

        first_host = HOSTS["NYC"]
        final = remote_root / first_host / "validator-key.json"
        final.parent.mkdir(parents=True)
        create(final, b"wrong key bytes\n", 0o600)
        result = run(command, success=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(final.read_bytes(), b"wrong key bytes\n")

        final.unlink()
        # A caller can rewrite JSON, its sidecar, and the command-line digest,
        # but cannot make that forged root match the fresh remote helper result.
        proof = json.loads(proof_path.read_text())
        forged_complete = "c" * 64
        proof["nodes"][0]["stop_complete_sha256"] = forged_complete
        status_path = self.root / "fresh-stopped-status" / f"{first_host}.json"
        forged_status = json.loads(status_path.read_text())
        forged_status["stop_complete_sha256"] = forged_complete
        proof["nodes"][0]["stopped_status_sha256"] = hashlib.sha256(
            canonical(forged_status)
        ).hexdigest()
        proof_path.chmod(0o600); create(proof_path, canonical(proof), 0o400)
        proof_sha = digest(proof_path)
        sidecar = Path(command[command.index("--offline-stop-evidence-sidecar") + 1])
        sidecar.chmod(0o600); create(sidecar, f"{proof_sha}  {proof_path.name}\n".encode(), 0o400)
        command[command.index("--offline-stop-evidence-sha256") + 1] = proof_sha
        result = run(command, success=False)
        self.assertIn("status differs from the evidence bundle", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
