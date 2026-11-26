#!/usr/bin/env python3
import argparse
import json
import os
from pathlib import Path
import subprocess


EXPECTED_BINARY_TESTS = {
    "tests::recovery_peer_generation_replays_for_different_absent_validator_compacts_and_restarts",
    "tests::recovery_peer_replay_rejects_unsigned_forged_and_wrong_domain_blocks",
    "tests::recovery_peer_replay_rejects_subquorum_forged_and_wrong_domain_skip_certificates",
    'recovery_peer_qualification_tests::qualify_peer_state_rejects_bad_manifest_schema_unknown_fields_limits_and_hash',
    'recovery_peer_qualification_tests::qualify_peer_state_rejects_bad_paths_nesting_symlinks_and_hardlinks',
    'recovery_peer_qualification_tests::qualify_peer_state_rejects_keyless_unsigned_forged_and_wrong_domain_dag_blocks',
    'recovery_peer_qualification_tests::qualify_peer_state_rejects_oversized_wrong_domain_forged_and_subquorum_certificates',
}
EXPECTED_LIBRARY_TESTS = {
    'community_sharding::tests::community_sharding_real_integer_model_pipeline_matches_try_generate',
    'community_sharding::tests::community_sharding_join_gap_fails_closed_and_caps_partial_range',
    'community_sharding::tests::community_sharding_replicate_join_and_leave_idle_rebalance',
    'community_sharding::tests::community_sharding_mid_session_expiry_and_provisioning_hard_deadline',
    'community_sharding::tests::community_sharding_output_tamper_wrong_hash_and_bounds_rejected',
    'community_sharding::tests::community_sharding_identity_epoch_model_and_serde_mismatch_rejected',
    'community_sharding::tests::community_sharding_no_duplicate_position_or_step_accepted',
    'community_sharding::tests::community_sharding_bounds_exhaustion_and_retention_caps',
    'community_sharding::tests::community_sharding_zero_and_overflow_params_fail',
    'community_sharding::tests::community_sharding_hard_caps_enforced_for_sequence_tokens_and_retention',
    'community_sharding::tests::community_sharding_request_lease_idempotent_retry_and_rebalance_epoch_fence',
    'community_sharding::tests::community_sharding_rebalance_skips_unfillable_gap_and_overflowed_epoch',
    'community_sharding::tests::community_sharding_decline_zero_layers_and_cancel_queue_churn_bounded',
    'rpc::community_shard_http::tests::community_shard_http_init_requires_model_artifact_canonical_profile_and_runs_once',
    'rpc::community_shard_http::tests::community_shard_http_auth_rejects_unregistered_forged_domain_outer_inner_and_replay',
    'rpc::community_shard_http::tests::community_shard_http_rejects_missing_capability_whole_mix_model_mismatch_and_expired',
    'rpc::community_shard_http::tests::community_shard_http_disabled_coordinator_returns_503_and_honest_status',
    'rpc::community_shard_http::tests::community_shard_http_lifecycle_lease_ready_claim_submit_status_and_release',
    'rpc::community_shard_http::tests::community_shard_http_router_body_limits_enforced',
    'rpc::tests::community_auth_valid_signature_registers_claimed_worker',
    'rpc::tests::community_auth_guards_all_four_mutation_handlers',
    'rpc::tests::community_auth_unsigned_legacy_body_fails_closed',
    'rpc::tests::community_auth_rejects_wrong_key_path_payload_and_stale_timestamp',
    'rpc::tests::community_auth_fresh_requests_have_distinct_random_nonces',
    'rpc::tests::community_auth_is_bound_to_exact_coordinator_and_recovery_domain',
    'rpc::tests::community_auth_concurrent_replay_is_atomically_rejected',
}


def run_recorded(command, log, timeout):
    with log.open("x") as output:
        result = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT, timeout=timeout)
    if result.returncode:
        print(log.read_text())
        raise SystemExit(f"Command failed with exit {result.returncode}: {command}")
    return log.read_text()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", required=True, type=Path)
    args = parser.parse_args()
    args.evidence_dir.mkdir(parents=True, exist_ok=False)
    os.environ["CARGO_BUILD_JOBS"] = "1"
    compiled = run_recorded(
        ["cargo", "test", "--locked", "-p", "arc-node", "--lib", "--bin", "arc-node",
         "--no-run", "--message-format=json-render-diagnostics"],
        args.evidence_dir / "compile.log", 3600,
    )
    executables = {}
    for line in compiled.splitlines():
        if not line.startswith("{"):
            continue
        row = json.loads(line)
        if (row.get("reason") == "compiler-artifact" and row.get("profile", {}).get("test")
                and row.get("executable")):
            target = row.get("target", {})
            if target.get("name") in {"arc-node", "arc_node"}:
                executables.setdefault(target["name"], set()).add(row["executable"])
    expected_targets = {
        "arc-node": (("tests::recovery_peer_", "recovery_peer_qualification_tests::"), EXPECTED_BINARY_TESTS),
        "arc_node": (("community_sharding::tests::", "rpc::community_shard_http::tests::", "rpc::tests::community_auth_"), EXPECTED_LIBRARY_TESTS),
    }
    if set(executables) != set(expected_targets) or any(len(paths) != 1 for paths in executables.values()):
        raise SystemExit(f"Expected one node binary and one node library test executable, found {executables}")
    passed, failed = [], []
    for target, (prefixes, expected) in expected_targets.items():
        binary = next(iter(executables[target]))
        listing = run_recorded(
            [binary, "--list"], args.evidence_dir / f"selectors-{target}.log", 30,
        )
        selected = {line.removesuffix(": test") for line in listing.splitlines() if line.endswith(": test") and line.startswith(prefixes)}
        if selected != expected:
            raise SystemExit(f"{target} selectors differ: expected {sorted(expected)}, found {sorted(selected)}")
        for index, test in enumerate(sorted(expected)):
            log = args.evidence_dir / f"test-{target}-{index}.log"
            with log.open("x") as output:
                result = subprocess.run(
                    [binary, test, "--exact", "--nocapture", "--test-threads=1"],
                    stdout=output, stderr=subprocess.STDOUT, timeout=120,
                )
            output = log.read_text()
            if result.returncode or output.count("test result: ok. 1 passed; 0 failed; 0 ignored;") != 1:
                failed.append(test)
                print(f"FAIL {test}\n{output}")
            else:
                passed.append(test)
                print(f"PASS {test}")
    with (args.evidence_dir / "result.json").open("x") as output:
        json.dump({"passed": passed, "failed": failed}, output, indent=2)
        output.write("\n")
    if failed:
        raise SystemExit(f"{len(failed)} selected qualification tests failed; inspect the saved evidence")


if __name__ == "__main__":
    main()
