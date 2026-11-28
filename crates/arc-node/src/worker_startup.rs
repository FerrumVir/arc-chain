use crate::rpc::{HealthResponse, RpcPeerAddr};
use anyhow::Context;
use arc_state::StateDB;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State as AxumState},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONNECTION, RETRY_AFTER},
    },
    routing::get,
};
use serde_json::{Value, json};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tower_http::cors::CorsLayer;

pub const STARTUP_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const STARTUP_FORCE_CLOSE_TIMEOUT: Duration = Duration::from_millis(250);
pub const STARTUP_RETRY_AFTER_SECS: &str = "5";
pub const WORKER_STARTUP_PHASE_LOADING_MODEL: &str = "loading_model";
const STARTUP_LOADING_BODY_LIMIT_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EarlyWorkerStartupRole {
    FullIntegerWorker,
    CommunityShardWorker,
}

impl EarlyWorkerStartupRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::FullIntegerWorker => "full_integer_worker",
            Self::CommunityShardWorker => "community_shard_worker",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerStartupPolicyInput<'a> {
    pub full_integer_worker: bool,
    pub community_shard_worker: bool,
    pub stake: u64,
    pub chain_participation_enabled: bool,
    pub is_shard_holder: bool,
    pub auto_shard_join: bool,
    pub community_shard_coordinator: bool,
    pub native_inference_runtime: bool,
    pub tokenizer_only: bool,
    pub enable_i16: bool,
    pub model_path: Option<&'a str>,
    pub tcp_rpc_listener: bool,
}

pub fn early_worker_startup_role(
    input: &WorkerStartupPolicyInput<'_>,
) -> Option<EarlyWorkerStartupRole> {
    if !input.tcp_rpc_listener
        || input.stake != 0
        || input.chain_participation_enabled
        || input.is_shard_holder
        || input.auto_shard_join
        || input.community_shard_coordinator
        || input.native_inference_runtime
        || input.tokenizer_only
        || input.enable_i16
    {
        return None;
    }
    let model_path = input.model_path?;
    if model_path.ends_with(".arc-int8") {
        return None;
    }
    match (input.full_integer_worker, input.community_shard_worker) {
        (true, false) => Some(EarlyWorkerStartupRole::FullIntegerWorker),
        (false, true) => Some(EarlyWorkerStartupRole::CommunityShardWorker),
        _ => None,
    }
}

#[derive(Clone)]
pub struct StartupLoadingState {
    pub role: EarlyWorkerStartupRole,
    pub state: Arc<StateDB>,
    pub boot_time: Instant,
    pub transport_wire_policy: Arc<arc_net::transport::TransportWirePolicy>,
}

fn close_connection_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONNECTION, HeaderValue::from_static("close"));
    headers
}

fn loading_unavailable_headers() -> HeaderMap {
    let mut headers = close_connection_headers();
    headers.insert(
        RETRY_AFTER,
        HeaderValue::from_static(STARTUP_RETRY_AFTER_SECS),
    );
    headers
}

async fn startup_health(
    AxumState(loading): AxumState<StartupLoadingState>,
) -> (StatusCode, HeaderMap, Json<HealthResponse>) {
    let body = HealthResponse {
        status: "loading".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        height: loading.state.height(),
        peers: 0,
        chain_participation_enabled: false,
        legacy_v3_wire: loading.transport_wire_policy.legacy_v3(),
        extended_consensus_wire_enabled: !loading.transport_wire_policy.legacy_v3(),
        wire_messages_suppressed: loading.transport_wire_policy.suppressed_messages(),
        uptime_secs: loading.boot_time.elapsed().as_secs(),
        dag_round: 0,
        dag_committed: 0,
        validators: 0,
        last_block_age_secs: None,
        chain_advancing: None,
        dag_bootstrapping: false,
        degraded_reason: Some(format!(
            "worker ({}) is loading its canonical model; inference, readiness, and work claims remain unavailable until startup completes",
            loading.role.label()
        )),
        startup_phase: Some(WORKER_STARTUP_PHASE_LOADING_MODEL),
        binary_sha256: crate::build_identity::executable_sha256(),
        features: crate::build_identity::features(),
    };
    (StatusCode::OK, close_connection_headers(), Json(body))
}

async fn startup_inference_readiness() -> (StatusCode, HeaderMap, Json<Value>) {
    let body = json!({
        "schema": "arc.inference.readiness.v1",
        "safe_to_dispatch": false,
        "community_dispatch_ready": false,
        "local_model_ready": false,
        "sharded_pipeline_ready": false,
        "live_community_workers": 0,
        "model_id": Value::Null,
        "required_community_execution_profile":
            arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE,
        "mutation_free_observation": true,
    });
    (
        StatusCode::SERVICE_UNAVAILABLE,
        loading_unavailable_headers(),
        Json(body),
    )
}

async fn startup_worker_status(
    AxumState(loading): AxumState<StartupLoadingState>,
) -> (StatusCode, HeaderMap, Json<Value>) {
    let body = json!({
        "status": "loading",
        "role": loading.role.label(),
        "error": "worker is initializing its canonical model; not ready to claim or serve work",
    });
    (
        StatusCode::SERVICE_UNAVAILABLE,
        loading_unavailable_headers(),
        Json(body),
    )
}

async fn startup_service_unavailable() -> (StatusCode, HeaderMap, Json<Value>) {
    let body = json!({
        "error": "worker is initializing its canonical model; retry after startup completes",
    });
    (
        StatusCode::SERVICE_UNAVAILABLE,
        loading_unavailable_headers(),
        Json(body),
    )
}

pub fn startup_loading_router(loading_state: StartupLoadingState) -> Router {
    Router::new()
        .route(
            "/health",
            get(startup_health).fallback(startup_service_unavailable),
        )
        .route(
            "/inference/readiness",
            get(startup_inference_readiness).fallback(startup_service_unavailable),
        )
        .route(
            crate::community_worker::COMMUNITY_WORKER_STATUS_PATH,
            get(startup_worker_status).fallback(startup_service_unavailable),
        )
        .fallback(startup_service_unavailable)
        .layer(DefaultBodyLimit::max(STARTUP_LOADING_BODY_LIMIT_BYTES))
        .layer(CorsLayer::permissive())
        .with_state(loading_state)
}

struct ConnectionTracker {
    active_connections: AtomicUsize,
    active_handlers: AtomicUsize,
    handed_off: AtomicBool,
    force_close: AtomicBool,
    force_close_tx: tokio::sync::watch::Sender<bool>,
    accepted_notify: tokio::sync::Notify,
    drained_notify: tokio::sync::Notify,
}

impl ConnectionTracker {
    fn new() -> Self {
        let (force_close_tx, _force_close_rx) = tokio::sync::watch::channel(false);
        Self {
            active_connections: AtomicUsize::new(0),
            active_handlers: AtomicUsize::new(0),
            handed_off: AtomicBool::new(false),
            force_close: AtomicBool::new(false),
            force_close_tx,
            accepted_notify: tokio::sync::Notify::new(),
            drained_notify: tokio::sync::Notify::new(),
        }
    }

    fn active_connections(&self) -> usize {
        self.active_connections.load(Ordering::Acquire)
    }

    fn active_handlers(&self) -> usize {
        self.active_handlers.load(Ordering::Acquire)
    }

    fn mark_handed_off(&self) {
        self.handed_off.store(true, Ordering::Release);
    }

    fn trigger_force_close(&self) {
        self.force_close.store(true, Ordering::Release);
        let _ = self.force_close_tx.send(true);
    }

    fn notify_if_drained(&self) {
        if self.active_connections() == 0 && self.active_handlers() == 0 {
            self.drained_notify.notify_waiters();
        }
    }
}

struct SupervisedTcpStream {
    inner: tokio::net::TcpStream,
    tracker: Arc<ConnectionTracker>,
    force_close: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl SupervisedTcpStream {
    fn new(inner: tokio::net::TcpStream, tracker: Arc<ConnectionTracker>) -> Self {
        tracker.active_connections.fetch_add(1, Ordering::AcqRel);
        tracker.accepted_notify.notify_waiters();
        let force_close_rx = tracker.force_close_tx.subscribe();
        let force_close = Box::pin(wait_for_shutdown_signal(force_close_rx));
        Self {
            inner,
            tracker,
            force_close,
        }
    }

    fn poll_force_closed(&mut self, cx: &mut TaskContext<'_>) -> bool {
        if self.tracker.handed_off.load(Ordering::Acquire) {
            return false;
        }
        self.tracker.force_close.load(Ordering::Acquire)
            || self.force_close.as_mut().poll(cx).is_ready()
    }
}

impl Drop for SupervisedTcpStream {
    fn drop(&mut self) {
        self.tracker
            .active_connections
            .fetch_sub(1, Ordering::AcqRel);
        self.tracker.notify_if_drained();
    }
}

impl AsyncRead for SupervisedTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.poll_force_closed(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "startup RPC server closed stalled connection",
            )));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for SupervisedTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.poll_force_closed(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "startup RPC server closed stalled connection",
            )));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.poll_force_closed(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "startup RPC server closed stalled connection",
            )));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.poll_force_closed(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "startup RPC server closed stalled connection",
            )));
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

struct SupervisedTcpListener {
    inner: tokio::net::TcpListener,
    tracker: Arc<ConnectionTracker>,
}

impl axum::serve::Listener for SupervisedTcpListener {
    type Io = SupervisedTcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, addr) =
            <tokio::net::TcpListener as axum::serve::Listener>::accept(&mut self.inner).await;
        (
            SupervisedTcpStream::new(stream, Arc::clone(&self.tracker)),
            addr,
        )
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        <tokio::net::TcpListener as axum::serve::Listener>::local_addr(&self.inner)
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, SupervisedTcpListener>>
    for RpcPeerAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, SupervisedTcpListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

#[derive(Clone)]
struct SwappableRouterState {
    router_slot: Arc<parking_lot::RwLock<Router>>,
    tracker: Arc<ConnectionTracker>,
}

struct ActiveHandlerGuard {
    tracker: Arc<ConnectionTracker>,
}

impl ActiveHandlerGuard {
    fn enter(tracker: Arc<ConnectionTracker>) -> Self {
        tracker.active_handlers.fetch_add(1, Ordering::AcqRel);
        Self { tracker }
    }
}

impl Drop for ActiveHandlerGuard {
    fn drop(&mut self) {
        self.tracker.active_handlers.fetch_sub(1, Ordering::AcqRel);
        self.tracker.notify_if_drained();
    }
}

async fn dispatch_swappable_router(
    AxumState(state): AxumState<SwappableRouterState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let _active_handler = ActiveHandlerGuard::enter(Arc::clone(&state.tracker));
    let mut router = state.router_slot.read().clone();
    std::future::poll_fn(|cx| {
        <Router as tower::Service<axum::extract::Request>>::poll_ready(&mut router, cx)
    })
    .await
    .unwrap_or_else(|infallible| match infallible {});
    <Router as tower::Service<axum::extract::Request>>::call(&mut router, request)
        .await
        .unwrap_or_else(|infallible| match infallible {})
}

async fn wait_for_shutdown_signal(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    if *shutdown.borrow_and_update() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow_and_update() {
            return;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupStopOutcome {
    Drained,
    ForcedCloseAfterTimeout {
        remaining_connections: usize,
        remaining_handlers: usize,
    },
}

impl StartupStopOutcome {
    pub fn drained_cleanly(self) -> bool {
        matches!(self, Self::Drained)
    }

    pub fn has_live_handlers(self) -> bool {
        match self {
            Self::Drained => false,
            Self::ForcedCloseAfterTimeout {
                remaining_handlers, ..
            } => remaining_handlers > 0,
        }
    }
}

pub struct StartupRpcServer {
    local_addr: SocketAddr,
    router_slot: Arc<parking_lot::RwLock<Router>>,
    tracker: Arc<ConnectionTracker>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    server_task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

pub(crate) fn bind_rpc_tcp_listener(
    socket_addr: SocketAddr,
) -> std::io::Result<tokio::net::TcpListener> {
    let socket = match socket_addr {
        SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
    };
    #[cfg(unix)]
    socket.set_reuseaddr(true)?;
    socket.bind(socket_addr)?;
    socket.listen(1024)
}

impl Drop for StartupRpcServer {
    fn drop(&mut self) {
        if let Some(task) = self.server_task.take() {
            let _ = self.shutdown_tx.send(true);
            self.tracker.trigger_force_close();
            task.abort();
        }
    }
}

impl StartupRpcServer {
    pub fn start_tcp(address: &str, loading_state: StartupLoadingState) -> anyhow::Result<Self> {
        Self::start_tcp_with_router(address, startup_loading_router(loading_state))
    }

    fn start_tcp_with_router(address: &str, initial_router: Router) -> anyhow::Result<Self> {
        let socket_addr: SocketAddr = address.parse()?;
        let listener = bind_rpc_tcp_listener(socket_addr)?;
        let local_addr = listener.local_addr()?;
        let tracker = Arc::new(ConnectionTracker::new());
        let supervised_listener = SupervisedTcpListener {
            inner: listener,
            tracker: Arc::clone(&tracker),
        };
        let router_slot = Arc::new(parking_lot::RwLock::new(initial_router));
        let outer = Router::new()
            .fallback(dispatch_swappable_router)
            .with_state(SwappableRouterState {
                router_slot: Arc::clone(&router_slot),
                tracker: Arc::clone(&tracker),
            });
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = tokio::spawn(async move {
            axum::serve(
                supervised_listener,
                outer.into_make_service_with_connect_info::<RpcPeerAddr>(),
            )
            .with_graceful_shutdown(wait_for_shutdown_signal(shutdown_rx))
            .await
        });
        Ok(Self {
            local_addr,
            router_slot,
            tracker,
            shutdown_tx,
            server_task: Some(server_task),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn handoff_router(&self, ready_router: Router) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.server_task
                .as_ref()
                .is_some_and(|task| !task.is_finished()),
            "startup RPC server exited before router handoff"
        );
        *self.router_slot.write() = ready_router;
        self.tracker.mark_handed_off();
        Ok(())
    }

    pub async fn wait_until_shutdown(
        mut self,
        runtime_shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> anyhow::Result<()> {
        let mut server_task = self
            .server_task
            .take()
            .ok_or_else(|| anyhow::anyhow!("startup RPC server task already consumed"))?;
        if let Some(mut runtime_shutdown) = runtime_shutdown {
            tokio::select! {
                result = &mut server_task => {
                    let server_result = result
                        .context("RPC server task failed to join")?
                        .map_err(anyhow::Error::from);
                    server_result?;
                    let shutdown_signaled = *runtime_shutdown.borrow_and_update()
                        || runtime_shutdown.has_changed().is_err();
                    if !shutdown_signaled {
                        anyhow::bail!(
                            "RPC server task exited unexpectedly before shutdown was requested"
                        );
                    }
                    return Ok(());
                }
                () = wait_for_shutdown_signal(runtime_shutdown.clone()) => {
                    let _ = self.shutdown_tx.send(true);
                }
            }
        }
        server_task
            .await
            .context("RPC server task failed to join")?
            .map_err(anyhow::Error::from)
    }

    #[cfg(test)]
    pub(crate) fn internal_shutdown_sender_for_test(&self) -> tokio::sync::watch::Sender<bool> {
        self.shutdown_tx.clone()
    }

    #[cfg(test)]
    pub(crate) async fn finish_server_task_for_test(&self) {
        let _ = self.shutdown_tx.send(true);
        let deadline = Instant::now() + Duration::from_secs(2);
        while self
            .server_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            assert!(
                Instant::now() < deadline,
                "startup server task must finish after internal shutdown signal"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[cfg(test)]
    pub(crate) fn start_with_panicking_task_for_test(
        address: &str,
        loading_state: StartupLoadingState,
        panic_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> anyhow::Result<Self> {
        let socket_addr: SocketAddr = address.parse()?;
        let listener = bind_rpc_tcp_listener(socket_addr)?;
        let local_addr = listener.local_addr()?;
        let tracker = Arc::new(ConnectionTracker::new());
        let router_slot = Arc::new(parking_lot::RwLock::new(startup_loading_router(
            loading_state,
        )));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server_task = tokio::spawn(async move {
            let _listener = listener;
            tokio::select! {
                _ = panic_rx => panic!("simulated startup RPC server task panic"),
                () = wait_for_shutdown_signal(shutdown_rx) => Ok(()),
            }
        });
        Ok(Self {
            local_addr,
            router_slot,
            tracker,
            shutdown_tx,
            server_task: Some(server_task),
        })
    }

    async fn wait_for_tracker_drain(&self, timeout: Duration) -> bool {
        if self.tracker.active_connections() == 0 && self.tracker.active_handlers() == 0 {
            return true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let drained = self.tracker.drained_notify.notified();
            tokio::pin!(drained);
            drained.as_mut().enable();
            if self.tracker.active_connections() == 0 && self.tracker.active_handlers() == 0 {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            if tokio::time::timeout(remaining, drained).await.is_err() {
                return self.tracker.active_connections() == 0
                    && self.tracker.active_handlers() == 0;
            }
        }
    }

    pub async fn stop_bounded(
        mut self,
        drain_timeout: Duration,
    ) -> anyhow::Result<StartupStopOutcome> {
        let Some(mut server_task) = self.server_task.take() else {
            if self.tracker.active_connections() == 0 && self.tracker.active_handlers() == 0 {
                return Ok(StartupStopOutcome::Drained);
            }
            return Ok(StartupStopOutcome::ForcedCloseAfterTimeout {
                remaining_connections: self.tracker.active_connections(),
                remaining_handlers: self.tracker.active_handlers(),
            });
        };
        let drain_deadline = Instant::now() + drain_timeout;
        let _ = self.shutdown_tx.send(true);
        match tokio::time::timeout(drain_timeout, &mut server_task).await {
            Ok(Ok(Ok(()))) => {
                let remaining = drain_deadline.saturating_duration_since(Instant::now());
                if self.wait_for_tracker_drain(remaining).await {
                    Ok(StartupStopOutcome::Drained)
                } else {
                    self.tracker.trigger_force_close();
                    Ok(StartupStopOutcome::ForcedCloseAfterTimeout {
                        remaining_connections: self.tracker.active_connections(),
                        remaining_handlers: self.tracker.active_handlers(),
                    })
                }
            }
            Ok(Ok(Err(error))) => Err(error.into()),
            Ok(Err(error)) => Err(anyhow::anyhow!(
                "startup RPC server task failed to join: {error}"
            )),
            Err(_) => {
                self.tracker.trigger_force_close();
                let force_close_timeout = drain_timeout.min(STARTUP_FORCE_CLOSE_TIMEOUT);
                let force_deadline = Instant::now() + force_close_timeout;
                match tokio::time::timeout(force_close_timeout, &mut server_task).await {
                    Ok(Ok(Ok(()))) => {
                        let remaining = force_deadline.saturating_duration_since(Instant::now());
                        let _ = self.wait_for_tracker_drain(remaining).await;
                    }
                    Ok(Ok(Err(error))) => return Err(error.into()),
                    Ok(Err(error)) => {
                        return Err(anyhow::anyhow!(
                            "startup RPC server task failed to join after force-close: {error}"
                        ));
                    }
                    Err(_) => {
                        server_task.abort();
                        let _ = server_task.await;
                    }
                }
                Ok(StartupStopOutcome::ForcedCloseAfterTimeout {
                    remaining_connections: self.tracker.active_connections(),
                    remaining_handlers: self.tracker.active_handlers(),
                })
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartupPreparationOutcome<T> {
    Ready(T),
    Cancelled,
}

pub async fn run_cancellable_blocking_preparation<T, E, F, IsCancelled>(
    shutdown_requested: Arc<AtomicBool>,
    task_name: &'static str,
    work: F,
    is_cancelled: IsCancelled,
) -> anyhow::Result<StartupPreparationOutcome<T>>
where
    T: Send + 'static,
    E: Into<anyhow::Error> + Send + 'static,
    F: FnOnce(Arc<AtomicBool>) -> Result<T, E> + Send + 'static,
    IsCancelled: FnOnce(&E) -> bool,
{
    let cancel_for_worker = Arc::clone(&shutdown_requested);
    let outcome = tokio::task::spawn_blocking(move || work(cancel_for_worker))
        .await
        .with_context(|| format!("{task_name} task panicked"))?;
    match outcome {
        Ok(value) => {
            if shutdown_requested.load(Ordering::SeqCst) {
                drop(value);
                Ok(StartupPreparationOutcome::Cancelled)
            } else {
                Ok(StartupPreparationOutcome::Ready(value))
            }
        }
        Err(error) => {
            if is_cancelled(&error) && shutdown_requested.load(Ordering::SeqCst) {
                Ok(StartupPreparationOutcome::Cancelled)
            } else {
                Err(error.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_inference::NativeRequestAdmission;
    use crate::rpc;
    use arc_crypto::KeyPair;
    use arc_mempool::Mempool;
    use std::sync::atomic::AtomicU32;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn base_full_worker_policy() -> WorkerStartupPolicyInput<'static> {
        WorkerStartupPolicyInput {
            full_integer_worker: true,
            community_shard_worker: false,
            stake: 0,
            chain_participation_enabled: false,
            is_shard_holder: false,
            auto_shard_join: false,
            community_shard_coordinator: false,
            native_inference_runtime: false,
            tokenizer_only: false,
            enable_i16: false,
            model_path: Some("/models/llama-2-7b-chat.Q4_K_M.gguf"),
            tcp_rpc_listener: true,
        }
    }

    fn test_loading_state(role: EarlyWorkerStartupRole) -> StartupLoadingState {
        StartupLoadingState {
            role,
            state: Arc::new(StateDB::new()),
            boot_time: Instant::now(),
            transport_wire_policy: Arc::new(arc_net::transport::TransportWirePolicy::new(false)),
        }
    }

    fn http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    #[test]
    fn early_worker_startup_role_policy_restricts_to_eligible_tcp_canonical_workers() {
        let full = base_full_worker_policy();
        assert_eq!(
            early_worker_startup_role(&full),
            Some(EarlyWorkerStartupRole::FullIntegerWorker)
        );

        let shard = WorkerStartupPolicyInput {
            full_integer_worker: false,
            community_shard_worker: true,
            ..full
        };
        assert_eq!(
            early_worker_startup_role(&shard),
            Some(EarlyWorkerStartupRole::CommunityShardWorker)
        );

        let rejected_cases = [
            WorkerStartupPolicyInput {
                full_integer_worker: false,
                community_shard_worker: false,
                ..full
            },
            WorkerStartupPolicyInput {
                full_integer_worker: true,
                community_shard_worker: true,
                ..full
            },
            WorkerStartupPolicyInput { stake: 1, ..full },
            WorkerStartupPolicyInput {
                chain_participation_enabled: true,
                ..full
            },
            WorkerStartupPolicyInput {
                is_shard_holder: true,
                ..full
            },
            WorkerStartupPolicyInput {
                auto_shard_join: true,
                ..full
            },
            WorkerStartupPolicyInput {
                community_shard_coordinator: true,
                ..full
            },
            WorkerStartupPolicyInput {
                native_inference_runtime: true,
                ..full
            },
            WorkerStartupPolicyInput {
                tokenizer_only: true,
                ..full
            },
            WorkerStartupPolicyInput {
                enable_i16: true,
                ..full
            },
            WorkerStartupPolicyInput {
                model_path: None,
                ..full
            },
            WorkerStartupPolicyInput {
                model_path: Some("/models/llama.arc-int8"),
                ..full
            },
            WorkerStartupPolicyInput {
                tcp_rpc_listener: false,
                ..full
            },
            WorkerStartupPolicyInput {
                tcp_rpc_listener: false,
                ..shard
            },
        ];
        for case in rejected_cases {
            assert_eq!(
                early_worker_startup_role(&case),
                None,
                "case must not enable early worker startup RPC: {case:?}"
            );
        }
    }

    #[tokio::test]
    async fn rpc_tcp_listener_rejects_an_occupied_port_and_rebinds_after_release() {
        let first = bind_rpc_tcp_listener("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = first.local_addr().unwrap();
        assert!(bind_rpc_tcp_listener(address).is_err());
        assert!(
            StartupRpcServer::start_tcp(
                &address.to_string(),
                test_loading_state(EarlyWorkerStartupRole::FullIntegerWorker),
            )
            .is_err()
        );
        drop(first);
        let rebound = bind_rpc_tcp_listener(address).unwrap();
        assert_eq!(rebound.local_addr().unwrap(), address);

        if let Ok(probe) = std::net::TcpListener::bind("[::1]:0") {
            drop(probe);
            let first_v6 = bind_rpc_tcp_listener("[::1]:0".parse().unwrap()).unwrap();
            let address_v6 = first_v6.local_addr().unwrap();
            assert!(bind_rpc_tcp_listener(address_v6).is_err());
            drop(first_v6);
            let rebound_v6 = bind_rpc_tcp_listener(address_v6).unwrap();
            assert_eq!(rebound_v6.local_addr().unwrap(), address_v6);
        }
    }

    #[tokio::test]
    async fn startup_loading_serves_health_loading_and_rejects_readiness_and_mutations_503() {
        let server = StartupRpcServer::start_tcp(
            "127.0.0.1:0",
            test_loading_state(EarlyWorkerStartupRole::FullIntegerWorker),
        )
        .unwrap();
        let base = format!("http://{}", server.local_addr());
        let client = http_client();

        let health = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(health.status(), reqwest::StatusCode::OK);
        assert_eq!(
            health
                .headers()
                .get(reqwest::header::CONNECTION)
                .and_then(|v| v.to_str().ok()),
            Some("close")
        );
        let health_json: Value = health.json().await.unwrap();
        assert_eq!(health_json["status"], json!("loading"));
        assert_eq!(
            health_json["startup_phase"],
            json!(WORKER_STARTUP_PHASE_LOADING_MODEL)
        );
        assert_eq!(health_json["chain_participation_enabled"], json!(false));
        assert_eq!(health_json["peers"], json!(0));
        assert!(
            health_json["degraded_reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("full_integer_worker"))
        );

        let readiness = client
            .get(format!("{base}/inference/readiness"))
            .send()
            .await
            .unwrap();
        assert_eq!(readiness.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            readiness
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some(STARTUP_RETRY_AFTER_SECS)
        );
        let readiness_json: Value = readiness.json().await.unwrap();
        assert_eq!(readiness_json.as_object().unwrap().len(), 9);
        assert_eq!(readiness_json["safe_to_dispatch"], json!(false));
        assert_eq!(readiness_json["local_model_ready"], json!(false));
        assert_eq!(readiness_json["community_dispatch_ready"], json!(false));
        assert_eq!(readiness_json["sharded_pipeline_ready"], json!(false));

        let worker_status = client
            .get(format!(
                "{base}{}",
                crate::community_worker::COMMUNITY_WORKER_STATUS_PATH
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            worker_status.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        );

        for path in [
            "/inference/run",
            "/tx/submit",
            "/tx/submit_signed",
            "/faucet/claim",
            "/community/register",
            "/eth",
        ] {
            let response = client
                .post(format!("{base}{path}"))
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "POST {path} must return 503 during startup loading"
            );
            assert_eq!(
                response
                    .headers()
                    .get(reqwest::header::CONNECTION)
                    .and_then(|v| v.to_str().ok()),
                Some("close")
            );
        }

        for path in [
            "/health",
            "/inference/readiness",
            crate::community_worker::COMMUNITY_WORKER_STATUS_PATH,
        ] {
            for method in [
                reqwest::Method::POST,
                reqwest::Method::PUT,
                reqwest::Method::DELETE,
                reqwest::Method::PATCH,
            ] {
                let response = client
                    .request(method.clone(), format!("{base}{path}"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    "{method} {path} must return 503 during startup loading"
                );
                assert_eq!(
                    response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok()),
                    Some(STARTUP_RETRY_AFTER_SECS)
                );
                assert_eq!(
                    response
                        .headers()
                        .get(reqwest::header::CONNECTION)
                        .and_then(|v| v.to_str().ok()),
                    Some("close")
                );
            }
        }

        let stop_outcome = server
            .stop_bounded(STARTUP_SHUTDOWN_DRAIN_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(stop_outcome, StartupStopOutcome::Drained);
        assert!(stop_outcome.drained_cleanly());
        assert!(!stop_outcome.has_live_handlers());
    }

    #[tokio::test]
    async fn startup_rpc_server_hands_off_to_full_router_on_same_listener_without_gap() {
        let state = Arc::new(StateDB::new());
        let wire_policy = Arc::new(arc_net::transport::TransportWirePolicy::new(false));
        let boot_time = Instant::now();
        let startup_server = StartupRpcServer::start_tcp(
            "127.0.0.1:0",
            StartupLoadingState {
                role: EarlyWorkerStartupRole::FullIntegerWorker,
                state: Arc::clone(&state),
                boot_time,
                transport_wire_policy: Arc::clone(&wire_policy),
            },
        )
        .unwrap();
        let addr = startup_server.local_addr();
        let base = format!("http://{addr}");
        let client = http_client();

        let pre_health: Value = client
            .get(format!("{base}/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(pre_health["status"], json!("loading"));
        let pre_node_info = client
            .get(format!("{base}/node/info"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            pre_node_info.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        );

        let keypair = Arc::new(KeyPair::generate_ed25519());
        let expected_address = keypair.address();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let probe_stop = Arc::new(AtomicBool::new(false));
        let probe_stop_task = Arc::clone(&probe_stop);
        let probe_base = base.clone();
        let probe_client = http_client();
        let probe_task = tokio::spawn(async move {
            let mut probes = 0usize;
            while !probe_stop_task.load(Ordering::Acquire) {
                let response = probe_client
                    .get(format!("{probe_base}/health"))
                    .send()
                    .await
                    .expect("listener must never drop or refuse connections across handoff");
                assert_eq!(response.status(), reqwest::StatusCode::OK);
                probes += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            probes
        });

        let serve_task = tokio::spawn(rpc::serve_on_startup_server(
            startup_server,
            state,
            Arc::new(Mempool::new(1_000)),
            expected_address,
            Some(keypair),
            0,
            boot_time,
            Arc::new(AtomicU32::new(0)),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            0,
            rpc::ComputePool::default(),
            None,
            false,
            None,
            Arc::new(NativeRequestAdmission::new(false)),
            Some(shutdown_rx),
            wire_policy,
            crate::twin::TwinConfig::default(),
            false,
            rpc::CommunityProfileServing::default(),
            false,
        ));

        let mut handed_off = false;
        for _ in 0..100 {
            let health: Value = client
                .get(format!("{base}/health"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if health["status"] != json!("loading") {
                assert!(health.get("startup_phase").is_none());
                handed_off = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            handed_off,
            "router handoff must transition /health out of loading"
        );

        probe_stop.store(true, Ordering::Release);
        let probe_count = probe_task.await.unwrap();
        assert!(probe_count > 0);

        let node_info_resp = client
            .get(format!("{base}/node/info"))
            .send()
            .await
            .unwrap();
        assert_eq!(node_info_resp.status(), reqwest::StatusCode::OK);
        let node_info: Value = node_info_resp.json().await.unwrap();
        assert_eq!(node_info["validator"], json!(expected_address.to_hex()));

        let readiness_resp = client
            .get(format!("{base}/inference/readiness"))
            .send()
            .await
            .unwrap();
        assert_eq!(readiness_resp.status(), reqwest::StatusCode::OK);

        let _ = shutdown_tx.send(true);
        serve_task.await.unwrap().unwrap();
    }

    #[derive(Debug, thiserror::Error)]
    enum TestPreparationError {
        #[error("test preparation cancelled")]
        Cancelled,
    }

    #[tokio::test]
    async fn cancellable_preparation_joins_before_cleanup_and_distinguishes_shutdown_from_errors() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let joined = Arc::new(AtomicBool::new(false));
        let entered_worker = Arc::clone(&entered);
        let joined_worker = Arc::clone(&joined);
        let shutdown_trigger = Arc::clone(&shutdown);

        let trigger_task = tokio::spawn(async move {
            while !entered_worker.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            shutdown_trigger.store(true, Ordering::SeqCst);
        });

        let outcome = run_cancellable_blocking_preparation(
            Arc::clone(&shutdown),
            "test model preparation",
            move |cancel| -> Result<u32, TestPreparationError> {
                entered.store(true, Ordering::Release);
                while !cancel.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(2));
                }
                std::thread::sleep(Duration::from_millis(15));
                joined_worker.store(true, Ordering::Release);
                Err(TestPreparationError::Cancelled)
            },
            |error| matches!(error, TestPreparationError::Cancelled),
        )
        .await
        .unwrap();
        trigger_task.await.unwrap();
        assert_eq!(outcome, StartupPreparationOutcome::Cancelled);
        assert!(
            joined.load(Ordering::Acquire),
            "blocking preparation must finish joining before returning Cancelled"
        );

        let not_shutdown = Arc::new(AtomicBool::new(false));
        let spurious = run_cancellable_blocking_preparation(
            Arc::clone(&not_shutdown),
            "spurious cancel",
            |_cancel| -> Result<u32, anyhow::Error> { Err(anyhow::anyhow!("cancelled")) },
            |_| true,
        )
        .await;
        assert!(
            spurious.is_err(),
            "cancellation error without shutdown_requested must be fatal"
        );

        let joined_on_err = Arc::new(AtomicBool::new(false));
        let joined_on_err_worker = Arc::clone(&joined_on_err);
        let real_err = run_cancellable_blocking_preparation(
            Arc::clone(&not_shutdown),
            "real error",
            move |_cancel| -> Result<u32, anyhow::Error> {
                std::thread::sleep(Duration::from_millis(10));
                joined_on_err_worker.store(true, Ordering::Release);
                Err(anyhow::anyhow!("corrupt gguf"))
            },
            |_| false,
        )
        .await;
        assert!(real_err.is_err());
        assert!(joined_on_err.load(Ordering::Acquire));

        let server = StartupRpcServer::start_tcp(
            "127.0.0.1:0",
            test_loading_state(EarlyWorkerStartupRole::CommunityShardWorker),
        )
        .unwrap();
        let addr = server.local_addr();
        let tracker = Arc::clone(&server.tracker);
        let accepted = tracker.accepted_notify.notified();
        tokio::pin!(accepted);
        accepted.as_mut().enable();
        let mut stalled_client = tokio::net::TcpStream::connect(addr).await.unwrap();
        stalled_client.write_all(b"GET /hea").await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), accepted)
            .await
            .expect("stalled client must be accepted by supervised startup listener");
        let stop_started = Instant::now();
        let stop_outcome = server
            .stop_bounded(Duration::from_millis(120))
            .await
            .unwrap();
        assert!(
            stop_started.elapsed() < Duration::from_secs(2),
            "stalled startup connection must not hang bounded stop"
        );
        assert_eq!(
            stop_outcome,
            StartupStopOutcome::ForcedCloseAfterTimeout {
                remaining_connections: 0,
                remaining_handlers: 0,
            }
        );
        assert!(!stop_outcome.drained_cleanly());
        assert!(!stop_outcome.has_live_handlers());
        assert!(
            tokio::net::TcpStream::connect(addr).await.is_err(),
            "stopped startup listener must immediately refuse new connections"
        );
        let mut eof_probe = [0u8; 16];
        let peer_closed =
            tokio::time::timeout(Duration::from_secs(2), stalled_client.read(&mut eof_probe))
                .await
                .expect("force-closed stalled socket must signal EOF or reset promptly");
        match peer_closed {
            Ok(0) | Err(_) => {}
            Ok(bytes) => panic!("unexpected response bytes ({bytes}) on force-closed socket"),
        }
        drop(stalled_client);
        #[cfg(unix)]
        {
            let rebound = bind_rpc_tcp_listener(addr);
            assert!(
                rebound.is_ok(),
                "listener port must be released after bounded startup stop"
            );
        }

        let idle_server = StartupRpcServer::start_tcp(
            "127.0.0.1:0",
            test_loading_state(EarlyWorkerStartupRole::CommunityShardWorker),
        )
        .unwrap();
        let idle_addr = idle_server.local_addr();
        let idle_outcome = idle_server
            .stop_bounded(STARTUP_SHUTDOWN_DRAIN_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(idle_outcome, StartupStopOutcome::Drained);
        assert!(
            bind_rpc_tcp_listener(idle_addr).is_ok(),
            "cleanly drained startup listener must release port for immediate rebind"
        );
    }

    #[tokio::test]
    async fn stop_bounded_force_close_cancels_pending_async_handler_and_refuses_new_connections() {
        struct HandlerDropOutcome {
            outcome_tx: Option<tokio::sync::oneshot::Sender<bool>>,
            completed: bool,
        }

        impl Drop for HandlerDropOutcome {
            fn drop(&mut self) {
                if let Some(tx) = self.outcome_tx.take() {
                    let _ = tx.send(self.completed);
                }
            }
        }

        let make_slow_router = || {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel::<bool>();
            let entered_slot = Arc::new(parking_lot::Mutex::new(Some(entered_tx)));
            let release_slot = Arc::new(parking_lot::Mutex::new(Some(release_rx)));
            let outcome_slot = Arc::new(parking_lot::Mutex::new(Some(outcome_tx)));

            let router = Router::new().route(
                "/slow",
                get({
                    let entered_slot = Arc::clone(&entered_slot);
                    let release_slot = Arc::clone(&release_slot);
                    let outcome_slot = Arc::clone(&outcome_slot);
                    move || {
                        let entered_slot = Arc::clone(&entered_slot);
                        let release_slot = Arc::clone(&release_slot);
                        let outcome_slot = Arc::clone(&outcome_slot);
                        async move {
                            let entered_tx = entered_slot.lock().take();
                            let release_rx = release_slot.lock().take();
                            let outcome_tx = outcome_slot.lock().take();
                            let mut drop_guard = HandlerDropOutcome {
                                outcome_tx,
                                completed: false,
                            };
                            if let Some(tx) = entered_tx {
                                let _ = tx.send(());
                            }
                            if let Some(rx) = release_rx {
                                let _ = rx.await;
                            }
                            drop_guard.completed = true;
                            (StatusCode::OK, close_connection_headers(), "done")
                        }
                    }
                }),
            );
            (router, entered_rx, release_tx, outcome_rx)
        };

        let (router, entered_rx, _release_tx, outcome_rx) = make_slow_router();
        let server = StartupRpcServer::start_tcp_with_router("127.0.0.1:0", router).unwrap();
        let addr = server.local_addr();
        let tracker = Arc::clone(&server.tracker);
        let client = http_client();
        let slow_url = format!("http://{addr}/slow");
        let request_task = tokio::spawn(async move { client.get(slow_url).send().await });

        tokio::time::timeout(Duration::from_secs(5), entered_rx)
            .await
            .expect("pending async handler must start executing")
            .unwrap();
        assert_eq!(tracker.active_handlers(), 1);
        assert_eq!(tracker.active_connections(), 1);

        let stop_outcome = server
            .stop_bounded(Duration::from_millis(50))
            .await
            .unwrap();
        assert!(!stop_outcome.drained_cleanly());
        assert!(!stop_outcome.has_live_handlers());
        assert_eq!(
            stop_outcome,
            StartupStopOutcome::ForcedCloseAfterTimeout {
                remaining_connections: 0,
                remaining_handlers: 0,
            }
        );
        let completed = tokio::time::timeout(Duration::from_secs(5), outcome_rx)
            .await
            .expect("canceled async handler must run its Drop guard promptly")
            .unwrap();
        assert!(
            !completed,
            "forced socket close must cancel the pending async handler before completion"
        );
        assert!(
            tokio::net::TcpStream::connect(addr).await.is_err(),
            "stopped startup listener must refuse new connections"
        );
        let _ = request_task.await;
        assert_eq!(tracker.active_handlers(), 0);
        assert_eq!(tracker.active_connections(), 0);

        let (drain_router, drain_entered_rx, drain_release_tx, drain_outcome_rx) =
            make_slow_router();
        let drain_server =
            StartupRpcServer::start_tcp_with_router("127.0.0.1:0", drain_router).unwrap();
        let drain_addr = drain_server.local_addr();
        let drain_tracker = Arc::clone(&drain_server.tracker);
        let drain_client = http_client();
        let drain_url = format!("http://{drain_addr}/slow");
        let drain_request = tokio::spawn(async move { drain_client.get(drain_url).send().await });

        tokio::time::timeout(Duration::from_secs(5), drain_entered_rx)
            .await
            .expect("graceful-drain async handler must start executing")
            .unwrap();
        assert_eq!(drain_tracker.active_handlers(), 1);
        assert_eq!(drain_tracker.active_connections(), 1);

        let drain_outcome = {
            let stop_fut = drain_server.stop_bounded(Duration::from_secs(2));
            tokio::pin!(stop_fut);
            assert!(
                tokio::time::timeout(Duration::from_millis(25), &mut stop_fut)
                    .await
                    .is_err(),
                "graceful stop must wait while the in-flight handler completes within budget"
            );
            assert!(tokio::net::TcpStream::connect(drain_addr).await.is_err());
            let _ = drain_release_tx.send(());
            tokio::time::timeout(Duration::from_secs(5), &mut stop_fut)
                .await
                .expect("graceful stop must finish once in-flight handler completes")
                .unwrap()
        };
        assert_eq!(drain_outcome, StartupStopOutcome::Drained);
        assert!(drain_outcome.drained_cleanly());
        assert!(!drain_outcome.has_live_handlers());
        let drain_completed = tokio::time::timeout(Duration::from_secs(5), drain_outcome_rx)
            .await
            .expect("drained handler must report completion via its Drop guard")
            .unwrap();
        assert!(drain_completed);
        let response = drain_request.await.unwrap().unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(drain_tracker.active_handlers(), 0);
        assert_eq!(drain_tracker.active_connections(), 0);
    }

    #[tokio::test]
    async fn serve_eth_rejects_occupied_tcp_port() {
        let first = bind_rpc_tcp_listener("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = first.local_addr().unwrap();
        let make_eth_node = || {
            let keypair = KeyPair::generate_ed25519();
            rpc::build_node_state(
                Arc::new(StateDB::new()),
                Arc::new(Mempool::new(1_000)),
                keypair.address(),
                Some(Arc::new(keypair)),
                0,
                Instant::now(),
                Arc::new(AtomicU32::new(0)),
                None,
                None,
                None,
                None,
            )
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let occupied = rpc::serve_eth(
            &address.to_string(),
            make_eth_node(),
            Some(shutdown_rx.clone()),
        )
        .await;
        assert!(occupied.is_err(), "serve_eth must reject an occupied port");

        drop(first);
        let _ = shutdown_tx.send(true);
        let rebound =
            rpc::serve_eth(&address.to_string(), make_eth_node(), Some(shutdown_rx)).await;
        assert!(
            rebound.is_ok(),
            "serve_eth must bind cleanly after port release and exit on shutdown"
        );
    }

    #[tokio::test]
    async fn serve_on_startup_server_pre_handoff_error_signals_and_drains_rpc_tasks() {
        let state = Arc::new(StateDB::new());
        let admission = Arc::new(NativeRequestAdmission::new(false));
        let wire_policy = Arc::new(arc_net::transport::TransportWirePolicy::new(false));
        let boot_time = Instant::now();
        let startup_server = StartupRpcServer::start_tcp(
            "127.0.0.1:0",
            StartupLoadingState {
                role: EarlyWorkerStartupRole::FullIntegerWorker,
                state: Arc::clone(&state),
                boot_time,
                transport_wire_policy: Arc::clone(&wire_policy),
            },
        )
        .unwrap();
        startup_server.finish_server_task_for_test().await;

        let keypair = Arc::new(KeyPair::generate_ed25519());
        let validator_address = keypair.address();
        let (external_shutdown_tx, external_shutdown_rx) = tokio::sync::watch::channel(false);
        let observer_started = Arc::new(AtomicBool::new(false));
        let observer_exited = Arc::new(AtomicBool::new(false));
        let hook_started = Arc::clone(&observer_started);
        let hook_exited = Arc::clone(&observer_exited);

        let handoff_err = tokio::time::timeout(
            Duration::from_secs(5),
            rpc::serve_on_startup_server_with_post_build_hook(
                startup_server,
                Arc::clone(&state),
                Arc::new(Mempool::new(1_000)),
                validator_address,
                Some(Arc::clone(&keypair)),
                0,
                boot_time,
                Arc::new(AtomicU32::new(0)),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                0,
                rpc::ComputePool::default(),
                None,
                false,
                None,
                Arc::clone(&admission),
                Some(external_shutdown_rx),
                Arc::clone(&wire_policy),
                crate::twin::TwinConfig::default(),
                false,
                rpc::CommunityProfileServing::default(),
                false,
                move |node| {
                    rpc::spawn_test_runtime_shutdown_observer(
                        node,
                        hook_started,
                        hook_exited,
                        None,
                        None,
                    );
                },
            ),
        )
        .await
        .expect("pre-handoff error cleanup must signal and drain RPC tasks without deadlocking")
        .expect_err("exited startup server must fail handoff_router");
        let handoff_msg = handoff_err.to_string();
        assert!(handoff_msg.contains("startup RPC server exited before router handoff"));
        assert!(!*external_shutdown_tx.borrow());
        assert!(observer_started.load(Ordering::Acquire));
        assert!(observer_exited.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&state), 1);
        assert_eq!(Arc::strong_count(&admission), 1);

        let build_fail_server = StartupRpcServer::start_tcp(
            "127.0.0.1:0",
            StartupLoadingState {
                role: EarlyWorkerStartupRole::FullIntegerWorker,
                state: Arc::clone(&state),
                boot_time,
                transport_wire_policy: Arc::clone(&wire_policy),
            },
        )
        .unwrap();
        let build_fail_addr = build_fail_server.local_addr();
        let (build_shutdown_tx, build_shutdown_rx) = tokio::sync::watch::channel(false);
        let build_err = tokio::time::timeout(
            Duration::from_secs(5),
            rpc::serve_on_startup_server(
                build_fail_server,
                Arc::clone(&state),
                Arc::new(Mempool::new(1_000)),
                validator_address,
                Some(Arc::clone(&keypair)),
                0,
                boot_time,
                Arc::new(AtomicU32::new(0)),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                0,
                rpc::ComputePool::default(),
                None,
                true,
                None,
                Arc::clone(&admission),
                Some(build_shutdown_rx),
                Arc::clone(&wire_policy),
                crate::twin::TwinConfig::default(),
                false,
                rpc::CommunityProfileServing::default(),
                false,
            ),
        )
        .await
        .expect("failed RPC factory must stop startup server promptly")
        .expect_err("missing rewards activation height must fail factory build");
        let build_msg = build_err.to_string();
        assert!(build_msg.contains("community_rewards_v1_activation_height"));
        assert!(!*build_shutdown_tx.borrow());
        assert!(
            tokio::net::TcpStream::connect(build_fail_addr)
                .await
                .is_err()
        );
        assert_eq!(Arc::strong_count(&state), 1);
        assert_eq!(Arc::strong_count(&admission), 1);

        let occupied_listener = bind_rpc_tcp_listener("127.0.0.1:0".parse().unwrap()).unwrap();
        let occupied_addr = occupied_listener.local_addr().unwrap();
        let (serve_shutdown_tx, serve_shutdown_rx) = tokio::sync::watch::channel(false);
        let bind_err = tokio::time::timeout(
            Duration::from_secs(5),
            rpc::serve(
                rpc::RpcListen::Tcp(occupied_addr.to_string()),
                Arc::clone(&state),
                Arc::new(Mempool::new(1_000)),
                validator_address,
                Some(keypair),
                0,
                boot_time,
                Arc::new(AtomicU32::new(0)),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                0,
                rpc::ComputePool::default(),
                None,
                false,
                None,
                Arc::clone(&admission),
                Some(serve_shutdown_rx),
                wire_policy,
                crate::twin::TwinConfig::default(),
                false,
                rpc::CommunityProfileServing::default(),
                false,
            ),
        )
        .await
        .expect("rpc::serve bind failure must drain spawned RPC tasks without hanging")
        .expect_err("rpc::serve must reject an occupied TCP port");
        drop(occupied_listener);
        assert!(!bind_err.to_string().is_empty());
        assert!(!*serve_shutdown_tx.borrow());
        assert_eq!(Arc::strong_count(&state), 1);
        assert_eq!(Arc::strong_count(&admission), 1);
    }

    #[tokio::test]
    async fn serve_on_startup_server_unexpected_stop_and_panic_signal_and_drain_rpc_tasks() {
        let state = Arc::new(StateDB::new());
        let admission = Arc::new(NativeRequestAdmission::new(false));
        let wire_policy = Arc::new(arc_net::transport::TransportWirePolicy::new(false));
        let boot_time = Instant::now();
        let startup_server = StartupRpcServer::start_tcp(
            "127.0.0.1:0",
            StartupLoadingState {
                role: EarlyWorkerStartupRole::FullIntegerWorker,
                state: Arc::clone(&state),
                boot_time,
                transport_wire_policy: Arc::clone(&wire_policy),
            },
        )
        .unwrap();
        let addr = startup_server.local_addr();
        let internal_shutdown_tx = startup_server.internal_shutdown_sender_for_test();

        let keypair = Arc::new(KeyPair::generate_ed25519());
        let validator_address = keypair.address();
        let (external_shutdown_tx, external_shutdown_rx) = tokio::sync::watch::channel(false);
        let observer_started = Arc::new(AtomicBool::new(false));
        let observer_exited = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (exited_tx, exited_rx) = tokio::sync::oneshot::channel();
        let (permit_tx, permit_rx) = tokio::sync::oneshot::channel();
        let hook_started = Arc::clone(&observer_started);
        let hook_exited = Arc::clone(&observer_exited);

        let mut serve_task = tokio::spawn(rpc::serve_on_startup_server_with_post_build_hook(
            startup_server,
            Arc::clone(&state),
            Arc::new(Mempool::new(1_000)),
            validator_address,
            Some(Arc::clone(&keypair)),
            0,
            boot_time,
            Arc::new(AtomicU32::new(0)),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            0,
            rpc::ComputePool::default(),
            None,
            false,
            None,
            Arc::clone(&admission),
            Some(external_shutdown_rx),
            Arc::clone(&wire_policy),
            crate::twin::TwinConfig::default(),
            false,
            rpc::CommunityProfileServing::default(),
            false,
            move |node| {
                let permit = rpc::try_acquire_test_public_inference_permit(node);
                let _ = permit_tx.send(permit);
                rpc::spawn_test_runtime_shutdown_observer(
                    node,
                    hook_started,
                    hook_exited,
                    Some(started_tx),
                    Some(exited_tx),
                );
            },
        ));

        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("observer runtime task must start")
            .unwrap();
        let active_compute_permit = tokio::time::timeout(Duration::from_secs(2), permit_rx)
            .await
            .expect("compute permit must be acquired during post-build hook")
            .unwrap();

        let client = http_client();
        let mut handed_off = false;
        for _ in 0..100 {
            let health: Value = client
                .get(format!("http://{addr}/health"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if health["status"] != json!("loading") {
                handed_off = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            handed_off,
            "router handoff must complete before unexpected stop"
        );
        drop(client);

        let _ = internal_shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(2), exited_rx)
            .await
            .expect("unexpected server exit must signal RPC runtime tasks to complete")
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut serve_task)
                .await
                .is_err(),
            "compute barrier must wait for active compute instead of aborting it"
        );
        drop(active_compute_permit);

        let unexpected_err = tokio::time::timeout(Duration::from_secs(5), serve_task)
            .await
            .expect("unexpected server exit cleanup must complete once active compute finishes")
            .unwrap()
            .expect_err("unexpected server exit without external shutdown must return Err");
        let unexpected_msg = unexpected_err.to_string();
        assert!(unexpected_msg.contains("exited unexpectedly before shutdown was requested"));
        assert!(!*external_shutdown_tx.borrow());
        assert!(observer_started.load(Ordering::Acquire));
        assert!(observer_exited.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&state), 1);
        assert_eq!(Arc::strong_count(&admission), 1);

        let (panic_tx, panic_rx) = tokio::sync::oneshot::channel();
        let panicking_server = StartupRpcServer::start_with_panicking_task_for_test(
            "127.0.0.1:0",
            StartupLoadingState {
                role: EarlyWorkerStartupRole::FullIntegerWorker,
                state: Arc::clone(&state),
                boot_time,
                transport_wire_policy: Arc::clone(&wire_policy),
            },
            panic_rx,
        )
        .unwrap();
        let panic_tracker = Arc::clone(&panicking_server.tracker);
        let (panic_shutdown_tx, panic_shutdown_rx) = tokio::sync::watch::channel(false);
        let panic_observer_started = Arc::new(AtomicBool::new(false));
        let panic_observer_exited = Arc::new(AtomicBool::new(false));
        let (panic_started_tx, panic_started_rx) = tokio::sync::oneshot::channel();
        let hook_panic_started = Arc::clone(&panic_observer_started);
        let hook_panic_exited = Arc::clone(&panic_observer_exited);

        let panic_serve_task = tokio::spawn(rpc::serve_on_startup_server_with_post_build_hook(
            panicking_server,
            Arc::clone(&state),
            Arc::new(Mempool::new(1_000)),
            validator_address,
            Some(keypair),
            0,
            boot_time,
            Arc::new(AtomicU32::new(0)),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            0,
            rpc::ComputePool::default(),
            None,
            false,
            None,
            Arc::clone(&admission),
            Some(panic_shutdown_rx),
            wire_policy,
            crate::twin::TwinConfig::default(),
            false,
            rpc::CommunityProfileServing::default(),
            false,
            move |node| {
                rpc::spawn_test_runtime_shutdown_observer(
                    node,
                    hook_panic_started,
                    hook_panic_exited,
                    Some(panic_started_tx),
                    None,
                );
            },
        ));

        tokio::time::timeout(Duration::from_secs(2), panic_started_rx)
            .await
            .expect("panic-path observer runtime task must start")
            .unwrap();
        let handoff_deadline = Instant::now() + Duration::from_secs(2);
        while !panic_tracker.handed_off.load(Ordering::Acquire) {
            assert!(
                Instant::now() < handoff_deadline,
                "panic-path router handoff must complete before injecting panic"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let _ = panic_tx.send(());

        let panic_err = tokio::time::timeout(Duration::from_secs(5), panic_serve_task)
            .await
            .expect("server panic cleanup must signal and drain RPC tasks without hanging")
            .unwrap()
            .expect_err("panicking startup RPC task must propagate join error");
        assert!(
            panic_err
                .to_string()
                .contains("RPC server task failed to join")
        );
        assert!(!*panic_shutdown_tx.borrow());
        assert!(panic_observer_started.load(Ordering::Acquire));
        assert!(panic_observer_exited.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&state), 1);
        assert_eq!(Arc::strong_count(&admission), 1);
    }
}
