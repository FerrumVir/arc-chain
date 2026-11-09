#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TfLegacyWorkResult {
    /// Must match the WorkItem.job_id.
    pub job_id: String,
    /// Worker's self-chosen identifier (hex of validator pubkey).
    pub worker_id: String,
    /// True if the worker produced output; false on inference error.
    #[serde(default = "default_true")]
    pub success: bool,
    /// True when the worker is deliberately returning an extra concurrent
    /// assignment without attempting inference. This unblocks the coordinator
    /// without counting a capacity race as a model failure.
    #[serde(default)]
    pub declined: bool,
    /// Generated text (decoded from the worker's tokens).
    #[serde(default)]
    pub output: String,
    /// BLAKE3 of the output token sequence (hex, 0x-prefixed). The chain
    /// uses this for cross-worker hash-majority verification.
    #[serde(default)]
    pub output_hash: String,
    /// Number of tokens generated.
    #[serde(default)]
    pub tokens_generated: u64,
    /// Total wall time on the worker, in milliseconds (encode + generate + decode).
    #[serde(default)]
    pub total_ms: u64,
    /// Average ms per generated token.
    #[serde(default)]
    pub ms_per_token: u64,
    /// Engine identifier ("INT8 integer (community worker)", etc.) for analytics.
    #[serde(default)]
    pub engine: String,
    /// Optional error message when success=false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Hex-encoded `bincode(arc_types::Transaction)` of the worker-signed
    /// InferenceAttestation (tx 0x16) for this completion. Lets the seed
    /// post the attestation on-chain with `from = worker_address`, so
    /// rewards accrue to the worker who actually did the work — not the
    /// seed that routed the request. Optional during the v0.7.0
    /// transition; pre-v0.7 workers don't send it, and the seed accepts
    /// the submit anyway (just no on-chain record).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_attestation_hex: Option<String>,
}

// Included inside rpc::tests to reuse its authenticated validator fixtures.
#[derive(Clone)]
struct TfHolder {
    node: NodeState,
    captured: Arc<std::sync::Mutex<Vec<ForwardShardRequest>>>,
    locks: Arc<dashmap::DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    compute_ms: u64,
    network_ms: u64,
    refuse: Arc<std::sync::atomic::AtomicBool>,
}
async fn tf_forward(
    AxumState(holder): AxumState<TfHolder>,
    Json(signed): Json<CommunitySignedRequest<ValidatorForwardShardRequest>>,
) -> Result<(HeaderMap, Json<ForwardShardResponse>), (StatusCode, String)> {
    holder
        .captured
        .lock()
        .unwrap()
        .push(signed.payload.request.clone());
    if holder.refuse.load(Ordering::SeqCst) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "test queue full".into()));
    }
    // Simulate transit outside the per-request cache lock, then computation
    // inside it. The real authenticated forward and its real KV lock still run.
    tokio::time::sleep(std::time::Duration::from_millis(holder.network_ms)).await;
    let lock = holder
        .locks
        .entry(signed.payload.request.request_id.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;
    tokio::time::sleep(std::time::Duration::from_millis(holder.compute_ms)).await;
    inference_forward_shard(AxumState(holder.node), Json(signed)).await
}

struct TfFixture {
    node: NodeState,
    pipeline: Vec<PipelineHop>,
    captured: Arc<std::sync::Mutex<Vec<ForwardShardRequest>>>,
    servers: tokio::task::JoinSet<()>,
    refuse: Arc<std::sync::atomic::AtomicBool>,
}
impl TfFixture {
    async fn build(
        ranges: usize,
        shared: bool,
        compute_ms: u64,
        network_ms: u64,
        eos: Vec<u32>,
    ) -> Self {
        let (keys, state, _temporary) = recovered_six_validator_state();
        let mut model = test_reward_inference_model();
        model.config.n_layers = ranges;
        model.config.max_seq = 256;
        model.config.rope_cos = vec![1 << 16; 512];
        model.config.rope_sin = vec![0; 512];
        model.config.eos_tokens = eos;
        model.layers = (0..ranges)
            .map(|_| test_reward_inference_model().layers.remove(0))
            .collect();
        let model = Arc::new(model);
        let model_id = arc_crypto::hash_bytes(b"teacher-forced-fixture");
        let mut node = fake_node_with_workers(Vec::new());
        node.state = state;
        node.validator_address = keys[5].address();
        node.validator_keypair = Some(Arc::new(keys[5].clone()));
        node.inference_model = Some(model.clone());
        node.model_artifact_id = Some(model_id);
        *node.dag_validators.write() = node.state.active_validators();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let holders = if shared { 6 } else { ranges * 3 };
        let mut pipeline: Vec<PipelineHop> =
            (0..ranges).map(|r| ((r, r + 1), Vec::new())).collect();
        let mut servers = tokio::task::JoinSet::new();
        for h in 0..holders {
            let mut holder_node = node.clone();
            // Independent holders need independent KV maps and admission gates.
            let fresh = fake_node_with_workers(Vec::new());
            holder_node.shard_kv_caches = fresh.shard_kv_caches;
            holder_node.shard_kv_cache_metadata = fresh.shard_kv_cache_metadata;
            holder_node.shard_kv_cache_gate = fresh.shard_kv_cache_gate;
            holder_node.validator_address = keys[h % 6].address();
            holder_node.validator_keypair = Some(Arc::new(keys[h % 6].clone()));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            for (r, (_, replicas)) in pipeline.iter_mut().enumerate() {
                if (shared && (0..3).any(|offset| (r + offset) % 6 == h)) || (!shared && h / 3 == r)
                {
                    let info = ShardInfo {
                        start_layer: r,
                        end_layer: r + 1,
                        total_layers: ranges,
                        model_id: format!("0x{}", model_id.to_hex()),
                        model_name: model_display_name(&model),
                        execution_profile: canonical_profile(),
                        memory_mb: 1,
                        full_model_mb: 1,
                        socket_addr: origin.clone(),
                        node_name: format!("holder-{h}"),
                    };
                    holder_node.shard_infos.push(info.clone());
                    replicas.push(info);
                }
            }
            let holder = TfHolder {
                node: holder_node.clone(),
                captured: captured.clone(),
                locks: Arc::new(dashmap::DashMap::new()),
                compute_ms,
                network_ms,
                refuse: refuse.clone(),
            };
            let app = Router::new()
                .route("/inference/forward_shard", post(tf_forward))
                .with_state(holder)
                .merge(
                    Router::new()
                        .route("/network/info", get(network_info))
                        .route("/inference/cleanup_shard", post(inference_cleanup_shard))
                        .with_state(holder_node),
                );
            servers.spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
        }
        node.community_verification_pipeline_override = Some(pipeline.clone());
        Self {
            node,
            pipeline,
            captured,
            servers,
            refuse,
        }
    }
    fn strategies(&self) -> Vec<HopStrategy> {
        self.pipeline
            .iter()
            .map(|_| HopStrategy::Fanout {
                fanout: 3,
                needed: 2,
                allow_degraded_quorum: false,
            })
            .collect()
    }
    fn take_requests(&self) -> std::collections::BTreeMap<(usize, usize, usize), Vec<Value>> {
        let mut groups = std::collections::BTreeMap::new();
        for request in self.captured.lock().unwrap().drain(..) {
            let key = (request.start_layer, request.end_layer, request.position);
            let mut value = serde_json::to_value(request).unwrap();
            value.as_object_mut().unwrap().remove("request_id");
            groups.entry(key).or_insert_with(Vec::new).push(value);
        }
        groups
    }
    async fn sequential(&self, tokens: &[u32], max: u32, id: &str) -> PipelineRun {
        let run = run_pipeline(
            &self.node,
            self.node.inference_model.as_ref().unwrap(),
            &self.pipeline,
            id,
            tokens,
            max,
            &self.strategies(),
            true,
            true,
        )
        .await
        .unwrap();
        cleanup_shards(&self.node, &self.pipeline, id).await;
        run
    }
    async fn fast(
        &self,
        tokens: &[u32],
        claimed: &[u32],
        max: u32,
        id: &str,
        duration: std::time::Duration,
    ) -> Result<PipelineRun, String> {
        let result = teacher_forced::run_teacher_forced_pipeline(
            &self.node,
            self.node.inference_model.as_ref().unwrap(),
            &self.pipeline,
            id,
            tokens,
            claimed,
            max,
            &self.strategies(),
            tokio::time::Instant::now() + duration,
        )
        .await;
        cleanup_shards(&self.node, &self.pipeline, id).await;
        result
    }
}

#[tokio::test]
async fn teacher_forced_requests_match_sequential_including_64_window_and_eos() {
    for eos in [Vec::new(), vec![0, 1, 2, 3, 4, 5, 6, 7]] {
        let mut f = TfFixture::build(6, true, 0, 0, eos).await;
        let tokens = vec![1, 3, 3];
        let sequential = f.sequential(&tokens, 70, "equivalence-sequential").await;
        let before = f.take_requests();
        let fast = f
            .fast(
                &tokens,
                &sequential.generated,
                70,
                "equivalence-fast",
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap();
        let after = f.take_requests();
        assert_eq!(before.len(), after.len());
        for (key, requests) in &before {
            assert_eq!(requests.len(), 3, "{key:?}");
            assert_eq!(after[key].len(), 3);
            assert!(
                requests
                    .iter()
                    .chain(after[key].iter())
                    .all(|value| value == &requests[0]),
                "different request at {key:?}"
            );
        }
        assert_eq!(fast.generated, sequential.generated);
        let replicas = vec![3; 6];
        let active = f.node.state.active_validators();
        assert_eq!(
            validate_community_range_position_quorums(
                &f.pipeline,
                &replicas,
                &fast.hop_stats,
                &fast.votes,
                &active
            )
            .unwrap(),
            validate_community_range_position_quorums(
                &f.pipeline,
                &replicas,
                &sequential.hop_stats,
                &sequential.votes,
                &active
            )
            .unwrap()
        );
        f.servers.shutdown().await;
    }
}

#[tokio::test]
async fn teacher_forced_bad_claims_fall_back_to_identical_verdict_and_stop_early() {
    let mut f = TfFixture::build(2, false, 0, 0, vec![]).await;
    let model = f.node.inference_model.as_ref().unwrap();
    let item = WorkItem {
        job_id: "teacher-forced-verdict".into(),
        input: "ARC".into(),
        max_tokens: 20,
        model_id: Some(format!("0x{}", f.node.model_artifact_id.unwrap().to_hex())),
        execution_profile: canonical_profile(),
        transaction_domain: None,
        expected_worker_id: None,
        submitted_at_unix_ms: 0,
        expires_at_unix_ms: u64::MAX,
    };
    let canonical = recompute_community_output_with_quorum(&f.node, &item)
        .await
        .unwrap();
    let result_for = |tokens: Vec<u32>| {
        crate::community_worker::successful_work_result(
            &json!({"accepts_output_tokens": true}),
            item.job_id.clone(),
            f.node.validator_address,
            tokens.clone(),
            model.decode(&tokens),
            1,
            None,
        )
    };
    let good = result_for(canonical.generated.clone());
    let accepted = verify_community_result_with_quorum(&f.node, &item, &good)
        .await
        .unwrap();
    assert_eq!(accepted.output_hash, canonical.output_hash);
    assert_eq!(accepted.tokens_generated, canonical.generated.len());
    assert_eq!(
        accepted.range_position_quorum_count,
        canonical.range_position_quorum_count
    );
    for index in [0, 10, 19] {
        let mut tokens = canonical.generated.clone();
        tokens[index] = (tokens[index] + 1) % 8;
        let bad = result_for(tokens.clone());
        f.take_requests();
        let (prompt, _) =
            community_worker_generation_context(model, &item.input, item.max_tokens).unwrap();
        let error = f
            .fast(
                &prompt,
                &tokens,
                item.max_tokens,
                &format!("wrong-{index}"),
                std::time::Duration::from_secs(10),
            )
            .await
            .err()
            .unwrap();
        assert!(error.contains("mismatch"), "{error}");
        let requests = f.take_requests();
        assert!(
            requests
                .keys()
                .filter(|(start, _, _)| *start == 1)
                .all(|(_, _, p)| *p <= prompt.len() - 1 + index)
        );
        let baseline = compare_community_result_with_tokens(
            &bad,
            &canonical.generated,
            &canonical.output_text,
        )
        .unwrap_err();
        match verify_community_result_with_quorum(&f.node, &item, &bad).await {
            Err(CommunityResultVerificationError::Invalid(error)) => assert_eq!(error, baseline),
            _ => panic!("bad claim did not produce sequential Invalid"),
        }
    }
    for tokens in [vec![], canonical.generated[..3].to_vec()] {
        assert!(matches!(
            verify_community_result_with_quorum(&f.node, &item, &result_for(tokens)).await,
            Err(CommunityResultVerificationError::Invalid(_))
        ));
    }
    let mut wrong_hint = good.clone();
    wrong_hint.output_tokens = Some(vec![7]);
    assert_eq!(
        verify_community_result_with_quorum(&f.node, &item, &wrong_hint)
            .await
            .unwrap()
            .output_hash,
        canonical.output_hash
    );
    let (prompt, _) =
        community_worker_generation_context(model, &item.input, item.max_tokens).unwrap();
    f.refuse.store(true, Ordering::SeqCst);
    assert!(
        f.fast(
            &prompt,
            &canonical.generated,
            item.max_tokens,
            "refusal",
            std::time::Duration::from_secs(10)
        )
        .await
        .err()
        .unwrap()
        .contains("429")
    );
    f.refuse.store(false, Ordering::SeqCst);
    assert!(
        f.fast(
            &prompt,
            &canonical.generated,
            item.max_tokens,
            "deadline",
            std::time::Duration::ZERO
        )
        .await
        .err()
        .unwrap()
        .contains("deadline")
    );
    assert!(!teacher_forced::valid_claim_shape(&[1, 2, 3], 3, &[2]));
    f.servers.shutdown().await;
}

#[tokio::test]
async fn teacher_forced_timing_three_topologies() {
    for (label, shared, network) in [("a", false, 0), ("b", true, 0), ("c", true, 100)] {
        let mut f = TfFixture::build(6, shared, 50, network, vec![]).await;
        let prompt = vec![3; 20];
        let start = std::time::Instant::now();
        let sequential = f
            .sequential(&prompt, 32, &format!("timing-seq-{label}"))
            .await;
        let sequential_ms = start.elapsed().as_millis();
        let start = std::time::Instant::now();
        let fast = f
            .fast(
                &prompt,
                &sequential.generated,
                32,
                &format!("timing-tf-{label}"),
                std::time::Duration::from_secs(90),
            )
            .await
            .unwrap();
        let fast_ms = start.elapsed().as_millis();
        assert_eq!(fast.generated, sequential.generated);
        println!("TIMING {label}: sequential={sequential_ms}ms teacher_forced={fast_ms}ms");
        if label == "a" {
            assert!(fast_ms * 2 < sequential_ms);
        }
        f.servers.shutdown().await;
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TfLegacyApprovalPayload {
    coordinator_id: String,
    reward: arc_types::transaction::CommunityInferenceRewardBody,
    input: String,
    output: String,
    tokens_generated: u64,
    execution_profile: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TfLegacyJournal {
    schema: String,
    verified_at_unix_ms: u64,
    payload_hash: Hash256,
    payload: TfLegacyApprovalPayload,
}

#[test]
fn teacher_forced_optional_fields_preserve_old_bytes_and_journal_hash() {
    let mut node = fake_node_with_workers(Vec::new());
    let directory = std::env::temp_dir().join(format!("tf-journal-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    node.community_settlement_journal_dir = Some(Arc::new(directory));
    let mut payload = test_verified_settlement_payload(Hash256([45; 32]), 500);
    let bytes = serde_json::to_vec(&payload).unwrap();
    let old: TfLegacyApprovalPayload = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(bytes, serde_json::to_vec(&old).unwrap());
    let hash = community_payload_hash(&payload).unwrap();
    payload.output_tokens = Some(vec![1, 2]);
    retain_verified_settlement(&node, payload.clone()).unwrap();
    let record = node
        .community_verified_settlements
        .get(&payload.reward.job_id)
        .unwrap();
    assert!(record.payload.output_tokens.is_none());
    assert_eq!(community_payload_hash(&record.payload).unwrap(), hash);
    let journal =
        std::fs::read(settlement_journal_file(&node, payload.reward.job_id).unwrap()).unwrap();
    let old: TfLegacyJournal = serde_json::from_slice(&journal).unwrap();
    assert_eq!(old.payload_hash, hash);
    assert_eq!(serde_json::to_vec(&old.payload).unwrap(), bytes);
    // Serialize and deserialize the legacy worker shape, retaining field order.
    let result = successful_test_result("job".into(), "worker");
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("output_tokens"));
    let old: TfLegacyWorkResult = serde_json::from_str(&json).unwrap();
    assert_eq!(serde_json::to_string(&old).unwrap(), json);
    let decoded: WorkResult = serde_json::from_str(&json).unwrap();
    assert!(decoded.output_tokens.is_none());
    assert_eq!(serde_json::to_string(&decoded).unwrap(), json);
}

#[derive(Clone)]
struct TfApprovalWire {
    info: Value,
    received: Arc<std::sync::Mutex<Vec<Value>>>,
    reject_tokens: bool,
    delay_ms: u64,
}
async fn tf_approval_info(AxumState(state): AxumState<TfApprovalWire>) -> Json<Value> {
    tokio::time::sleep(std::time::Duration::from_millis(state.delay_ms)).await;
    Json(state.info)
}
async fn tf_approval_post(
    AxumState(state): AxumState<TfApprovalWire>,
    Json(body): Json<Value>,
) -> (StatusCode, String) {
    let has_tokens = body["payload"].get("output_tokens").is_some();
    state.received.lock().unwrap().push(body);
    if state.reject_tokens && has_tokens {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown field `output_tokens`".into(),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "test approval captured".into(),
        )
    }
}
async fn tf_wire_server(state: TfApprovalWire, servers: &mut tokio::task::JoinSet<()>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/network/info", get(tf_approval_info))
        .route(COMMUNITY_REWARD_APPROVE_PATH, post(tf_approval_post))
        .with_state(state);
    servers.spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    origin
}

#[tokio::test]
async fn teacher_forced_approval_capability_and_resigned_422_retry() {
    let key = Arc::new(arc_crypto::KeyPair::generate_ed25519());
    let payload = test_verified_settlement_payload(Hash256([6; 32]), 500);
    let mut servers = tokio::task::JoinSet::new();
    for (capable, rejects) in [(false, false), (true, false), (true, true)] {
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let origin = tf_wire_server(TfApprovalWire { info: json!({"validator_address": Hash256([2;32]).to_hex(), "community_approval_output_tokens": capable}), received: received.clone(), reject_tokens: rejects, delay_ms: 0 }, &mut servers).await;
        let audience = probe_reward_approval_audience(reqwest::Client::new(), origin)
            .await
            .unwrap();
        let _ = post_remote_reward_approval(
            reqwest::Client::new(),
            audience,
            payload.clone(),
            Some(vec![1, 2]),
            key.clone(),
        )
        .await;
        let requests = received.lock().unwrap();
        assert_eq!(requests.len(), if rejects { 2 } else { 1 });
        assert_eq!(
            requests[0]["payload"].get("output_tokens").is_some(),
            capable
        );
        if rejects {
            assert!(requests[1]["payload"].get("output_tokens").is_none());
            assert_ne!(requests[0]["signature"], requests[1]["signature"]);
            assert_ne!(requests[0]["nonce"], requests[1]["nonce"]);
        }
    }
    servers.shutdown().await;
}

#[tokio::test]
async fn teacher_forced_skip_counts_distinct_active_validators_and_pending_retries() {
    let mut fixture = ReleaseOnVerificationFixture::build(false).await;
    let original = fixture.coordinator.community_rpc_bases.as_ref().clone();
    let mut servers = tokio::task::JoinSet::new();
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let own = tf_wire_server(
        TfApprovalWire {
            info: json!({"validator_address": fixture.coordinator.validator_address.to_hex()}),
            received: received.clone(),
            reject_tokens: false,
            delay_ms: 0,
        },
        &mut servers,
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bad_gateway = format!("http://{}", listener.local_addr().unwrap());
    servers.spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/network/info", get(|| async { StatusCode::BAD_GATEWAY })),
        )
        .await
        .unwrap();
    });
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let refused = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    fixture.coordinator.community_rpc_bases = Arc::new(vec![
        own,
        original[0].clone(),
        original[1].clone(),
        original[2].clone(),
        refused,
        bad_gateway,
    ]);
    let job = fixture.start_job().await;
    let Json(submit) = job.submit.await.unwrap().unwrap();
    let Json(answer) = job.run.await.unwrap().unwrap();
    assert!(submit.to_string().contains("pending"), "{submit}");
    assert!(answer.to_string().contains("pending"), "{answer}");
    fixture.coordinator.runtime_tasks.lock().abort_all();
    for _ in 0..3 {
        let pending = attempt_verified_community_reward(&fixture.coordinator, job.job_hash)
            .await
            .unwrap_err();
        assert!(pending.contains("4 distinct active"), "{pending}");
    }
    assert_eq!(fixture.approval_request_count(), 0);
    assert!(received.lock().unwrap().is_empty());
    // A second origin reporting the same address contributes no extra vote.
    let mut duplicate = fixture.coordinator.community_rpc_bases.as_ref().clone();
    duplicate.push(original[0].clone());
    fixture.coordinator.community_rpc_bases = Arc::new(duplicate);
    assert!(
        attempt_verified_community_reward(&fixture.coordinator, job.job_hash)
            .await
            .unwrap_err()
            .contains("4 distinct active")
    );
    assert_eq!(fixture.approval_request_count(), 0);
    // A fourth remote, with a delayed but successful probe, starts the round.
    let mut remote_info: Value = fixture
        .coordinator
        .inference_http
        .get(format!("{}/network/info", original[3]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    remote_info["community_approval_output_tokens"] = Value::Bool(false);
    let delayed = tf_wire_server(
        TfApprovalWire {
            info: remote_info,
            received: received.clone(),
            reject_tokens: false,
            delay_ms: 150,
        },
        &mut servers,
    )
    .await;
    let mut reachable = fixture.coordinator.community_rpc_bases.as_ref().clone();
    reachable.push(delayed);
    fixture.coordinator.community_rpc_bases = Arc::new(reachable);
    fixture.open_approvals();
    let _ = attempt_verified_community_reward(&fixture.coordinator, job.job_hash).await;
    assert!(fixture.approval_request_count() >= 3);
    assert_eq!(received.lock().unwrap().len(), 1);
    assert!(
        received.lock().unwrap()[0]["payload"]
            .get("output_tokens")
            .is_none()
    ); // old capability
    servers.shutdown().await;
}

#[tokio::test]
async fn teacher_forced_released_settlement_retains_tokens_and_bad_approval_hints_fall_back() {
    let fixture = ReleaseOnVerificationFixture::build(true).await;
    let job = fixture.start_job().await;
    job.submit.await.unwrap().unwrap();
    job.run.await.unwrap().unwrap();
    fixture.coordinator.runtime_tasks.lock().abort_all();
    let record = fixture
        .coordinator
        .community_verified_settlements
        .get(&job.job_hash)
        .unwrap()
        .clone();
    assert!(
        record
            .output_tokens
            .as_ref()
            .is_some_and(|tokens| !tokens.is_empty())
    );
    assert!(record.payload.output_tokens.is_none());
    let mut bad = record.payload.clone();
    bad.output_tokens = Some(vec![u32::MAX]);
    let (item, result) = validate_reward_approval_payload(&fixture.coordinator, &bad).unwrap();
    verify_community_result_with_quorum(&fixture.coordinator, &item, &result)
        .await
        .unwrap();
    let mut servers = tokio::task::JoinSet::new();
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let keys: Vec<_> = fixture
        .coordinator
        .state
        .active_validators()
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| *key != fixture.coordinator.validator_address)
        .collect();
    let mut origins = Vec::new();
    for key in keys.into_iter().take(4) {
        origins.push(tf_wire_server(TfApprovalWire { info: json!({"validator_address": key.to_hex(), "community_approval_output_tokens": true, "transaction_domain": fixture.transaction_domain.to_hex()}), received: received.clone(), reject_tokens: false, delay_ms: 0 }, &mut servers).await);
    }
    let mut coordinator = fixture.coordinator.clone();
    coordinator.community_rpc_bases = Arc::new(origins);
    let _ = attempt_verified_community_reward(&coordinator, job.job_hash).await;
    {
        let requests = received.lock().unwrap();
        assert_eq!(requests.len(), 4);
        for request in requests.iter() {
            assert_eq!(
                request["payload"]["output_tokens"],
                serde_json::to_value(&record.output_tokens).unwrap()
            );
        }
    }
    servers.shutdown().await;
}

#[tokio::test]
async fn teacher_forced_declined_result_cannot_carry_tokens() {
    let node = fake_node_with_workers(Vec::new());
    let result = WorkResult {
        job_id: "declined".into(),
        worker_id: "worker".into(),
        success: false,
        declined: true,
        output: String::new(),
        output_hash: String::new(),
        tokens_generated: 0,
        total_ms: 0,
        ms_per_token: 0,
        engine: String::new(),
        error: None,
        signed_attestation_hex: None,
        output_tokens: Some(Vec::new()),
    };
    let (status, reason) = community_submit_work(AxumState(node), Json(result))
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(reason.contains("canonical no-compute"));
}
