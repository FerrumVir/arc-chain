mod routing_tests {
    use super::*;
    use community_routing as routing;

    fn node(ids: &[&str]) -> NodeState {
        fake_node_with_workers(
            ids.iter()
                .map(|id| (worker(id, &["inference"]), Instant::now()))
                .collect(),
        )
    }

    fn item(id: &str) -> WorkItem {
        WorkItem {
            job_id: arc_crypto::hash_bytes(id.as_bytes()).to_hex(),
            input: "ARC".into(),
            max_tokens: 1,
            model_id: Some(test_model_id()),
            execution_profile: canonical_profile(),
            transaction_domain: None,
            expected_worker_id: None,
            submitted_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
        }
    }

    fn waiting(node: &NodeState, id: &str) {
        node.community_active_jobs.insert(id.into(), String::new());
        routing::open_claim(
            node,
            id,
            parse_hash256_hex(&test_model_id(), "model").unwrap(),
        );
    }

    fn enqueue(
        node: &NodeState,
        job: &WorkItem,
    ) -> tokio::sync::oneshot::Receiver<CommunityDispatchOutcome> {
        let rx = insert_pending_job(node, job.clone(), None);
        node.community_work_tx
            .as_ref()
            .unwrap()
            .try_send(job.clone())
            .unwrap();
        rx
    }

    async fn poll(node: &NodeState, id: &str) -> Option<WorkItem> {
        routing::poll(
            node,
            id,
            &mut *node.community_work_queue.as_ref().unwrap().lock().await,
        )
        .ok()
        .flatten()
    }

    async fn claim(node: &NodeState, id: &str) -> Value {
        community_claim_work(
            AxumState(node.clone()),
            Json(ClaimWorkRequest {
                worker_id: id.into(),
                capabilities: vec!["inference".into()],
                model_id: test_model_id(),
                execution_profile: canonical_profile(),
            }),
        )
        .await
        .unwrap()
        .0
    }

    fn decline(job: &str, worker: &str) -> WorkResult {
        serde_json::from_value(json!({"job_id": job, "worker_id": worker,
            "success": false, "declined": true}))
        .unwrap()
    }

    #[tokio::test]
    async fn fastest_waiter_wins_and_only_slow_waiter_has_no_delay() {
        let n = node(&["fast", "medium", "slow"]);
        for (id, speed) in [("fast", 1), ("medium", 4), ("slow", 20)] {
            waiting(&n, id);
            routing::record_slow(&n, id, speed, 1);
        }
        let job = item("fastest");
        let _rx = enqueue(&n, &job);
        assert!(poll(&n, "slow").await.is_none());
        assert!(poll(&n, "medium").await.is_none());
        assert_eq!(poll(&n, "fast").await.unwrap().job_id, job.job_id);
        n.community_active_jobs.remove("fast");
        n.community_active_jobs.remove("medium");
        let job = item("only-slow");
        let _rx = enqueue(&n, &job);
        let start = tokio::time::Instant::now();
        assert_eq!(poll(&n, "slow").await.unwrap().job_id, job.job_id);
        assert!(start.elapsed() < Duration::from_millis(25));
    }

    #[tokio::test]
    async fn exploration_limits_twenty_new_workers_to_one_job_in_four() {
        let ids: Vec<_> = (0..20).map(|i| format!("new-{i:02}")).collect();
        let mut names: Vec<_> = ids.iter().map(String::as_str).collect();
        names.push("fast");
        let n = node(&names);
        for id in &names {
            waiting(&n, id);
        }
        routing::record_slow(&n, "fast", 1, 100);
        let mut fast_count = 0;
        for i in 0..100 {
            let _rx = enqueue(&n, &item(&format!("explore-{i}")));
            if poll(&n, "fast").await.is_some() {
                fast_count += 1;
                continue;
            }
            let mut found = false;
            for id in &ids {
                if poll(&n, id).await.is_some() {
                    routing::assigned(&n, id);
                    routing::record_slow(&n, id, 10, 1);
                    found = true;
                    break;
                }
            }
            assert!(found);
        }
        assert!(fast_count >= 75, "fast worker received {fast_count}");
    }

    #[tokio::test(start_paused = true)]
    async fn held_item_releases_at_1500ms_and_never_extends_its_enqueue_deadline() {
        let n = node(&["fast", "slow"]);
        for id in ["fast", "slow"] {
            waiting(&n, id);
        }
        routing::record_slow(&n, "fast", 1, 1);
        routing::record_slow(&n, "slow", 20, 1);
        let job = item("hold");
        let _rx = enqueue(&n, &job);
        tokio::time::advance(Duration::from_millis(1_000)).await;
        assert!(poll(&n, "slow").await.is_none());
        tokio::time::advance(Duration::from_millis(499)).await;
        assert!(poll(&n, "slow").await.is_none());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(poll(&n, "slow").await.unwrap().job_id, job.job_id);
    }

    #[tokio::test]
    async fn cancelled_claim_and_full_queue_preserve_held_sender() {
        let mut n = node(&["fast", "slow"]);
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        n.community_work_tx = Some(Arc::new(tx));
        n.community_work_queue = Some(Arc::new(tokio::sync::Mutex::new(rx)));
        waiting(&n, "fast");
        routing::record_slow(&n, "fast", 1, 1);
        routing::record_slow(&n, "slow", 20, 1);
        let job = item("cancel");
        let mut answer = enqueue(&n, &job);
        let clone = n.clone();
        let slow = tokio::spawn(async move { claim(&clone, "slow").await });
        // The receiver becoming empty proves the slow handler moved the item
        // into shared storage before entering its polling sleep.
        loop {
            if n.community_work_queue
                .as_ref()
                .unwrap()
                .lock()
                .await
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        slow.abort();
        assert!(slow.await.unwrap_err().is_cancelled());
        let other = item("queue-filler");
        let _other_rx = enqueue(&n, &other);
        assert_eq!(poll(&n, "fast").await.unwrap().job_id, job.job_id);
        assert!(matches!(
            answer.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            n.community_work_queue.as_ref().unwrap().lock().await.len(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn exclusion_and_model_match_apply_before_and_after_hold_expiry() {
        let n = node(&["decliner", "other", "wrong-model"]);
        for id in ["decliner", "other", "wrong-model"] {
            waiting(&n, id);
        }
        n.community_workers
            .get_mut("wrong-model")
            .unwrap()
            .0
            .model_id = Some(Hash256::ZERO.to_hex());
        let job = item("exclude");
        let _rx = enqueue(&n, &job);
        n.community_work_results
            .as_ref()
            .unwrap()
            .get_mut(&job.job_id)
            .unwrap()
            .routing
            .excluded_worker = Some("decliner".into());
        assert!(poll(&n, "decliner").await.is_none());
        assert!(poll(&n, "wrong-model").await.is_none());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(poll(&n, "decliner").await.is_none());
        assert!(poll(&n, "wrong-model").await.is_none());
        assert_eq!(poll(&n, "other").await.unwrap().job_id, job.job_id);
    }

    #[tokio::test]
    async fn decline_invalidates_old_waiter_until_new_claim_and_drops_its_rank() {
        let n = node(&["decliner", "new"]);
        waiting(&n, "decliner");
        let job = item("decline-old");
        let rx = insert_pending_job(&n, job.clone(), Some("decliner".into()));
        community_submit_work(AxumState(n.clone()), Json(decline(&job.job_id, "decliner")))
            .await
            .unwrap();
        assert!(rx.await.unwrap().result.declined);
        assert!(!routing::has_other_waiter(
            &n,
            &test_model_id(),
            "unrelated"
        ));
        assert!(routing::sample(&n, "decliner").is_none());
        // A stale empty reservation is still ignored.
        n.community_active_jobs
            .insert("decliner".into(), String::new());
        assert!(!routing::has_other_waiter(
            &n,
            &test_model_id(),
            "unrelated"
        ));
        routing::open_claim(
            &n,
            "decliner",
            parse_hash256_hex(&test_model_id(), "model").unwrap(),
        );
        assert!(routing::has_other_waiter(&n, &test_model_id(), "unrelated"));
        waiting(&n, "new");
        let _rx = enqueue(&n, &item("new-beats-decliner"));
        assert!(poll(&n, "decliner").await.is_none());
        assert!(poll(&n, "new").await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_penalizes_missing_submit_but_not_verification_delay() {
        for submitted in [false, true] {
            let n = node(&["fast", "other"]);
            routing::record_slow(&n, "fast", 1, 100);
            routing::record_slow(&n, "other", 1, 1);
            let cloned = n.clone();
            let dispatch = tokio::spawn(async move {
                dispatch_to_community_worker(&cloned, "ARC".into(), 1, Some(test_model_id())).await
            });
            let body = claim(&n, "fast").await;
            assert_eq!(body["status"], "work");
            if submitted {
                let mut p = n
                    .community_work_results
                    .as_ref()
                    .unwrap()
                    .get_mut(body["job_id"].as_str().unwrap())
                    .unwrap();
                p.first_submit_at = Some(tokio::time::Instant::now());
                p.submitting = true;
            }
            let budget = community_dispatch_timeout_secs(
                community_worker_generation_context(n.inference_model.as_ref().unwrap(), "ARC", 1)
                    .unwrap()
                    .1
                    .required_positions,
            )
            .unwrap();
            tokio::time::advance(Duration::from_secs(budget)).await;
            assert!(!dispatch.await.unwrap().unwrap_err().local_fallback_safe);
            if submitted {
                assert_eq!(routing::sample(&n, "fast"), Some(0.01));
            } else {
                assert!(routing::sample(&n, "fast").unwrap() > 1.0);
                waiting(&n, "fast");
                waiting(&n, "other");
                let _rx = enqueue(&n, &item("after-timeout"));
                assert!(poll(&n, "fast").await.is_none());
                assert!(poll(&n, "other").await.is_some());
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn accepted_sample_uses_coordinator_time_positions_and_not_worker_total_ms() {
        let n = node(&["worker"]);
        let job = item("sample");
        let _rx = insert_pending_job(&n, job.clone(), Some("worker".into()));
        let (_, mut pending) = n
            .community_work_results
            .as_ref()
            .unwrap()
            .remove(&job.job_id)
            .unwrap();
        let model = n.inference_model.as_ref().unwrap();
        pending.routing.prompt_len = model.encode(&job.input).len();
        pending.assigned_at = Some(tokio::time::Instant::now());
        tokio::time::advance(Duration::from_secs(2)).await;
        pending.first_submit_at = Some(tokio::time::Instant::now());
        let mut result = successful_test_result(job.job_id, "worker");
        result.tokens_generated = 1;
        result.total_ms = u64::MAX;
        let positions = model
            .preflight_generation(pending.routing.prompt_len, 1)
            .unwrap()
            .required_positions;
        let arrived = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_secs(100)).await;
        routing::record_accepted(&n, &pending, &result, arrived);
        assert_eq!(routing::sample(&n, "worker"), Some(2.0 / positions as f64));
        result.total_ms = 0;
        routing::record_accepted(&n, &pending, &result, arrived);
        assert!((routing::sample(&n, "worker").unwrap() - 2.0 / positions as f64).abs() < 1e-12);
    }

    #[tokio::test]
    async fn routing_samples_leave_latency_stats_output_unchanged() {
        let n = node(&["worker"]);
        record_latency(&n.latency_stats, "worker:worker", 100);
        let before = inference_latency_stats(AxumState(n.clone())).await.0;
        routing::record_slow(&n, "worker", 1000, 1);
        let after = inference_latency_stats(AxumState(n.clone())).await.0;
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn targeted_item_skips_preference_hold() {
        let n = node(&["target", "fast"]);
        waiting(&n, "fast");
        routing::record_slow(&n, "fast", 1, 100);
        let mut job = item("targeted");
        job.expected_worker_id = Some("target".into());
        let _rx = enqueue(&n, &job);
        let body = claim(&n, "target").await;
        assert_eq!(body["job_id"], job.job_id);
    }

    #[tokio::test]
    async fn retry_excludes_first_dequeuer_with_full_queue_and_preserves_second_outcome() {
        for second_decline in [false, true] {
            let n = node(&["first", "second"]);
            let cloned = n.clone();
            let dispatch = tokio::spawn(async move {
                dispatch_to_community_worker(&cloned, "ARC".into(), 1, Some(test_model_id())).await
            });
            let first = claim(&n, "first").await;
            let first_job = first["job_id"].as_str().unwrap();
            waiting(&n, "second");
            community_submit_work(AxumState(n.clone()), Json(decline(first_job, "first")))
                .await
                .unwrap();
            while n
                .community_work_queue
                .as_ref()
                .unwrap()
                .lock()
                .await
                .is_empty()
            {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                n.public_inference_permits.available_permits(),
                PUBLIC_INFERENCE_CONCURRENCY - 1
            );
            waiting(&n, "first");
            assert!(poll(&n, "first").await.is_none()); // retry is safely held
            let mut fillers = Vec::new();
            for i in 0..16 {
                fillers.push(enqueue(&n, &item(&format!("fill-{i}"))));
            }
            n.community_active_jobs.remove("second");
            let second = claim(&n, "second").await;
            let second_job = second["job_id"].as_str().unwrap();
            assert_ne!(first_job, second_job);
            let late =
                community_submit_work(AxumState(n.clone()), Json(decline(first_job, "first")))
                    .await
                    .unwrap_err();
            assert_eq!(late.0, StatusCode::NOT_FOUND);
            if second_decline {
                community_submit_work(AxumState(n.clone()), Json(decline(second_job, "second")))
                    .await
                    .unwrap();
                assert!(dispatch.await.unwrap().unwrap_err().local_fallback_safe);
            } else {
                // Verification/settlement have their own tests. Send a fully
                // identifiable second outcome through the actual dispatcher.
                let (_, pending) = n
                    .community_work_results
                    .as_ref()
                    .unwrap()
                    .remove(second_job)
                    .unwrap();
                let settlement = json!({"job_id": second_job});
                let release = json!({"settlement_status_url": format!("/community/reward_status/{second_job}")});
                pending
                    .sender
                    .send(CommunityDispatchOutcome {
                        result: successful_test_result(second_job.into(), "second"),
                        verification: None,
                        settlement: Some(settlement.clone()),
                        answer_release: Some(release.clone()),
                    })
                    .unwrap();
                let outcome = dispatch.await.unwrap().unwrap();
                assert_eq!(outcome.result.job_id, second_job);
                assert_eq!(outcome.settlement, Some(settlement));
                assert_eq!(outcome.answer_release, Some(release));
            }
            assert_eq!(
                n.public_inference_permits.available_permits(),
                PUBLIC_INFERENCE_CONCURRENCY
            );
        }
    }

    #[test]
    fn retry_budget_has_exact_30s_and_maximum_position_boundaries() {
        assert!(routing::retry_fits(Duration::from_secs(30), 45));
        assert!(!routing::retry_fits(Duration::from_millis(30_001), 45));
        let budget = community_dispatch_timeout_secs(260).unwrap();
        assert_eq!(budget, 3891);
        assert!(routing::retry_fits(Duration::from_secs(9), budget));
        assert!(!routing::retry_fits(Duration::from_millis(9_001), budget));
    }
    #[tokio::test(start_paused = true)]
    async fn declines_without_retry_budget_or_waiter_and_recovery_probes_fall_back() {
        for case in ["late", "largest", "no-waiter", "probe"] {
            let first_id = format!("0x{}", "1".repeat(64));
            let mut n = node(&[&first_id, "other"]);
            n.inference_model = Some(context_test_inference_model(512));
            let prompt_len = n.inference_model.as_ref().unwrap().encode("ARC").len();
            let max_tokens = if case == "largest" {
                (259 - prompt_len) as u32
            } else {
                1
            };
            assert!(max_tokens <= INFERENCE_RUN_MAX_TOKENS);
            let probe = if case == "probe" {
                let mut bytes = [3; 32];
                let prefix = arc_types::transaction::RECOVERY_REWARD_PROBE_PREFIX;
                bytes[..prefix.len()].copy_from_slice(&prefix);
                Some(Hash256(bytes))
            } else {
                None
            };
            let target = probe.map(|_| first_id.clone());
            let cloned = n.clone();
            let dispatch = tokio::spawn(async move {
                dispatch_to_community_worker_with_probe(
                    &cloned,
                    "ARC".into(),
                    max_tokens,
                    Some(test_model_id()),
                    probe,
                    target,
                )
                .await
            });
            let first = claim(&n, &first_id).await;
            assert_eq!(first["status"], "work");
            if case != "no-waiter" {
                waiting(&n, "other");
            }
            if case == "late" {
                tokio::time::advance(Duration::from_millis(30_001)).await;
            }
            if case == "largest" {
                tokio::time::advance(Duration::from_millis(9_001)).await;
            }
            community_submit_work(
                AxumState(n.clone()),
                Json(decline(first["job_id"].as_str().unwrap(), &first_id)),
            )
            .await
            .unwrap();
            assert!(
                dispatch.await.unwrap().unwrap_err().local_fallback_safe,
                "{case}"
            );
            assert!(
                n.community_work_queue
                    .as_ref()
                    .unwrap()
                    .lock()
                    .await
                    .is_empty(),
                "{case}"
            );
            assert!(
                n.community_work_results.as_ref().unwrap().is_empty(),
                "{case}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn withdrawal_never_retries_even_with_another_waiter_and_records_slow_worker() {
        let n = node(&["dead", "other"]);
        let cloned = n.clone();
        let dispatch = tokio::spawn(async move {
            dispatch_to_community_worker(&cloned, "ARC".into(), 1, Some(test_model_id())).await
        });
        assert_eq!(claim(&n, "dead").await["status"], "work");
        waiting(&n, "other");
        n.community_workers.get_mut("dead").unwrap().1 =
            Instant::now() - Duration::from_secs(COMMUNITY_WORKER_TTL_SECS + 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(dispatch.await.unwrap().unwrap_err().local_fallback_safe);
        assert!(routing::sample(&n, "dead").is_some());
        assert!(
            n.community_work_queue
                .as_ref()
                .unwrap()
                .lock()
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn worker_failure_records_budget_per_position_and_never_retries() {
        let n = node(&["failed", "other"]);
        let cloned = n.clone();
        let dispatch = tokio::spawn(async move {
            dispatch_to_community_worker(&cloned, "ARC".into(), 1, Some(test_model_id())).await
        });
        let body = claim(&n, "failed").await;
        waiting(&n, "other");
        let mut result = decline(body["job_id"].as_str().unwrap(), "failed");
        result.declined = false;
        community_submit_work(AxumState(n.clone()), Json(result))
            .await
            .unwrap();
        assert!(!dispatch.await.unwrap().unwrap_err().local_fallback_safe);
        let positions =
            community_worker_generation_context(n.inference_model.as_ref().unwrap(), "ARC", 1)
                .unwrap()
                .1
                .required_positions;
        assert_eq!(
            routing::sample(&n, "failed"),
            Some(community_dispatch_timeout_secs(positions).unwrap() as f64 / positions as f64)
        );
        assert!(
            n.community_work_queue
                .as_ref()
                .unwrap()
                .lock()
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn changed_registration_cannot_retarget_an_existing_claim() {
        let n = node(&["worker", "other"]);
        waiting(&n, "worker");
        waiting(&n, "other");
        let mut job = item("changed-model");
        job.model_id = Some(Hash256::ZERO.to_hex());
        n.community_workers.get_mut("worker").unwrap().0.model_id = job.model_id.clone();
        let _rx = enqueue(&n, &job);
        assert!(poll(&n, "worker").await.is_none());
        routing::open_claim(&n, "worker", Hash256::ZERO);
        assert_eq!(poll(&n, "worker").await.unwrap().job_id, job.job_id);
    }
}
