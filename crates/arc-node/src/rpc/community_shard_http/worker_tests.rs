use crate::community_shard_worker::{
    CommunityShardWorkerConfig, CommunityShardWorkerError, PartialModelLoader,
    run_community_shard_worker,
};
use crate::community_sharding::tests::{
    build_deterministic_integer_model, build_partial_integer_model, model_spec_from,
};
use crate::community_sharding::{
    COMMUNITY_SHARD_PROTOCOL_VERSION, CommunityShardCoordinator, CommunityShardLimits, LeaseId,
    SessionAbortReason, SessionId, SessionOutcome, ShardClaimStepRequest, ShardClaimStepResponse,
    ShardHeartbeatRequest, ShardHeartbeatResponse, ShardLayerRange, ShardLeaseAssignment,
    ShardLeaseRequest, ShardLeaseResponse, ShardStepSubmitRequest, ShardStepSubmitResponse,
    TrustedModelSpec,
};
use arc_crypto::{Hash256, KeyPair};
use arc_inference::cached_integer_model::{CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel};
use axum::extract::{DefaultBodyLimit, State as AxumState};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, watch};

fn now_ms() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).expect("unix ms")
}

fn build_worker_test_node(
    model: Option<Arc<CachedIntegerModel>>,
    model_id: Option<Hash256>,
) -> super::super::NodeState {
    let coord_kp = Arc::new(KeyPair::generate_ed25519());
    let coord_addr = coord_kp.address();
    super::super::build_node_state(
        Arc::new(arc_state::StateDB::new()),
        Arc::new(arc_mempool::Mempool::new(1_000)),
        coord_addr,
        Some(coord_kp),
        1_000,
        Instant::now(),
        Arc::new(AtomicU32::new(0)),
        model,
        None,
        None,
        model_id,
    )
}

fn build_production_router(node: super::super::NodeState) -> Router {
    super::routes()
        .route(
            super::super::COMMUNITY_REGISTER_PATH,
            post(super::super::community_register_signed),
        )
        .with_state(node)
}

async fn spawn_router(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), handle)
}

fn worker_config(
    base_url: &str,
    node: &super::super::NodeState,
    name: &str,
) -> CommunityShardWorkerConfig {
    let mut cfg = CommunityShardWorkerConfig::new(
        base_url,
        node.validator_address,
        node.state.transaction_domain_hash(),
        2,
        name,
        "arc-4L-16d-2h-32v",
        "linux-x86_64",
    );
    cfg.allow_insecure_for_loopback = true;
    cfg.heartbeat_interval_ms = 50;
    cfg.idle_poll_interval_ms = 15;
    cfg.retry_base_backoff_ms = 20;
    cfg.retry_max_backoff_ms = 100;
    cfg.reacquire_base_backoff_ms = 25;
    cfg.reacquire_max_backoff_ms = 150;
    cfg
}

fn make_tiny_model_and_spec(tag: &[u8]) -> (Arc<CachedIntegerModel>, TrustedModelSpec) {
    let full = Arc::new(build_deterministic_integer_model(
        32,
        16,
        2,
        32,
        4,
        64,
        vec![128_001, 128_009],
    ));
    let model_id = arc_crypto::hash_bytes(tag);
    let spec = model_spec_from(model_id, &full);
    (full, spec)
}

fn partial_loader(full: Arc<CachedIntegerModel>) -> PartialModelLoader {
    Arc::new(move |assignment| {
        Ok(Arc::new(build_partial_integer_model(
            &full,
            assignment.range,
        )))
    })
}

async fn wait_for_condition<F>(node: &super::super::NodeState, timeout: Duration, mut pred: F)
where
    F: FnMut(&CommunityShardCoordinator) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let notified = node.community_shard_notify.notified();
        {
            let guard = node.community_shard_coordinator.lock();
            if let Some(coord) = guard.as_ref() {
                if pred(coord) {
                    return;
                }
            }
        }
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for coordinator condition"
        );
        let remaining = (deadline - now).min(Duration::from_millis(50));
        let _ = tokio::time::timeout(remaining, notified).await;
    }
}

async fn wait_for_session_result(
    node: &super::super::NodeState,
    session_id: SessionId,
    timeout: Duration,
) -> SessionOutcome {
    let deadline = Instant::now() + timeout;
    loop {
        let notified = node.community_shard_notify.notified();
        {
            let mut guard = node.community_shard_coordinator.lock();
            if let Some(coord) = guard.as_mut() {
                if let Some(outcome) = coord.take_result(session_id) {
                    return outcome;
                }
            }
        }
        let now = Instant::now();
        if now >= deadline {
            let guard = node.community_shard_coordinator.lock();
            let coordinator = guard.as_ref().unwrap();
            let session = coordinator.session_status(session_id);
            let worker = session
                .as_ref()
                .and_then(|session| coordinator.worker_lease_snapshot(session.expected_worker));
            panic!(
                "timed out waiting for session {session_id:?}: session={session:?}, expected_worker={worker:?}, coordinator={:?}",
                coordinator.status_snapshot(now_ms())
            );
        }
        let remaining = (deadline - now).min(Duration::from_millis(50));
        let _ = tokio::time::timeout(remaining, notified).await;
    }
}

#[tokio::test]
async fn worker_rejects_redirect_auth_bad_lease_ttl_and_oversized_body_before_loader() {
    let (full, spec) = make_tiny_model_and_spec(b"worker-reject-before-loader");
    let node = build_worker_test_node(Some(Arc::clone(&full)), Some(spec.model_id));
    super::init_community_shard_coordinator_with_limits(
        &node,
        CommunityShardLimits {
            target_partitions: 2,
            ..CommunityShardLimits::default()
        },
    )
    .expect("init coordinator");

    let loader_calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = Arc::clone(&loader_calls);
    let loader: PartialModelLoader = Arc::new(move |_| {
        calls_ref.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("loader must never run on rejected responses")
    });
    let (_tx, rx) = watch::channel(false);

    async fn redirect_handler() -> impl IntoResponse {
        (
            StatusCode::FOUND,
            [(
                header::LOCATION,
                HeaderValue::from_static("https://127.0.0.1/redirected"),
            )],
        )
    }

    let (redirect_base, redirect_srv) = spawn_router(Router::new().route(
        super::super::COMMUNITY_REGISTER_PATH,
        post(redirect_handler),
    ))
    .await;
    let err = run_community_shard_worker(
        worker_config(&redirect_base, &node, "w-redir"),
        Arc::new(KeyPair::generate_ed25519()),
        spec.clone(),
        Arc::clone(&loader),
        rx.clone(),
    )
    .await
    .unwrap_err();
    redirect_srv.abort();
    assert!(matches!(
        err,
        CommunityShardWorkerError::RedirectRejected { status: 302, .. }
    ));

    let (prod_base, prod_srv) = spawn_router(build_production_router(node.clone())).await;
    let mut wrong_coord_cfg = worker_config(&prod_base, &node, "w-wrong-coord");
    wrong_coord_cfg.coordinator_id = arc_crypto::hash_bytes(b"different-coordinator");
    let err = run_community_shard_worker(
        wrong_coord_cfg,
        Arc::new(KeyPair::generate_ed25519()),
        spec.clone(),
        Arc::clone(&loader),
        rx.clone(),
    )
    .await
    .unwrap_err();
    prod_srv.abort();
    assert!(matches!(
        err,
        CommunityShardWorkerError::AuthenticationOrAuthorization { status: 403, .. }
    ));

    struct BadLeaseState {
        response: ShardLeaseResponse,
    }
    async fn bad_lease_handler(
        AxumState(state): AxumState<Arc<BadLeaseState>>,
        Json(_signed): Json<
            super::super::CommunitySignedRequest<
                super::SignedShardWorkerPayload<ShardLeaseRequest>,
            >,
        >,
    ) -> Json<ShardLeaseResponse> {
        Json(state.response.clone())
    }

    let base_assignment = ShardLeaseAssignment {
        lease_id: LeaseId {
            boot_nonce: arc_crypto::hash_bytes(b"bad-lease-boot"),
            seq: 1,
        },
        lease_epoch: 1,
        model_id: spec.model_id,
        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
        range: ShardLayerRange {
            start_layer: 0,
            end_layer: 2,
        },
        provisioning_deadline_unix_ms: now_ms() + 60_000,
    };
    let cases = [
        ShardLeaseResponse {
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            assignment: ShardLeaseAssignment {
                range: ShardLayerRange {
                    start_layer: 0,
                    end_layer: 4,
                },
                ..base_assignment.clone()
            },
            ready_ttl_ms: 45_000,
        },
        ShardLeaseResponse {
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            assignment: ShardLeaseAssignment {
                range: ShardLayerRange {
                    start_layer: 0,
                    end_layer: 3,
                },
                ..base_assignment.clone()
            },
            ready_ttl_ms: 45_000,
        },
        ShardLeaseResponse {
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            assignment: base_assignment.clone(),
            ready_ttl_ms: 0,
        },
        ShardLeaseResponse {
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            assignment: base_assignment.clone(),
            ready_ttl_ms: 50,
        },
        ShardLeaseResponse {
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            assignment: base_assignment,
            ready_ttl_ms: 10_000,
        },
    ];
    for bad_resp in cases {
        let reg_router = Router::new()
            .route(
                super::super::COMMUNITY_REGISTER_PATH,
                post(super::super::community_register_signed),
            )
            .with_state(node.clone());
        let lease_router = Router::new()
            .route(super::COMMUNITY_SHARD_LEASE_PATH, post(bad_lease_handler))
            .with_state(Arc::new(BadLeaseState { response: bad_resp }));
        let (base, srv) = spawn_router(reg_router.merge(lease_router)).await;
        let err = run_community_shard_worker(
            worker_config(&base, &node, "w-bad-lease"),
            Arc::new(KeyPair::generate_ed25519()),
            spec.clone(),
            Arc::clone(&loader),
            rx.clone(),
        )
        .await
        .unwrap_err();
        srv.abort();
        assert!(matches!(err, CommunityShardWorkerError::InvalidLease(_)));
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind oversized listener");
    let oversized_addr = listener.local_addr().expect("oversized addr");
    let oversized_srv = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut req_buf = [0u8; 4096];
            let _ = stream.read(&mut req_buf).await;
            let header = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n";
            if stream.write_all(header).await.is_ok() {
                let chunk = vec![b'a'; 64 * 1024];
                let chunk_prefix = format!("{:x}\r\n", chunk.len());
                for _ in 0..36 {
                    if stream.write_all(chunk_prefix.as_bytes()).await.is_err()
                        || stream.write_all(&chunk).await.is_err()
                        || stream.write_all(b"\r\n").await.is_err()
                    {
                        break;
                    }
                }
                let _ = stream.write_all(b"0\r\n\r\n").await;
            }
        }
    });
    let oversized_base = format!("http://{oversized_addr}");
    let err = run_community_shard_worker(
        worker_config(&oversized_base, &node, "w-oversized"),
        Arc::new(KeyPair::generate_ed25519()),
        spec,
        loader,
        rx,
    )
    .await
    .unwrap_err();
    oversized_srv.abort();
    assert!(matches!(
        err,
        CommunityShardWorkerError::ResponseTooLarge { .. }
    ));
    assert_eq!(loader_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn two_partial_workers_execute_real_networked_pipeline_matching_try_generate() {
    let (full, spec) = make_tiny_model_and_spec(b"worker-two-tile-pipeline");
    let node = build_worker_test_node(Some(Arc::clone(&full)), Some(spec.model_id));
    super::init_community_shard_coordinator_with_limits(
        &node,
        CommunityShardLimits {
            target_partitions: 2,
            ..CommunityShardLimits::default()
        },
    )
    .expect("init coordinator");

    let (base_url, srv) = spawn_router(build_production_router(node.clone())).await;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let kp1 = Arc::new(KeyPair::generate_ed25519());
    let kp2 = Arc::new(KeyPair::generate_ed25519());
    let w1 = tokio::spawn(run_community_shard_worker(
        worker_config(&base_url, &node, "shard-w1"),
        Arc::clone(&kp1),
        spec.clone(),
        partial_loader(Arc::clone(&full)),
        shutdown_rx.clone(),
    ));
    let w2 = tokio::spawn(run_community_shard_worker(
        worker_config(&base_url, &node, "shard-w2"),
        Arc::clone(&kp2),
        spec,
        partial_loader(Arc::clone(&full)),
        shutdown_rx,
    ));

    wait_for_condition(&node, Duration::from_secs(10), |coord| {
        coord.status_snapshot(now_ms()).fully_covered_ready
    })
    .await;

    let prompt = vec![1u32, 5, 9, 13];
    let max_tokens = 4u32;
    let session_id = node
        .community_shard_coordinator
        .lock()
        .as_mut()
        .expect("coord")
        .open_session(now_ms(), prompt.clone(), max_tokens)
        .expect("open session");

    let outcome = wait_for_session_result(&node, session_id, Duration::from_secs(10)).await;
    let pending = match outcome {
        SessionOutcome::PendingVerification(pending) => pending,
        other => panic!("expected pending verification, got {other:?}"),
    };
    assert!(pending.verification_required);
    assert!(!pending.reward_eligible);
    let (expected_tokens, _) = full
        .try_generate(&prompt, max_tokens, &full.config.eos_tokens)
        .expect("full try_generate");
    assert_eq!(pending.unverified_generated_tokens, expected_tokens);

    let _ = shutdown_tx.send(true);
    let s1 = w1.await.expect("join w1").expect("w1 ok");
    let s2 = w2.await.expect("join w2").expect("w2 ok");
    srv.abort();

    assert_eq!(s1.registrations, 1);
    assert_eq!(s2.registrations, 1);
    let expected_steps = 1 + prompt.len() as u64 + u64::from(max_tokens);
    for summary in [&s1, &s2] {
        assert_eq!(summary.steps_executed, expected_steps);
        assert!(summary.steps_submitted <= summary.steps_executed);
        assert!(summary.steps_executed - summary.steps_submitted <= 1);
    }
    assert!(s1.clean_release_sent);
    assert!(s2.clean_release_sent);
}

#[tokio::test]
async fn transient_submit_response_loss_retries_without_recompute_or_false_success() {
    let (full, spec) = make_tiny_model_and_spec(b"worker-lost-submit-ack");
    let node = build_worker_test_node(Some(Arc::clone(&full)), Some(spec.model_id));
    super::init_community_shard_coordinator_with_limits(
        &node,
        CommunityShardLimits {
            target_partitions: 2,
            ..CommunityShardLimits::default()
        },
    )
    .expect("init coordinator");

    let kp1 = Arc::new(KeyPair::generate_ed25519());
    let kp2 = Arc::new(KeyPair::generate_ed25519());

    struct SubmitLossState {
        node: super::super::NodeState,
        target_worker: Hash256,
        dropped_once: AtomicBool,
        observed: Mutex<Vec<(Hash256, u64, Hash256)>>,
    }

    async fn lossy_submit_handler(
        AxumState(state): AxumState<Arc<SubmitLossState>>,
        Json(signed): Json<
            super::super::CommunitySignedRequest<
                super::SignedShardWorkerPayload<ShardStepSubmitRequest>,
            >,
        >,
    ) -> Result<Json<ShardStepSubmitResponse>, (StatusCode, String)> {
        let worker_id = signed.payload.request.worker_id;
        let nonce = signed.nonce;
        let step_seq = signed.payload.request.step_id.seq;
        let payload_hash = signed.payload_hash;
        if worker_id == state.target_worker {
            state.observed.lock().push((nonce, step_seq, payload_hash));
        }
        let result =
            super::community_shard_submit_step_signed(AxumState(state.node.clone()), Json(signed))
                .await;
        if worker_id == state.target_worker
            && result.is_ok()
            && !state.dropped_once.swap(true, Ordering::SeqCst)
        {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "simulated dropped submit response".to_string(),
            ));
        }
        result
    }

    let loss_state = Arc::new(SubmitLossState {
        node: node.clone(),
        target_worker: kp1.address(),
        dropped_once: AtomicBool::new(false),
        observed: Mutex::new(Vec::new()),
    });

    let base_routes = Router::new()
        .route(
            super::super::COMMUNITY_REGISTER_PATH,
            post(super::super::community_register_signed),
        )
        .route(
            super::COMMUNITY_SHARD_LEASE_PATH,
            post(super::community_shard_lease_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_HEARTBEAT_PATH,
            post(super::community_shard_heartbeat_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_CLAIM_STEP_PATH,
            post(super::community_shard_claim_step_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_RELEASE_PATH,
            post(super::community_shard_release_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_STATUS_PATH,
            get(super::community_shard_status),
        )
        .with_state(node.clone());

    let submit_route = Router::new()
        .route(
            super::COMMUNITY_SHARD_SUBMIT_STEP_PATH,
            post(lossy_submit_handler).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_SUBMIT_BODY_LIMIT_BYTES,
            )),
        )
        .with_state(Arc::clone(&loss_state));

    let (base_url, srv) = spawn_router(base_routes.merge(submit_route)).await;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let w1 = tokio::spawn(run_community_shard_worker(
        worker_config(&base_url, &node, "lossy-w1"),
        Arc::clone(&kp1),
        spec.clone(),
        partial_loader(Arc::clone(&full)),
        shutdown_rx.clone(),
    ));
    let w2 = tokio::spawn(run_community_shard_worker(
        worker_config(&base_url, &node, "lossy-w2"),
        Arc::clone(&kp2),
        spec,
        partial_loader(Arc::clone(&full)),
        shutdown_rx,
    ));

    wait_for_condition(&node, Duration::from_secs(10), |coord| {
        coord.status_snapshot(now_ms()).fully_covered_ready
    })
    .await;

    let prompt = vec![2u32, 4, 6];
    let max_tokens = 3u32;
    let session_id = node
        .community_shard_coordinator
        .lock()
        .as_mut()
        .expect("coord")
        .open_session(now_ms(), prompt.clone(), max_tokens)
        .expect("open session");

    let outcome = wait_for_session_result(&node, session_id, Duration::from_secs(10)).await;
    let pending = match outcome {
        SessionOutcome::PendingVerification(pending) => pending,
        other => panic!("expected pending verification, got {other:?}"),
    };
    let (expected_tokens, _) = full
        .try_generate(&prompt, max_tokens, &full.config.eos_tokens)
        .expect("full try_generate");
    assert_eq!(pending.unverified_generated_tokens, expected_tokens);

    let _ = shutdown_tx.send(true);
    let s1 = w1.await.expect("join w1").expect("w1 ok");
    let _s2 = w2.await.expect("join w2").expect("w2 ok");
    srv.abort();

    let observed = loss_state.observed.lock().clone();
    assert!(observed.len() >= 2);
    assert_eq!(observed[0].1, observed[1].1);
    assert_eq!(observed[0].2, observed[1].2);
    assert_ne!(observed[0].0, observed[1].0);
    assert!(s1.submit_retries >= 1);
    assert_eq!(s1.submit_stale_conflicts, 1);
    assert_eq!(s1.steps_submitted + 1, s1.steps_executed);
}

#[tokio::test]
async fn heartbeat_runs_during_blocked_load_and_active_state_and_shutdown_drains() {
    let (full, spec) = make_tiny_model_and_spec(b"worker-heartbeat-drain");
    let node = build_worker_test_node(Some(Arc::clone(&full)), Some(spec.model_id));
    super::init_community_shard_coordinator_with_limits(
        &node,
        CommunityShardLimits {
            target_partitions: 2,
            ..CommunityShardLimits::default()
        },
    )
    .expect("init coordinator");

    struct HeartbeatObserver {
        node: super::super::NodeState,
        provisioning_count: AtomicUsize,
        ready_count: AtomicUsize,
        notify: Notify,
    }

    async fn observing_heartbeat_handler(
        AxumState(state): AxumState<Arc<HeartbeatObserver>>,
        Json(signed): Json<
            super::super::CommunitySignedRequest<
                super::SignedShardWorkerPayload<ShardHeartbeatRequest>,
            >,
        >,
    ) -> Result<Json<ShardHeartbeatResponse>, (StatusCode, String)> {
        let is_ready = signed.payload.request.ready;
        let res =
            super::community_shard_heartbeat_signed(AxumState(state.node.clone()), Json(signed))
                .await;
        if res.is_ok() {
            if is_ready {
                state.ready_count.fetch_add(1, Ordering::SeqCst);
            } else {
                state.provisioning_count.fetch_add(1, Ordering::SeqCst);
            }
            state.notify.notify_waiters();
        }
        res
    }

    let observer = Arc::new(HeartbeatObserver {
        node: node.clone(),
        provisioning_count: AtomicUsize::new(0),
        ready_count: AtomicUsize::new(0),
        notify: Notify::new(),
    });

    let base_routes = Router::new()
        .route(
            super::super::COMMUNITY_REGISTER_PATH,
            post(super::super::community_register_signed),
        )
        .route(
            super::COMMUNITY_SHARD_LEASE_PATH,
            post(super::community_shard_lease_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_CLAIM_STEP_PATH,
            post(super::community_shard_claim_step_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_SUBMIT_STEP_PATH,
            post(super::community_shard_submit_step_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_SUBMIT_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_RELEASE_PATH,
            post(super::community_shard_release_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_STATUS_PATH,
            get(super::community_shard_status),
        )
        .with_state(node.clone());

    let hb_route = Router::new()
        .route(
            super::COMMUNITY_SHARD_HEARTBEAT_PATH,
            post(observing_heartbeat_handler).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .with_state(Arc::clone(&observer));

    let (base_url, srv) = spawn_router(base_routes.merge(hb_route)).await;

    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let gate_rx = Arc::new(Mutex::new(Some(gate_rx)));
    let loaded_weak: Arc<Mutex<Option<Weak<CachedIntegerModel>>>> = Arc::new(Mutex::new(None));
    let full_for_blocked = Arc::clone(&full);
    let weak_for_blocked = Arc::clone(&loaded_weak);
    let blocked_loader: PartialModelLoader = Arc::new(move |assignment| {
        let rx = gate_rx.lock().take().expect("blocked loader invoked once");
        let _ = rx.recv_timeout(Duration::from_secs(10));
        let partial = Arc::new(build_partial_integer_model(
            &full_for_blocked,
            assignment.range,
        ));
        *weak_for_blocked.lock() = Some(Arc::downgrade(&partial));
        Ok(partial)
    });

    let (shut1_tx, shut1_rx) = watch::channel(false);
    let mut cfg1 = worker_config(&base_url, &node, "blocked-loader-w1");
    cfg1.heartbeat_interval_ms = 35;
    let w1 = tokio::spawn(run_community_shard_worker(
        cfg1,
        Arc::new(KeyPair::generate_ed25519()),
        spec.clone(),
        blocked_loader,
        shut1_rx,
    ));

    let deadline = Instant::now() + Duration::from_secs(10);
    while observer.provisioning_count.load(Ordering::SeqCst) < 2 {
        let notified = observer.notify.notified();
        if observer.provisioning_count.load(Ordering::SeqCst) >= 2 {
            break;
        }
        assert!(Instant::now() < deadline, "expected ready=false heartbeats");
        let _ = tokio::time::timeout(Duration::from_millis(50), notified).await;
    }

    let _ = shut1_tx.send(true);
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(!w1.is_finished());

    let _ = gate_tx.send(());
    let summary1 = w1.await.expect("join w1").expect("w1 clean shutdown");
    assert!(observer.provisioning_count.load(Ordering::SeqCst) >= 2);
    assert!(summary1.heartbeats_sent >= 1);
    assert_eq!(summary1.models_loaded, 0);
    assert!(summary1.clean_release_sent);
    assert!(
        loaded_weak
            .lock()
            .as_ref()
            .expect("weak captured")
            .upgrade()
            .is_none()
    );

    let (shut2_tx, shut2_rx) = watch::channel(false);
    let mut cfg2 = worker_config(&base_url, &node, "active-hb-w2");
    cfg2.heartbeat_interval_ms = 35;
    let w2 = tokio::spawn(run_community_shard_worker(
        cfg2,
        Arc::new(KeyPair::generate_ed25519()),
        spec,
        partial_loader(full),
        shut2_rx,
    ));

    let deadline2 = Instant::now() + Duration::from_secs(10);
    while observer.ready_count.load(Ordering::SeqCst) < 3 {
        let notified = observer.notify.notified();
        if observer.ready_count.load(Ordering::SeqCst) >= 3 {
            break;
        }
        assert!(Instant::now() < deadline2, "expected ready=true heartbeats");
        let _ = tokio::time::timeout(Duration::from_millis(50), notified).await;
    }

    let _ = shut2_tx.send(true);
    let summary2 = w2.await.expect("join w2").expect("w2 clean shutdown");
    srv.abort();
    assert!(observer.ready_count.load(Ordering::SeqCst) >= 3);
    assert!(summary2.heartbeats_sent >= 2);
    assert!(summary2.clean_release_sent);
}

#[tokio::test]
async fn rebalance_and_coordinator_restart_drop_old_model_before_reload_and_fence() {
    let (full, spec) = make_tiny_model_and_spec(b"worker-rebalance-and-fence");
    let node = build_worker_test_node(Some(Arc::clone(&full)), Some(spec.model_id));
    let limits = CommunityShardLimits {
        target_partitions: 2,
        ready_ttl_ms: 400,
        provisioning_ttl_ms: 5_000,
        ..CommunityShardLimits::default()
    };
    super::init_community_shard_coordinator_with_limits(&node, limits.clone())
        .expect("init coordinator");

    let (base_url, srv) = spawn_router(build_production_router(node.clone())).await;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let drop_verified_count = Arc::new(AtomicUsize::new(0));
    let make_drop_checking_loader =
        |full: Arc<CachedIntegerModel>, verified: Arc<AtomicUsize>| -> PartialModelLoader {
            let prior_weaks: Arc<Mutex<Vec<Weak<CachedIntegerModel>>>> =
                Arc::new(Mutex::new(Vec::new()));
            Arc::new(move |assignment| {
                let mut guard = prior_weaks.lock();
                if !guard.is_empty() {
                    for prev in guard.iter() {
                        assert!(
                            prev.upgrade().is_none(),
                            "previous partial model must be dropped before loading next range"
                        );
                    }
                    verified.fetch_add(1, Ordering::SeqCst);
                }
                let model = Arc::new(build_partial_integer_model(&full, assignment.range));
                guard.push(Arc::downgrade(&model));
                Ok(model)
            })
        };

    let mut cfg1 = worker_config(&base_url, &node, "rebal-w1");
    cfg1.connect_timeout_ms = 100;
    cfg1.request_timeout_ms = 200;
    cfg1.release_timeout_ms = 200;
    let w1 = tokio::spawn(run_community_shard_worker(
        cfg1,
        Arc::new(KeyPair::generate_ed25519()),
        spec.clone(),
        make_drop_checking_loader(Arc::clone(&full), Arc::clone(&drop_verified_count)),
        shutdown_rx.clone(),
    ));

    wait_for_condition(&node, Duration::from_secs(10), |coord| {
        coord.status_snapshot(now_ms()).ready_workers == 1
    })
    .await;

    let dummy_kp = KeyPair::generate_ed25519();
    let reg_req = super::super::CommunityRegisterRequest {
        worker_id: format!("0x{}", dummy_kp.address().to_hex()),
        name: "silent-dummy-w2".to_string(),
        capabilities: vec![super::COMMUNITY_SHARD_CAPABILITY.to_string()],
        model: Some("arc-4L-16d-2h-32v".to_string()),
        model_id: Some(format!("0x{}", spec.model_id.to_hex())),
        execution_profile: Some(CANONICAL_REWARD_INFERENCE_PROFILE.to_string()),
        platform: "linux-x86_64".to_string(),
    };
    let signed_reg = super::super::sign_community_request(
        super::super::COMMUNITY_REGISTER_PATH,
        reg_req,
        &dummy_kp,
        node.validator_address,
        node.state.transaction_domain_hash(),
    )
    .expect("sign dummy reg");
    let registered =
        super::super::community_register_signed(AxumState(node.clone()), Json(signed_reg))
            .await
            .expect("register dummy");
    assert_eq!(registered.0["ok"], true);
    let signed_lease = super::sign_shard_worker_request(
        super::COMMUNITY_SHARD_LEASE_PATH,
        ShardLeaseRequest {
            worker_id: dummy_kp.address(),
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            n_layers: spec.n_layers,
            d_model: spec.d_model,
            vocab_size: spec.vocab_size,
            max_seq: spec.max_seq,
            max_resident_layers: 2,
        },
        &dummy_kp,
        node.validator_address,
        node.state.transaction_domain_hash(),
    )
    .expect("sign dummy lease");
    let dummy_lease =
        super::community_shard_lease_signed(AxumState(node.clone()), Json(signed_lease))
            .await
            .expect("dummy lease")
            .0
            .assignment;
    let signed_hb = super::sign_shard_worker_request(
        super::COMMUNITY_SHARD_HEARTBEAT_PATH,
        ShardHeartbeatRequest {
            worker_id: dummy_kp.address(),
            lease_id: dummy_lease.lease_id,
            lease_epoch: dummy_lease.lease_epoch,
            model_id: dummy_lease.model_id,
            execution_profile: dummy_lease.execution_profile.clone(),
            loaded_range: dummy_lease.range,
            ready: true,
        },
        &dummy_kp,
        node.validator_address,
        node.state.transaction_domain_hash(),
    )
    .expect("sign dummy hb");
    let dummy_hb_resp =
        super::community_shard_heartbeat_signed(AxumState(node.clone()), Json(signed_hb))
            .await
            .expect("dummy hb")
            .0;
    let dummy_expires_at = match dummy_hb_resp.directive {
        crate::community_sharding::HeartbeatDirective::Active { expires_at_unix_ms } => {
            expires_at_unix_ms
        }
        other => panic!("expected active directive for dummy worker, got {other:?}"),
    };

    let mut cfg3 = worker_config(&base_url, &node, "rebal-w3");
    cfg3.connect_timeout_ms = 100;
    cfg3.request_timeout_ms = 200;
    cfg3.release_timeout_ms = 200;
    let w3 = tokio::spawn(run_community_shard_worker(
        cfg3,
        Arc::new(KeyPair::generate_ed25519()),
        spec.clone(),
        make_drop_checking_loader(Arc::clone(&full), Arc::clone(&drop_verified_count)),
        shutdown_rx,
    ));

    wait_for_condition(&node, Duration::from_secs(10), |coord| {
        coord.status_snapshot(now_ms()).ready_workers == 3
    })
    .await;

    while now_ms() <= dummy_expires_at + 30 {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    wait_for_condition(&node, Duration::from_secs(10), |coord| {
        let snap = coord.status_snapshot(now_ms());
        snap.fully_covered_ready
            && snap.ready_workers == 2
            && coord.worker_lease_snapshot(dummy_kp.address()).is_none()
            && drop_verified_count.load(Ordering::SeqCst) >= 1
    })
    .await;

    let new_boot = arc_crypto::hash_bytes(b"coordinator-restarted-boot-2");
    let restarted_coord =
        CommunityShardCoordinator::new(new_boot, spec.clone(), limits).expect("new coord");
    *node.community_shard_coordinator.lock() = Some(restarted_coord);

    wait_for_condition(&node, Duration::from_secs(10), |coord| {
        let snap = coord.status_snapshot(now_ms());
        snap.boot_nonce == new_boot && snap.fully_covered_ready && snap.ready_workers == 2
    })
    .await;
    assert!(drop_verified_count.load(Ordering::SeqCst) >= 3);

    let prompt = vec![3u32, 7, 11];
    let max_tokens = 2u32;
    let session_id = node
        .community_shard_coordinator
        .lock()
        .as_mut()
        .expect("coord")
        .open_session(now_ms(), prompt.clone(), max_tokens)
        .expect("open session after restart");
    let outcome = wait_for_session_result(&node, session_id, Duration::from_secs(10)).await;
    let pending = match outcome {
        SessionOutcome::PendingVerification(pending) => pending,
        other => panic!("expected pending verification after restart, got {other:?}"),
    };
    let (expected_tokens, _) = full
        .try_generate(&prompt, max_tokens, &full.config.eos_tokens)
        .expect("full try_generate");
    assert_eq!(pending.unverified_generated_tokens, expected_tokens);

    let _ = shutdown_tx.send(true);
    let s1 = w1.await.expect("join w1").expect("w1 ok");
    let s3 = w3.await.expect("join w3").expect("w3 ok");
    srv.abort();

    assert_eq!(s1.rebalances_completed + s3.rebalances_completed, 1);
    assert_eq!(s1.registrations, 1);
    assert_eq!(s3.registrations, 1);
}

#[tokio::test]
async fn cancellation_during_retry_backoff_prevents_stale_submit() {
    let (full, spec) = make_tiny_model_and_spec(b"worker-cancel-prevents-stale");
    let node = build_worker_test_node(Some(Arc::clone(&full)), Some(spec.model_id));
    super::init_community_shard_coordinator_with_limits(
        &node,
        CommunityShardLimits {
            target_partitions: 2,
            ..CommunityShardLimits::default()
        },
    )
    .expect("init coordinator");

    let kp1 = Arc::new(KeyPair::generate_ed25519());
    let kp2 = Arc::new(KeyPair::generate_ed25519());

    struct CancelOnSubmitState {
        node: super::super::NodeState,
        target_worker: Hash256,
        cancelled_session: Mutex<Option<SessionId>>,
        stale_submits_after_cancel: AtomicUsize,
    }

    async fn cancelling_submit_handler(
        AxumState(state): AxumState<Arc<CancelOnSubmitState>>,
        Json(signed): Json<
            super::super::CommunitySignedRequest<
                super::SignedShardWorkerPayload<ShardStepSubmitRequest>,
            >,
        >,
    ) -> Result<Json<ShardStepSubmitResponse>, (StatusCode, String)> {
        let worker_id = signed.payload.request.worker_id;
        let session_id = signed.payload.request.session_id;
        if worker_id == state.target_worker {
            let mut guard = state.cancelled_session.lock();
            if guard.is_none() {
                *guard = Some(session_id);
                drop(guard);
                state
                    .node
                    .community_shard_coordinator
                    .lock()
                    .as_mut()
                    .expect("coord")
                    .cancel_session(now_ms(), session_id, SessionAbortReason::CallerCancelled)
                    .expect("cancel session");
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "transient error while session cancelled".to_string(),
                ));
            } else if *guard == Some(session_id) {
                state
                    .stale_submits_after_cancel
                    .fetch_add(1, Ordering::SeqCst);
            }
        }
        super::community_shard_submit_step_signed(AxumState(state.node.clone()), Json(signed)).await
    }

    let cancel_state = Arc::new(CancelOnSubmitState {
        node: node.clone(),
        target_worker: kp1.address(),
        cancelled_session: Mutex::new(None),
        stale_submits_after_cancel: AtomicUsize::new(0),
    });

    let base_routes = Router::new()
        .route(
            super::super::COMMUNITY_REGISTER_PATH,
            post(super::super::community_register_signed),
        )
        .route(
            super::COMMUNITY_SHARD_LEASE_PATH,
            post(super::community_shard_lease_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_HEARTBEAT_PATH,
            post(super::community_shard_heartbeat_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_CLAIM_STEP_PATH,
            post(super::community_shard_claim_step_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_RELEASE_PATH,
            post(super::community_shard_release_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_STATUS_PATH,
            get(super::community_shard_status),
        )
        .with_state(node.clone());

    let submit_route = Router::new()
        .route(
            super::COMMUNITY_SHARD_SUBMIT_STEP_PATH,
            post(cancelling_submit_handler).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_SUBMIT_BODY_LIMIT_BYTES,
            )),
        )
        .with_state(Arc::clone(&cancel_state));

    let (base_url, srv) = spawn_router(base_routes.merge(submit_route)).await;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut cfg1 = worker_config(&base_url, &node, "cancel-w1");
    cfg1.heartbeat_interval_ms = 25;
    cfg1.retry_base_backoff_ms = 250;
    cfg1.retry_max_backoff_ms = 500;
    let cfg2 = worker_config(&base_url, &node, "cancel-w2");

    let w1 = tokio::spawn(run_community_shard_worker(
        cfg1,
        Arc::clone(&kp1),
        spec.clone(),
        partial_loader(Arc::clone(&full)),
        shutdown_rx.clone(),
    ));
    let w2 = tokio::spawn(run_community_shard_worker(
        cfg2,
        Arc::clone(&kp2),
        spec,
        partial_loader(Arc::clone(&full)),
        shutdown_rx,
    ));

    wait_for_condition(&node, Duration::from_secs(10), |coord| {
        coord.status_snapshot(now_ms()).fully_covered_ready
    })
    .await;

    let session_1 = node
        .community_shard_coordinator
        .lock()
        .as_mut()
        .expect("coord")
        .open_session(now_ms(), vec![1u32, 2, 3], 2)
        .expect("open session 1");

    let outcome_1 = wait_for_session_result(&node, session_1, Duration::from_secs(10)).await;
    assert!(matches!(outcome_1, SessionOutcome::Aborted(_)));

    let prompt_2 = vec![4u32, 8, 12];
    let max_tokens_2 = 2u32;
    let session_2 = node
        .community_shard_coordinator
        .lock()
        .as_mut()
        .expect("coord")
        .open_session(now_ms(), prompt_2.clone(), max_tokens_2)
        .expect("open session 2");

    let outcome_2 = wait_for_session_result(&node, session_2, Duration::from_secs(10)).await;
    let pending = match outcome_2 {
        SessionOutcome::PendingVerification(pending) => pending,
        other => panic!("expected session 2 pending verification, got {other:?}"),
    };
    let (expected_tokens, _) = full
        .try_generate(&prompt_2, max_tokens_2, &full.config.eos_tokens)
        .expect("full try_generate");
    assert_eq!(pending.unverified_generated_tokens, expected_tokens);

    let _ = shutdown_tx.send(true);
    let s1 = w1.await.expect("join w1").expect("w1 ok");
    let _s2 = w2.await.expect("join w2").expect("w2 ok");
    srv.abort();

    assert_eq!(
        cancel_state
            .stale_submits_after_cancel
            .load(Ordering::SeqCst),
        0
    );
    assert!(s1.cancellations_applied >= 1);
}

#[tokio::test]
async fn heartbeat_not_starved_by_instant_claim_responses_and_default_loopback_idles_cleanly() {
    let (full, spec) = make_tiny_model_and_spec(b"worker-hb-starvation-and-defaults");
    let node = build_worker_test_node(Some(Arc::clone(&full)), Some(spec.model_id));
    super::init_community_shard_coordinator_with_limits(
        &node,
        CommunityShardLimits {
            target_partitions: 2,
            ..CommunityShardLimits::default()
        },
    )
    .expect("init coordinator");

    struct StarvationState {
        node: super::super::NodeState,
        claim_calls: AtomicUsize,
        ready_heartbeats: AtomicUsize,
        notify: Notify,
    }

    async fn instant_claim_handler(
        AxumState(state): AxumState<Arc<StarvationState>>,
        Json(_signed): Json<
            super::super::CommunitySignedRequest<
                super::SignedShardWorkerPayload<ShardClaimStepRequest>,
            >,
        >,
    ) -> Json<ShardClaimStepResponse> {
        state.claim_calls.fetch_add(1, Ordering::SeqCst);
        Json(ShardClaimStepResponse::CancelSessions {
            cancellations: Vec::new(),
        })
    }

    async fn counting_hb_handler(
        AxumState(state): AxumState<Arc<StarvationState>>,
        Json(signed): Json<
            super::super::CommunitySignedRequest<
                super::SignedShardWorkerPayload<ShardHeartbeatRequest>,
            >,
        >,
    ) -> Result<Json<ShardHeartbeatResponse>, (StatusCode, String)> {
        let is_ready = signed.payload.request.ready;
        let res =
            super::community_shard_heartbeat_signed(AxumState(state.node.clone()), Json(signed))
                .await;
        if res.is_ok() && is_ready {
            state.ready_heartbeats.fetch_add(1, Ordering::SeqCst);
            state.notify.notify_waiters();
        }
        res
    }

    let st = Arc::new(StarvationState {
        node: node.clone(),
        claim_calls: AtomicUsize::new(0),
        ready_heartbeats: AtomicUsize::new(0),
        notify: Notify::new(),
    });

    let base_routes = Router::new()
        .route(
            super::super::COMMUNITY_REGISTER_PATH,
            post(super::super::community_register_signed),
        )
        .route(
            super::COMMUNITY_SHARD_LEASE_PATH,
            post(super::community_shard_lease_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_SUBMIT_STEP_PATH,
            post(super::community_shard_submit_step_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_SUBMIT_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_RELEASE_PATH,
            post(super::community_shard_release_signed).layer(DefaultBodyLimit::max(
                super::COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            super::COMMUNITY_SHARD_STATUS_PATH,
            get(super::community_shard_status),
        )
        .with_state(node.clone());

    let wrapped_routes = Router::new()
        .route(
            super::COMMUNITY_SHARD_CLAIM_STEP_PATH,
            post(instant_claim_handler),
        )
        .route(
            super::COMMUNITY_SHARD_HEARTBEAT_PATH,
            post(counting_hb_handler),
        )
        .with_state(Arc::clone(&st));

    let (base_url, srv) = spawn_router(base_routes.merge(wrapped_routes)).await;
    let (shut_tx, shut_rx) = watch::channel(false);
    let mut cfg = worker_config(&base_url, &node, "starve-check");
    cfg.heartbeat_interval_ms = 35;
    let worker = tokio::spawn(run_community_shard_worker(
        cfg,
        Arc::new(KeyPair::generate_ed25519()),
        spec.clone(),
        partial_loader(Arc::clone(&full)),
        shut_rx,
    ));

    let deadline = Instant::now() + Duration::from_secs(5);
    while st.ready_heartbeats.load(Ordering::SeqCst) < 4 {
        let notified = st.notify.notified();
        if st.ready_heartbeats.load(Ordering::SeqCst) >= 4 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "heartbeats starved under instant claim loop"
        );
        let _ = tokio::time::timeout(Duration::from_millis(50), notified).await;
    }
    let _ = shut_tx.send(true);
    let summary = worker.await.expect("join worker").expect("worker ok");
    srv.abort();
    assert!(st.claim_calls.load(Ordering::SeqCst) >= 10);
    assert!(st.ready_heartbeats.load(Ordering::SeqCst) >= 4);
    assert!(summary.heartbeats_sent >= 3);
    assert!(summary.clean_release_sent);

    let (prod_base, prod_srv) = spawn_router(build_production_router(node.clone())).await;
    let (def_tx, def_rx) = watch::channel(false);
    let mut default_cfg = CommunityShardWorkerConfig::new(
        &prod_base,
        node.validator_address,
        node.state.transaction_domain_hash(),
        2,
        "default-loopback-idle",
        "arc-4L-16d-2h-32v",
        "linux-x86_64",
    );
    default_cfg.allow_insecure_for_loopback = true;
    let def_worker = tokio::spawn(run_community_shard_worker(
        default_cfg,
        Arc::new(KeyPair::generate_ed25519()),
        spec,
        partial_loader(full),
        def_rx,
    ));
    wait_for_condition(&node, Duration::from_secs(5), |coord| {
        coord.status_snapshot(now_ms()).ready_workers == 1
    })
    .await;
    let _ = def_tx.send(true);
    let def_summary = def_worker.await.expect("join default").expect("default ok");
    prod_srv.abort();
    assert_eq!(def_summary.registrations, 1);
    assert!(def_summary.clean_release_sent);
}

#[tokio::test]
async fn stalled_http_response_body_hits_total_elapsed_timeout() {
    let (full, spec) = make_tiny_model_and_spec(b"worker-stalled-body-timeout");
    let node = build_worker_test_node(Some(Arc::clone(&full)), Some(spec.model_id));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalled listener");
    let addr = listener.local_addr().expect("stalled addr");
    let stalled_srv = tokio::spawn(async move {
        let mut held_streams = Vec::new();
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut req_buf = [0u8; 4096];
            let _ = stream.read(&mut req_buf).await;
            let header = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n";
            let _ = stream.write_all(header).await;
            let _ = stream.flush().await;
            held_streams.push(stream);
        }
    });

    let mut cfg = worker_config(&format!("http://{addr}"), &node, "stalled-body");
    cfg.connect_timeout_ms = 50;
    cfg.request_timeout_ms = 80;
    cfg.retry_base_backoff_ms = 10;
    cfg.retry_max_backoff_ms = 20;

    let (_tx, rx) = watch::channel(false);
    let started = Instant::now();
    let err = run_community_shard_worker(
        cfg,
        Arc::new(KeyPair::generate_ed25519()),
        spec,
        partial_loader(full),
        rx,
    )
    .await
    .unwrap_err();
    stalled_srv.abort();

    assert!(matches!(
        err,
        CommunityShardWorkerError::TransientBudgetExhausted { .. }
    ));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hundred_signed_network_workers_all_execute_canonical_partial_inference() {
    let full = Arc::new(build_deterministic_integer_model(
        32,
        16,
        2,
        32,
        12,
        64,
        vec![128_001, 128_009],
    ));
    let spec = model_spec_from(arc_crypto::hash_bytes(b"hundred-network-workers"), &full);
    let node = build_worker_test_node(Some(Arc::clone(&full)), Some(spec.model_id));
    super::init_community_shard_coordinator_with_limits(&node, CommunityShardLimits::default())
        .expect("coordinator");
    let (base_url, server) = spawn_router(build_production_router(node.clone())).await;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut workers = tokio::task::JoinSet::new();
    let mut identities = std::collections::BTreeSet::new();
    for index in 0..100 {
        let key = Arc::new(KeyPair::generate_ed25519());
        assert!(identities.insert(key.address().to_hex()));
        let mut config = worker_config(&base_url, &node, &format!("network-worker-{index}"));
        config.model_name = super::super::model_display_name(&full);
        config.idle_poll_interval_ms = 250;
        config.heartbeat_interval_ms = 5_000;
        workers.spawn(run_community_shard_worker(
            config,
            key,
            spec.clone(),
            partial_loader(Arc::clone(&full)),
            shutdown_rx.clone(),
        ));
    }
    wait_for_condition(&node, Duration::from_secs(30), |coordinator| {
        let status = coordinator.status_snapshot(now_ms());
        status.ready_workers == 100 && status.fully_covered_ready
    })
    .await;
    let prompt = vec![5, 9];
    let max_tokens = 2;
    let (canonical, _) = full
        .try_generate(&prompt, max_tokens, &full.config.eos_tokens)
        .unwrap();
    let mut participants = std::collections::BTreeSet::new();
    for _ in 0..5 {
        let sessions = {
            let mut guard = node.community_shard_coordinator.lock();
            let coordinator = guard.as_mut().unwrap();
            (0..4)
                .map(|_| {
                    coordinator
                        .open_session(now_ms(), prompt.clone(), max_tokens)
                        .unwrap()
                })
                .collect::<Vec<_>>()
        };
        for session in sessions {
            let outcome = tokio::select! {
                outcome = wait_for_session_result(&node, session, Duration::from_secs(40)) => outcome,
                exited = workers.join_next() => panic!("worker exited before completing session {session:?}: {exited:?}"),
            };
            let SessionOutcome::PendingVerification(result) = outcome else {
                panic!("expected completed partial pipeline: {outcome:?}");
            };
            assert_eq!(result.unverified_generated_tokens, canonical);
            assert!(result.verification_required);
            assert!(!result.reward_eligible);
            assert_eq!(result.participants.len(), 6);
            for participant in result.participants {
                assert_eq!(participant.range.len(), 2);
                participants.insert(participant.worker_id.to_hex());
            }
        }
    }
    assert_eq!(participants, identities);
    shutdown_tx.send(true).unwrap();
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let steps_per_session_per_worker = 1 + prompt.len() as u64 + u64::from(max_tokens);
    let mut total_steps_executed = 0u64;
    let mut finished = 0;
    while let Some(joined) = tokio::time::timeout_at(drain_deadline, workers.join_next())
        .await
        .expect("workers must drain and release")
    {
        let summary = joined.expect("worker join").expect("worker exit");
        assert_eq!(summary.registrations, 1);
        assert!(summary.steps_executed > 0);
        assert_eq!(summary.steps_executed % steps_per_session_per_worker, 0);
        assert!(summary.steps_submitted <= summary.steps_executed);
        assert!(summary.steps_executed - summary.steps_submitted <= 1);
        assert!(summary.clean_release_sent);
        total_steps_executed += summary.steps_executed;
        finished += 1;
    }
    server.abort();
    assert_eq!(finished, 100);
    assert_eq!(
        total_steps_executed,
        5 * 4 * 6 * steps_per_session_per_worker
    );
}
