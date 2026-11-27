#!/usr/bin/env python3
import argparse
import json
import os
from pathlib import Path
import subprocess


EXPECTED_BINARY_TESTS = {
    'community_shard_cli::tests::integrated_shard_worker_shared_prepare_cancels_when_shutdown_flag_set',
    'community_shard_cli::tests::integrated_shard_worker_shared_prepare_rejects_unpinned_model_before_metadata_parse',
    'community_shard_cli::tests::integrated_shard_worker_runtime_task_drains_before_join_returns',
    'community_shard_cli::tests::integrated_shard_worker_skips_start_when_shutdown_already_requested',
    'integrated_shard_worker_tests::integrated_shard_worker_parses_explicit_pins_and_defaults_to_one_thread',
    'integrated_shard_worker_tests::integrated_shard_worker_respects_explicit_threads_and_rejects_zero',
    'integrated_shard_worker_tests::integrated_shard_worker_requires_all_immutable_pins_and_budget',
    'integrated_shard_worker_tests::integrated_shard_worker_rejects_pin_flags_without_mode_flag',
    'integrated_shard_worker_tests::integrated_shard_worker_rejects_incompatible_roles_and_capability_mixing',
    'integrated_shard_worker_tests::integrated_shard_worker_requires_persistent_identity_and_forbids_ephemeral_or_seed',
    'integrated_shard_worker_tests::integrated_shard_worker_enforces_https_or_explicit_literal_loopback',
    'integrated_shard_worker_tests::integrated_shard_worker_transaction_domain_guard_handles_none_match_and_mismatch',

    'recovery_peer_qualification_tests::inspect_peer_capture_reads_real_persistent_generation_and_target_signing_record',
    'community_shard_cli::tests::pending_shutdown_signal_preempts_worker_first_poll_and_prepare_completion',
    'community_shard_cli::tests::prepare_rejects_non_utf8_model_path_before_hashing_or_check_only_pass',
    'community_shard_cli::tests::shutdown_signals_watch_and_drains_worker_and_prepare_without_abort',
    'community_shard_cli::tests::partial_loader_admits_before_weights_and_verifies_artifact_before_and_after_load',
    'community_shard_cli::tests::derive_all_roles_admission_enforces_budget_caps_and_first_embedding_floor',
    'community_shard_cli::tests::prepare_rejects_wrong_artifact_pin_before_metadata_keys_or_network',
    'community_shard_cli::tests::validate_args_rejects_zero_and_hard_cap_budgets_threads_and_insecure_urls',
    'community_shard_cli::tests::cli_rejects_bad_identity_and_domain_hex',
    'community_shard_cli::tests::cli_requires_pins_and_allows_missing_keyfile_only_for_check_only',
    'community_shard_cli::tests::cli_parses_top_level_subcommand_and_rejects_operator_prefix',
    "tests::recovery_peer_qualify_command_accepts_persistent_candidate_and_rejects_mutations",
    "tests::recovery_peer_generation_replays_for_different_absent_validator_compacts_and_restarts",
    "tests::recovery_peer_replay_rejects_unsigned_forged_and_wrong_domain_blocks",
    "tests::recovery_peer_replay_rejects_subquorum_forged_and_wrong_domain_skip_certificates",
    'recovery_peer_qualification_tests::qualify_peer_state_rejects_bad_manifest_schema_unknown_fields_limits_and_hash',
    'recovery_peer_qualification_tests::qualify_peer_state_rejects_bad_paths_nesting_symlinks_and_hardlinks',
    'recovery_peer_qualification_tests::qualify_peer_state_rejects_keyless_unsigned_forged_and_wrong_domain_dag_blocks',
    'recovery_peer_qualification_tests::qualify_peer_state_rejects_oversized_wrong_domain_forged_and_subquorum_certificates',
}
EXPECTED_LIBRARY_TESTS = {
    'rpc::community_shard_dispatch::tests::compute_only_readiness_binds_identity_and_is_read_only_over_http',
    'rpc::community_shard_dispatch::inference_run_adapter_tests::community_sharded_request_serde_defaults_chat_template_false_and_rejects_unknown_fields',
    'rpc::community_shard_dispatch::inference_run_adapter_tests::chat_template_formatting_matches_inference_run_and_preserves_1000_byte_dispatched_cap',
    'rpc::community_shard_dispatch::tests::inference_run_routes_eligible_request_to_outbound_partial_workers',
    'rpc::community_shard_dispatch::tests::inference_run_partial_failure_and_bypass_guards_never_fallback_or_duplicate',
    'rpc::community_shard_dispatch::tests::readiness_snapshot_reflects_ready_community_shards_mutation_free',
    'rpc::community_sharded_routing_tests::readiness_v1_preserves_exact_nine_keys_when_community_sharded_ready',
    'rpc::community_sharded_routing_tests::community_sharded_route_guards_enforce_eligibility_and_caps',
    'rpc::community_shard_dispatch::inference_run_adapter_tests::dispatch_bounds_helper_matches_validate_request_bounds',
    'rpc::community_shard_dispatch::inference_run_adapter_tests::flat_adapter_copies_model_hash_preserves_verified_fields_and_omits_settlement',
    'community_worker::tests::partial_snapshot_rejects_expired_or_malformed_leases_without_counting_whole_jobs',
    'rpc::community_shard_http::worker_tests::partial_worker_telemetry_tracks_real_loading_ready_ttl_steps_stop_and_fatal_failure',
    'community_sharding::tests::pipeline_capacity_scales_across_6_12_24_and_100_workers_and_tracks_churn',
    'community_sharding::tests::pipeline_capacity_respects_default_24_and_hard_cap_64',
    'community_sharding::tests::heartbeat_ready_transition_heals_coverage_gap_without_deadlock',
    'community_sharding::tests::rebalance_recovers_post_churn_throughput_and_heterogeneous_tile_balance',
    'community_sharding::tests::rejoining_worker_cannot_jump_lru_fairness_queue',
    'community_sharding::tests::rebalance_state_model_preserves_invariants_across_homogeneous_heterogeneous_and_ack_retries',
    'rpc::community_shard_dispatch::tests::four_concurrent_sharded_sessions_scale_past_verifier_cap_two',
    'rpc::community_shard_dispatch::tests::shutdown_cancels_in_flight_and_verification_queued_sharded_requests',
    'rpc::community_shard_dispatch::tests::verification_wait_deadline_exhaustion_fails_bounded_and_releases_slot',
    'community_shard_worker::tests::auto_reconnect_stops_during_backoff_without_extra_http_and_fails_closed_on_auth',
    'community_shard_worker::tests::auto_reconnect_drops_weights_and_retains_boot_fence_across_outage',
    'rpc::community_shard_http::worker_tests::auto_reconnect_recovers_after_503_outage_beyond_budget_and_executes_canonical_step',
    'community_shard_worker::tests::transient_heartbeat_backoff_grows_and_resets_after_success',
    'community_shard_worker::tests::elapsed_idle_wait_returns_before_overdue_heartbeat_but_shutdown_wins',
    'rpc::community_shard_dispatch::tests::outbound_workers_and_public_dispatch_return_independently_verified_answer',
    'rpc::community_shard_dispatch::tests::validate_pending_result_rejects_altered_digest_and_eos_or_token_anomalies',
    'rpc::community_shard_dispatch::tests::verification_candidate_binds_node_transaction_domain',
    'rpc::community_shard_dispatch::tests::lost_notify_does_not_hang',
    'rpc::community_shard_dispatch::tests::dropped_caller_future_synchronously_cancels_session_and_clears_retained_result',
    'rpc::community_shard_dispatch::tests::body_input_capacity_and_disabled_failures',
    'rpc::community_shard_dispatch::tests::missing_verification_replicas_fails_before_opening_session',
    'rpc::community_shard_dispatch::tests::aborted_session_returns_no_answer',
    'rpc::community_shard_dispatch::tests::tampered_generated_tokens_fail_verification_with_no_answer',
    'rpc::community_shard_dispatch::tests::early_eos_two_partial_worker_session_returns_verified_answer',
    'rpc::community_shard_dispatch::tests::valid_two_partial_worker_session_returns_verified_answer',
    'rpc::community_shard_http::worker_tests::hundred_signed_network_workers_all_execute_canonical_partial_inference',
    'rpc::community_shard_http::worker_tests::stalled_http_response_body_hits_total_elapsed_timeout',
    'rpc::community_shard_http::worker_tests::heartbeat_not_starved_by_instant_claim_responses_and_default_loopback_idles_cleanly',
    'community_shard_worker::tests::lease_ttl_validation_enforces_request_and_connect_timeout_headroom',
    'community_shard_worker::tests::config_rejects_extreme_timings_and_reacquires_and_accepts_short_timings',
    'community_shard_worker::tests::config_rejects_insecure_non_literal_loopback_urls_and_bad_layer_budget',
    'community_shard_worker::tests::pre_loader_lease_validation_rejects_full_range_ttl_expiry_and_fenced_boot',
    'community_shard_worker::tests::runtime_rejects_invalid_config_and_non_ed25519_before_loader',
    'rpc::community_shard_http::worker_tests::worker_rejects_redirect_auth_bad_lease_ttl_and_oversized_body_before_loader',
    'rpc::community_shard_http::worker_tests::two_partial_workers_execute_real_networked_pipeline_matching_try_generate',
    'rpc::community_shard_http::worker_tests::transient_submit_response_loss_retries_without_recompute_or_false_success',
    'rpc::community_shard_http::worker_tests::heartbeat_runs_during_blocked_load_and_active_state_and_shutdown_drains',
    'rpc::community_shard_http::worker_tests::rebalance_and_coordinator_restart_drop_old_model_before_reload_and_fence',
    'rpc::community_shard_http::worker_tests::cancellation_during_retry_backoff_prevents_stale_submit',

    'rpc::tests::community_auth_replay_cache_capacity_expiry_and_clock_rollback',
    'rpc::tests::community_auth_replay_cache_many_identities_bounded_counts',
    'rpc::tests::community_auth_ordinary_paths_keep_64_cap_and_accept_31s_skew',
    'rpc::community_shard_http::tests::community_shard_http_replay_cache_allows_over_64_claims_enforces_30s_skew_and_isolates_paths',

    'community_shard_executor::tests::full_pipeline_two_partial_executors_matches_try_generate_and_allows_terminal_retry',
    'community_shard_executor::tests::constructor_rejects_full_model_bad_range_shape_profile_and_config',
    'community_shard_executor::tests::step_retry_preserves_single_kv_advance_and_rejects_conflicts_and_order_violations',
    'community_shard_executor::tests::rejects_invalid_work_auth_binding_inputs_history_terminal_and_deadlines',
    'community_shard_executor::tests::cancellation_expiry_release_and_rebind_prevent_resurrection_and_fence_old_boots',
    'community_shard_executor::tests::enforces_kv_budget_session_cap_tombstone_cap_and_fenced_boot_cap',

    'community_sharding::tests::community_sharding_real_integer_model_pipeline_matches_try_generate',
    'community_sharding::tests::community_sharding_hundred_workers_execute_fair_partial_pipelines',
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


EXPECTED_INFERENCE_TESTS = {
    'golden_vectors::gguf_preparation::load_cached_model_ranges_canonical_i8_parity_clamping_and_roles',
    'canonical_shard_admission::candle_tests::admit_rejects_zero_kv_cache_and_zero_working_set_budgets',
    'canonical_shard_admission::candle_tests::rejects_layer_ceiling_above_1024_and_accepts_1024_boundary_without_weights',
    'canonical_shard_admission::candle_tests::content_parses_captured_header_only_never_reads_past_header_or_mutated_source',
    'canonical_shard_admission::candle_tests::boundary_overflow_undersized_cap_reserved_kv_and_header_limit',
    'canonical_shard_admission::candle_tests::rejects_empty_reversed_out_of_bounds_full_and_over_half_ranges',
    'canonical_shard_admission::candle_tests::first_shard_embedding_budget_dwarfs_interior_and_last_shard',
    'canonical_shard_admission::candle_tests::metadata_array_heap_storage_is_included_in_loading_budget',
    'canonical_shard_admission::candle_tests::rejects_malformed_geometry_ceilings_coercions_and_non_llama',
    'canonical_shard_admission::candle_tests::real_candle_written_non_square_gguf_matches_canonical_loader',
    'canonical_shard_admission::candle_tests::header_only_gguf_without_weights_inspects_and_admits_partial_ranges',
    'golden_vectors::gguf_preparation::a_gguf_prepared_by_the_canonical_loader_matches_the_independent_reference',
    'golden_vectors::gguf_preparation::a_gguf_missing_a_norm_weight_is_refused_by_the_canonical_loader',
    'golden_vectors::gguf_preparation::canonical_i8_partial_shards_match_full_engine_and_ownership',
    'golden_vectors::gguf_preparation::canonical_i8_shard_loader_rejects_invalid_ranges_before_reading_tensor_weights',
}


EXPECTED_STATE_TESTS = {
    'recovery::tests::restart_quarantines_complete_first_block_without_a_checkpoint_to_empty_base',
    'recovery::tests::restart_quarantines_a_torn_first_post_recovery_frame_to_empty_base',
    'recovery::tests::streaming_recovery_reread_rejects_same_size_mutation_and_added_empty_plan_entry',
    'wal::tests::attach_verified_recovery_wal_enforces_name_invariants_identity_and_sequence',
    'recovery::tests::staged_attach_clean_and_quarantined_restart_supports_subsequent_commits_and_restarts',
    'recovery::tests::staged_attach_rejects_wal_mutation_deletion_and_path_replacement_before_binding',
    'recovery::tests::staged_attach_enforces_new_empty_wal_discipline_and_pre_attach_failure_isolation',
    'recovery::tests::prepared_recovery_restart_restores_rooted_native_migration_and_context',
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
        ["cargo", "test", "--locked", "-p", "arc-node", "-p", "arc-inference", "-p", "arc-state", "--features", "candle", "--lib", "--bin", "arc-node",
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
            if target.get("name") in {"arc-node", "arc_node", "arc_inference", "arc_state"}:
                executables.setdefault(target["name"], set()).add(row["executable"])
    expected_targets = {
        "arc_state": (("wal::tests::attach_verified_recovery_wal_", "recovery::tests::staged_attach_", "recovery::tests::prepared_recovery_restart_restores_rooted_native_migration_and_context", "recovery::tests::restart_quarantines_complete_first_block_without_a_checkpoint_to_empty_base", "recovery::tests::restart_quarantines_a_torn_first_post_recovery_frame_to_empty_base", "recovery::tests::streaming_recovery_reread_rejects_same_size_mutation_and_added_empty_plan_entry"), EXPECTED_STATE_TESTS),
        "arc_inference": (("canonical_shard_admission::candle_tests::", "golden_vectors::gguf_preparation::",), EXPECTED_INFERENCE_TESTS),
        "arc-node": (("integrated_shard_worker_tests::", "community_shard_cli::tests::", "tests::recovery_peer_", "recovery_peer_qualification_tests::"), EXPECTED_BINARY_TESTS),
        "arc_node": (("rpc::community_sharded_routing_tests::", "rpc::community_shard_dispatch::inference_run_adapter_tests::", "community_worker::tests::partial_snapshot_", "rpc::community_shard_dispatch::tests::", "community_shard_worker::tests::", "rpc::community_shard_http::worker_tests::", "community_shard_executor::tests::", "community_sharding::tests::", "rpc::community_shard_http::tests::", "rpc::tests::community_auth_"), EXPECTED_LIBRARY_TESTS),
    }
    if set(executables) != set(expected_targets) or any(len(paths) != 1 for paths in executables.values()):
        raise SystemExit(f"Expected one executable for each node binary, node library, inference library, and state library target, found {executables}")
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
    non_candle_test = "canonical_shard_admission::non_candle_tests::non_candle_build_refuses_canonical_shard_metadata_inspection"
    output = run_recorded(
        ["cargo", "test", "--locked", "-p", "arc-inference", "--no-default-features", "--lib",
         non_candle_test, "--", "--exact", "--test-threads=1"],
        args.evidence_dir / "non-candle-admission.log", 1200,
    )
    if output.count("test result: ok. 1 passed; 0 failed; 0 ignored;") != 1:
        failed.append(non_candle_test)
        print(f"FAIL {non_candle_test}\n{output}")
    else:
        passed.append(non_candle_test)
        print(f"PASS {non_candle_test}")
    with (args.evidence_dir / "result.json").open("x") as output:
        json.dump({"passed": passed, "failed": failed}, output, indent=2)
        output.write("\n")
    if failed:
        raise SystemExit(f"{len(failed)} selected qualification tests failed; inspect the saved evidence")


if __name__ == "__main__":
    main()
