#!/usr/bin/env python3
"""Fail-closed validator for genesis.toml files shipped in ARC releases."""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path
from typing import NoReturn


HEX_ADDRESS = re.compile(r"^[0-9a-fA-F]{64}$")
PRIVATE_MARKER = re.compile(r"-----BEGIN [^-\n]*PRIVATE KEY-----", re.IGNORECASE)
SENSITIVE_KEY_PARTS = {"mnemonic", "private", "secret", "seed"}


def fail(message: str) -> NoReturn:
    raise SystemExit(f"genesis contract: {message}")


def reject_sensitive_material(value: object, path: str = "genesis") -> None:
    if isinstance(value, dict):
        for raw_key, child in value.items():
            key = str(raw_key)
            parts = set(filter(None, re.split(r"[^a-z0-9]+", key.lower())))
            if parts & SENSITIVE_KEY_PARTS:
                fail(f"forbidden secret-bearing field at {path}.{key}")
            reject_sensitive_material(child, f"{path}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value, start=1):
            reject_sensitive_material(child, f"{path}[{index}]")
    elif isinstance(value, str) and PRIVATE_MARKER.search(value):
        fail(f"private-key material is forbidden at {path}")


def require_exact_keys(
    value: object,
    expected: set[str],
    path: str,
    *,
    required: set[str] | None = None,
) -> dict[str, object]:
    if not isinstance(value, dict):
        fail(f"{path} must be a TOML table")
    actual = set(value)
    missing = (expected if required is None else required) - actual
    unknown = actual - expected
    if missing:
        fail(f"{path} is missing required field(s): {', '.join(sorted(missing))}")
    if unknown:
        fail(f"{path} contains unsupported field(s): {', '.join(sorted(unknown))}")
    return value


def require_nonnegative_integer(value: object, path: str, *, positive: bool) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        fail(f"{path} must be an integer")
    if positive and value <= 0:
        fail(f"{path} must be positive")
    if not positive and value < 0:
        fail(f"{path} must be non-negative")
    return value


def require_address(value: object, path: str) -> str:
    if not isinstance(value, str) or not HEX_ADDRESS.fullmatch(value):
        fail(f"{path} must be a 64-character hexadecimal public ARC address")
    return value.lower()


def validate(path: Path) -> tuple[bool, int, int | None]:
    try:
        raw = path.read_bytes()
    except OSError as error:
        fail(f"cannot read {path}: {error}")

    try:
        document = tomllib.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        fail(f"{path} is not valid UTF-8 TOML: {error}")

    reject_sensitive_material(document)
    root = require_exact_keys(
        document,
        {"chain", "accounts", "validators"},
        "genesis",
        required={"chain"},
    )
    chain = require_exact_keys(
        root["chain"],
        {
            "name",
            "chain_id",
            "validator_set_complete",
            "community_rewards_v1_activation_height",
        },
        "genesis.chain",
        required={"name", "chain_id", "validator_set_complete"},
    )
    if not isinstance(chain["name"], str) or not chain["name"].strip():
        fail("genesis.chain.name must be a non-empty string")
    if not isinstance(chain["chain_id"], str) or not chain["chain_id"].strip():
        fail("genesis.chain.chain_id must be a non-empty string")
    complete = chain["validator_set_complete"]
    if not isinstance(complete, bool):
        fail("genesis.chain.validator_set_complete must be explicitly true or false")
    activation_height = chain.get("community_rewards_v1_activation_height")
    if activation_height is not None:
        activation_height = require_nonnegative_integer(
            activation_height,
            "genesis.chain.community_rewards_v1_activation_height",
            positive=False,
        )
        # StateDB reserves u64::MAX as the in-memory sentinel for "absent".
        # Shipping that value would make the semantic genesis hash say Some
        # while runtime state treats it as None, so the release contract must
        # reject it (and values Rust's u64 TOML field cannot represent).
        if activation_height >= (1 << 64) - 1:
            fail(
                "genesis.chain.community_rewards_v1_activation_height must be "
                "less than 18446744073709551615"
            )

    accounts = root.get("accounts", [])
    if not isinstance(accounts, list):
        fail("genesis.accounts must be an array of tables")
    seen_accounts: set[str] = set()
    for index, raw_account in enumerate(accounts, start=1):
        account = require_exact_keys(
            raw_account, {"address", "balance"}, f"genesis.accounts[{index}]"
        )
        address = require_address(account["address"], f"genesis.accounts[{index}].address")
        require_nonnegative_integer(
            account["balance"], f"genesis.accounts[{index}].balance", positive=False
        )
        if address in seen_accounts:
            fail(f"duplicate account address at genesis.accounts[{index}].address")
        seen_accounts.add(address)

    validators = root.get("validators", [])
    if not isinstance(validators, list):
        fail("genesis.validators must be an array of tables")
    if not complete:
        if validators:
            fail(
                "an incomplete community-observer genesis must not contain a partial "
                "validator list"
            )
        if activation_height is not None:
            fail(
                "an incomplete community-observer genesis must not schedule "
                "community reward activation"
            )
        return False, 0, None
    if not validators:
        fail("validator_set_complete is true but no public validators are declared")

    seen_validators: set[str] = set()
    for index, raw_validator in enumerate(validators, start=1):
        validator = require_exact_keys(
            raw_validator, {"address", "stake"}, f"genesis.validators[{index}]"
        )
        address = require_address(
            validator["address"], f"genesis.validators[{index}].address"
        )
        require_nonnegative_integer(
            validator["stake"], f"genesis.validators[{index}].stake", positive=True
        )
        if address in seen_validators:
            fail(f"duplicate validator address at genesis.validators[{index}].address")
        if address not in seen_accounts:
            fail(
                f"genesis.validators[{index}].address must also be declared in "
                "genesis.accounts so every node materializes identical state"
            )
        seen_validators.add(address)

    return True, len(validators), activation_height


def main() -> None:
    if len(sys.argv) != 2:
        fail("usage: validate-genesis.py PATH")
    path = Path(sys.argv[1])
    complete, validator_count, activation_height = validate(path)
    if complete:
        message = (
            "genesis contract: complete production validator set contains "
            f"{validator_count} public address(es) and no secret material"
        )
        if activation_height is None:
            message += "; community rewards v1 disabled (activation absent)"
        else:
            message += (
                "; community rewards v1 activation height "
                f"{activation_height} is explicit"
            )
        print(message)
    else:
        print(
            "genesis contract: explicit incomplete stake-zero community-observer "
            "placeholder contains no validators or secret material; community "
            "rewards v1 disabled (activation absent)"
        )


if __name__ == "__main__":
    main()
