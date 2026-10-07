//! Island transports: how frames move between stages.
//!
//! A [`Transport`] opens [`Listener`]s and [`Link`]s that carry whole frames.
//! [`TcpTransport`] is the first real one (length-prefixed frames, Nagle
//! off); [`MemTransport`] connects threads of one process (tests);
//! [`ShapedTransport`] wraps any transport with an emulated wide-area link
//! (one-way delay, jitter, a bounded uplink) for benchmarks. RDMA or
//! Thunderbolt transports implement the same three traits.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Largest frame accepted (1 GiB).
pub const MAX_FRAME: usize = 1 << 30;

/// A bidirectional, ordered, reliable channel of whole frames.
pub trait Link: Send {
    fn send(&mut self, frame: &[u8]) -> io::Result<()>;
    fn recv(&mut self) -> io::Result<Vec<u8>>;
    /// `false` once the peer is known to be gone. A send-only link checks
    /// this before sending, so a frame is never written into a connection
    /// whose reader has already exited (and silently lost).
    fn alive(&mut self) -> bool {
        true
    }
}

/// Accepts links.
pub trait Listener: Send {
    fn accept(&mut self) -> io::Result<Box<dyn Link>>;
    /// The address peers connect to.
    fn address(&self) -> String;
}

/// Opens listeners and links by address.
pub trait Transport: Send + Sync {
    fn listen(&self, address: &str) -> io::Result<Box<dyn Listener>>;
    fn connect(&self, address: &str) -> io::Result<Box<dyn Link>>;
}

// ------------------------------------------------------------------- TCP --

/// TCP: `u32` little-endian length, then the frame.
#[derive(Debug, Default, Clone, Copy)]
pub struct TcpTransport;

pub struct TcpLink {
    stream: TcpStream,
}

impl TcpLink {
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self { stream })
    }
}

impl Link for TcpLink {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        if frame.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame too large",
            ));
        }
        let mut buf = Vec::with_capacity(4 + frame.len());
        buf.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        buf.extend_from_slice(frame);
        self.stream.write_all(&buf)
    }

    fn recv(&mut self) -> io::Result<Vec<u8>> {
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let mut frame = vec![0u8; len];
        self.stream.read_exact(&mut frame)?;
        Ok(frame)
    }

    fn alive(&mut self) -> bool {
        // The peer never writes on a send-only link: anything readable is
        // the end of the stream (or an error), i.e. the peer is gone.
        if self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let mut probe = [0u8; 1];
        let alive = match self.stream.peek(&mut probe) {
            Ok(0) => false,
            Ok(_) => true,
            Err(e) => e.kind() == io::ErrorKind::WouldBlock,
        };
        self.stream.set_nonblocking(false).is_ok() && alive
    }
}

pub struct TcpListenerBox {
    listener: TcpListener,
}

impl Listener for TcpListenerBox {
    fn accept(&mut self) -> io::Result<Box<dyn Link>> {
        let (stream, _) = self.listener.accept()?;
        Ok(Box::new(TcpLink::new(stream)?))
    }

    fn address(&self) -> String {
        self.listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default()
    }
}

impl Transport for TcpTransport {
    fn listen(&self, address: &str) -> io::Result<Box<dyn Listener>> {
        Ok(Box::new(TcpListenerBox {
            listener: TcpListener::bind(address)?,
        }))
    }

    fn connect(&self, address: &str) -> io::Result<Box<dyn Link>> {
        Ok(Box::new(TcpLink::new(TcpStream::connect(address)?)?))
    }
}

/// TCP with a connect timeout and one absolute budget per complete frame,
/// including its length prefix. Partial progress never renews that budget.
/// Endpoints are numeric socket addresses: DNS resolution happens in discovery.
#[derive(Debug, Clone, Copy)]
pub struct DeadlineTcpTransport {
    pub timeout: Duration,
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "whole-frame deadline exceeded"))
}

fn write_until(
    mut bytes: &[u8],
    deadline: Instant,
    mut write: impl FnMut(&[u8], Duration) -> io::Result<usize>,
) -> io::Result<()> {
    while !bytes.is_empty() {
        match write(bytes, remaining(deadline)?) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    remaining(deadline).map(|_| ())
}

fn read_until(stream: &mut TcpStream, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        match stream.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => bytes = &mut bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    remaining(deadline).map(|_| ())
}

struct DeadlineTcpLink {
    inner: TcpLink,
    timeout: Duration,
    closed: bool,
}

impl DeadlineTcpLink {
    fn new(stream: TcpStream, timeout: Duration) -> io::Result<Self> {
        validate_timeout(timeout)?;
        Ok(Self {
            inner: TcpLink::new(stream)?,
            timeout,
            closed: false,
        })
    }

    fn deadline(&self) -> io::Result<Instant> {
        if self.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        Instant::now()
            .checked_add(self.timeout)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame deadline overflow"))
    }

    fn finish<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if result.is_err() {
            self.closed = true;
            let _ = self.inner.stream.shutdown(std::net::Shutdown::Both);
        }
        result
    }
}

fn validate_timeout(timeout: Duration) -> io::Result<()> {
    if timeout.is_zero() || Instant::now().checked_add(timeout).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid frame timeout",
        ));
    }
    Ok(())
}

impl Link for DeadlineTcpLink {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        let result = (|| {
            let deadline = self.deadline()?;
            if frame.len() > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "frame too large",
                ));
            }
            for bytes in [&(frame.len() as u32).to_le_bytes()[..], frame] {
                write_until(bytes, deadline, |bytes, left| {
                    self.inner.stream.set_write_timeout(Some(left))?;
                    self.inner.stream.write(bytes)
                })?;
            }
            Ok(())
        })();
        self.finish(result)
    }

    fn recv(&mut self) -> io::Result<Vec<u8>> {
        let result = (|| {
            let deadline = self.deadline()?;
            let mut len = [0; 4];
            read_until(&mut self.inner.stream, &mut len, deadline)?;
            let len = u32::from_le_bytes(len) as usize;
            if len > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame too large",
                ));
            }
            let mut frame = vec![0; len];
            read_until(&mut self.inner.stream, &mut frame, deadline)?;
            Ok(frame)
        })();
        self.finish(result)
    }

    fn alive(&mut self) -> bool {
        if !self.closed && !self.inner.alive() {
            self.closed = true;
            let _ = self.inner.stream.shutdown(std::net::Shutdown::Both);
        }
        !self.closed
    }
}

struct DeadlineTcpListener {
    inner: TcpListenerBox,
    timeout: Duration,
}

impl Listener for DeadlineTcpListener {
    fn accept(&mut self) -> io::Result<Box<dyn Link>> {
        let (stream, _) = self.inner.listener.accept()?;
        Ok(Box::new(DeadlineTcpLink::new(stream, self.timeout)?))
    }
    fn address(&self) -> String {
        self.inner.address()
    }
}

impl Transport for DeadlineTcpTransport {
    fn listen(&self, address: &str) -> io::Result<Box<dyn Listener>> {
        validate_timeout(self.timeout)?;
        Ok(Box::new(DeadlineTcpListener {
            inner: TcpListenerBox {
                listener: TcpListener::bind(address)?,
            },
            timeout: self.timeout,
        }))
    }

    fn connect(&self, address: &str) -> io::Result<Box<dyn Link>> {
        validate_timeout(self.timeout)?;
        let address: SocketAddr = address.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("numeric endpoint: {e}"),
            )
        })?;
        // Connect retains its independent timeout. Each send/recv starts a new
        // frame budget; prefix and payload share it, including short syscalls.
        let stream = TcpStream::connect_timeout(&address, self.timeout)?;
        Ok(Box::new(DeadlineTcpLink::new(stream, self.timeout)?))
    }
}

// ---------------------------------------------------------------- memory --

type Incoming = Sender<MemLink>;

/// In-process links between threads, addressed by name.
#[derive(Default, Clone)]
pub struct MemTransport {
    endpoints: Arc<Mutex<HashMap<String, Incoming>>>,
}

pub struct MemLink {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    closed: Arc<AtomicBool>,
    peer_closed: Arc<AtomicBool>,
}

impl Drop for MemLink {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed")
}

impl Link for MemLink {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        if self.peer_closed.load(Ordering::SeqCst) {
            return Err(gone());
        }
        self.tx.send(frame.to_vec()).map_err(|_| gone())
    }

    fn recv(&mut self) -> io::Result<Vec<u8>> {
        self.rx.recv().map_err(|_| gone())
    }

    fn alive(&mut self) -> bool {
        !self.peer_closed.load(Ordering::SeqCst)
    }
}

pub struct MemListener {
    name: String,
    incoming: Receiver<MemLink>,
}

impl Listener for MemListener {
    fn accept(&mut self) -> io::Result<Box<dyn Link>> {
        self.incoming
            .recv()
            .map(|l| Box::new(l) as Box<dyn Link>)
            .map_err(|_| gone())
    }

    fn address(&self) -> String {
        self.name.clone()
    }
}

impl MemTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget a listener (its owner "crashed"): new connections fail until
    /// the name is listened on again.
    pub fn unlisten(&self, address: &str) {
        self.endpoints.lock().expect("endpoints").remove(address);
    }
}

impl Transport for MemTransport {
    fn listen(&self, address: &str) -> io::Result<Box<dyn Listener>> {
        let (tx, rx) = channel();
        // Listening again on a name replaces the old listener (a restart).
        self.endpoints
            .lock()
            .expect("endpoints")
            .insert(address.to_string(), tx);
        Ok(Box::new(MemListener {
            name: address.to_string(),
            incoming: rx,
        }))
    }

    fn connect(&self, address: &str) -> io::Result<Box<dyn Link>> {
        let endpoints = self.endpoints.lock().expect("endpoints");
        let incoming = endpoints.get(address).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("no listener at {address}"),
            )
        })?;
        let (a_tx, b_rx) = channel();
        let (b_tx, a_rx) = channel();
        let (a_closed, b_closed) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let remote = MemLink {
            tx: b_tx,
            rx: b_rx,
            closed: b_closed.clone(),
            peer_closed: a_closed.clone(),
        };
        incoming.send(remote).map_err(|_| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("{address} is gone"),
            )
        })?;
        Ok(Box::new(MemLink {
            tx: a_tx,
            rx: a_rx,
            closed: a_closed,
            peer_closed: b_closed,
        }))
    }
}

// ------------------------------------------------------- WAN emulation --

/// An emulated wide-area link, applied to every frame a shaped link sends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WanProfile {
    /// One-way propagation delay per hop, milliseconds.
    pub one_way_ms: f64,
    /// Uniform jitter in `[-jitter_ms, +jitter_ms]` (frames stay in order).
    pub jitter_ms: f64,
    /// Sender uplink in Mbit/s; 0 is unlimited. Frames queue behind each
    /// other on the uplink, so serialisation delay accumulates under load.
    pub uplink_mbit: f64,
    /// Seed of the jitter sequence.
    pub seed: u64,
}

impl WanProfile {
    /// Serialisation time of `bytes` on the uplink.
    pub fn serialisation(&self, bytes: usize) -> Duration {
        if self.uplink_mbit <= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(bytes as f64 * 8.0 / (self.uplink_mbit * 1e6))
        }
    }
}

/// How the delay line waits: `"sleep"` where the OS honours short sleeps,
/// `"spin"` where it does not. Some hosts stretch a 10 ms sleep to tens of
/// milliseconds (timer coalescing for background work), which would make
/// every emulated hop far slower than its profile; there the delay line
/// yields in a loop instead. Calibrated once per process.
pub fn timer_mode() -> &'static str {
    static MODE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    MODE.get_or_init(|| {
        let mut worst = Duration::ZERO;
        for _ in 0..5 {
            let start = Instant::now();
            std::thread::sleep(Duration::from_millis(1));
            worst = worst.max(start.elapsed());
        }
        if worst > Duration::from_micros(1_500) {
            "spin"
        } else {
            "sleep"
        }
    })
}

fn wait_until(at: Instant) {
    let spin = timer_mode() == "spin";
    loop {
        let now = Instant::now();
        if now >= at {
            return;
        }
        if spin {
            std::thread::yield_now();
        } else {
            std::thread::sleep(at - now);
        }
    }
}

/// Wraps a transport: links it connects send through a [`WanProfile`].
pub struct ShapedTransport {
    pub inner: Arc<dyn Transport>,
    pub profile: WanProfile,
}

impl Transport for ShapedTransport {
    fn listen(&self, address: &str) -> io::Result<Box<dyn Listener>> {
        self.inner.listen(address)
    }

    fn connect(&self, address: &str) -> io::Result<Box<dyn Link>> {
        let inner = self.inner.connect(address)?;
        let seed = self.profile.seed
            ^ u64::from_le_bytes(
                blake3::hash(address.as_bytes()).as_bytes()[..8]
                    .try_into()
                    .expect("8"),
            );
        Ok(Box::new(ShapedLink::new(inner, self.profile, seed)))
    }
}

/// Process-local emulated data-hop telemetry. Only Step/Tree frames count;
/// controls, startup and pings do not. Elapsed time includes uplink queueing,
/// propagation, host scheduling and the underlying send. It is measured,
/// not the configured delay multiplied by the number of hops.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct HopMetrics {
    pub frames: u64,
    pub bytes: u64,
    pub residence_seconds: f64,
}

static HOP_METRICS: Mutex<HopMetrics> = Mutex::new(HopMetrics {
    frames: 0,
    bytes: 0,
    residence_seconds: 0.0,
});

pub fn shaped_hop_metrics() -> HopMetrics {
    HOP_METRICS.lock().expect("hop metrics").clone()
}

/// A send-only link whose frames leave through an emulated uplink and
/// arrive after the propagation delay. The caller never waits: frames sit
/// in a delay line, as packets sit on a wire.
pub struct ShapedLink {
    line: Option<Sender<(Instant, Instant, Vec<u8>)>>,
    dead: Arc<AtomicBool>,
    profile: WanProfile,
    rng: u64,
    uplink_free: Instant,
    last_arrival: Instant,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ShapedLink {
    pub fn new(mut inner: Box<dyn Link>, profile: WanProfile, seed: u64) -> Self {
        let (tx, rx) = channel::<(Instant, Instant, Vec<u8>)>();
        let dead = Arc::new(AtomicBool::new(false));
        let flag = dead.clone();
        let worker = std::thread::spawn(move || {
            for (at, queued, frame) in rx {
                wait_until(at);
                if !inner.alive() || inner.send(&frame).is_err() {
                    flag.store(true, Ordering::SeqCst);
                    return;
                }
                // Kind bytes 1 and 7 are Step and Tree in wire.rs.
                if matches!(frame.first(), Some(1 | 7)) {
                    let mut stats = HOP_METRICS.lock().expect("hop metrics");
                    stats.frames += 1;
                    stats.bytes += frame.len() as u64;
                    stats.residence_seconds += queued.elapsed().as_secs_f64();
                }
            }
        });
        let now = Instant::now();
        Self {
            line: Some(tx),
            dead,
            profile,
            rng: seed | 1,
            uplink_free: now,
            last_arrival: now,
            worker: Some(worker),
        }
    }

    fn jitter(&mut self) -> f64 {
        // xorshift64*: a fixed jitter sequence per link and seed.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let unit =
            (self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64;
        (unit * 2.0 - 1.0) * self.profile.jitter_ms
    }
}

impl Link for ShapedLink {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(gone());
        }
        let now = Instant::now();
        let start = self.uplink_free.max(now);
        // The 4-byte length prefix and ~40 bytes of TCP/IP headers per
        // frame are ignored; frames here are kilobytes.
        self.uplink_free = start + self.profile.serialisation(frame.len());
        let delay = (self.profile.one_way_ms + self.jitter()).max(0.0);
        let arrival =
            (self.uplink_free + Duration::from_secs_f64(delay / 1e3)).max(self.last_arrival);
        self.last_arrival = arrival;
        self.line
            .as_ref()
            .expect("line")
            .send((arrival, now, frame.to_vec()))
            .map_err(|_| gone())
    }

    fn recv(&mut self) -> io::Result<Vec<u8>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a shaped link only sends",
        ))
    }

    fn alive(&mut self) -> bool {
        !self.dead.load(Ordering::SeqCst)
    }
}

impl Drop for ShapedLink {
    fn drop(&mut self) {
        // Deliver what is on the wire, then stop.
        self.line.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(transport: &dyn Transport, address: &str) {
        let mut listener = transport.listen(address).unwrap();
        let address = listener.address();
        let server = std::thread::spawn(move || {
            let mut link = listener.accept().unwrap();
            while let Ok(frame) = link.recv() {
                if frame.is_empty() {
                    break;
                }
                link.send(&frame).unwrap();
            }
        });
        let mut link = transport.connect(&address).unwrap();
        for n in [1usize, 1000, 300_000] {
            let frame: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
            link.send(&frame).unwrap();
            assert_eq!(link.recv().unwrap(), frame);
        }
        assert!(link.alive());
        link.send(&[]).unwrap();
        server.join().unwrap();
        // The server dropped its end: the link notices before sending.
        let deadline = Instant::now() + Duration::from_secs(5);
        while link.alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!link.alive());
    }

    #[test]
    fn tcp_and_memory_links_carry_frames_and_notice_a_dead_peer() {
        exchange(&TcpTransport, "127.0.0.1:0");
        exchange(&MemTransport::new(), "mem-a");
    }

    fn deadline_pair(accepted: bool, timeout: Duration) -> (Box<dyn Link>, TcpStream) {
        let transport = DeadlineTcpTransport { timeout };
        if accepted {
            let mut listener = transport.listen("127.0.0.1:0").unwrap();
            let peer = TcpStream::connect(listener.address()).unwrap();
            (listener.accept().unwrap(), peer)
        } else {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let link = transport
                .connect(&listener.local_addr().unwrap().to_string())
                .unwrap();
            (link, listener.accept().unwrap().0)
        }
    }

    fn assert_closed(link: &mut dyn Link) {
        assert!(!link.alive());
        assert_eq!(link.recv().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            link.send(b"later").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn deadline_links_accept_healthy_fragmented_frames_in_both_directions() {
        exchange(
            &DeadlineTcpTransport {
                timeout: Duration::from_secs(5),
            },
            "127.0.0.1:0",
        );
        for accepted in [false, true] {
            let (mut link, mut peer) = deadline_pair(accepted, Duration::from_secs(5));
            peer.set_nodelay(true).unwrap();
            let worker = std::thread::spawn(move || {
                for byte in 257u32.to_le_bytes() {
                    peer.write_all(&[byte]).unwrap();
                }
                for chunk in vec![42; 257].chunks(3) {
                    peer.write_all(chunk).unwrap();
                }
                let mut prefix = [0; 4];
                peer.read_exact(&mut prefix).unwrap();
                assert_eq!(u32::from_le_bytes(prefix), 257);
                let mut reply = vec![0; 257];
                peer.read_exact(&mut reply).unwrap();
                assert_eq!(reply, vec![42; 257]);
            });
            assert_eq!(link.recv().unwrap(), vec![42; 257]);
            link.send(&vec![42; 257]).unwrap();
            worker.join().unwrap();
        }
    }

    #[test]
    fn deadline_covers_trickling_prefix_and_payload_and_poisoned_links() {
        for accepted in [false, true] {
            for payload in [false, true] {
                let (mut link, mut peer) = deadline_pair(accepted, Duration::from_millis(150));
                peer.set_nodelay(true).unwrap();
                let sender = std::thread::spawn(move || {
                    let bytes = if payload {
                        peer.write_all(&100u32.to_le_bytes()).unwrap();
                        vec![42; 100]
                    } else {
                        100u32.to_le_bytes().to_vec()
                    };
                    for byte in bytes {
                        // Every syscall makes progress sooner than 150 ms, but
                        // the complete header/payload takes longer than it.
                        std::thread::sleep(Duration::from_millis(60));
                        if peer.write_all(&[byte]).is_err() {
                            break;
                        }
                    }
                });
                let start = Instant::now();
                let error = link.recv().unwrap_err();
                assert!(matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ));
                // Broad scheduler allowance, not a throughput threshold.
                assert!(start.elapsed() < Duration::from_secs(3));
                assert_closed(&mut *link);
                sender.join().unwrap();
            }
        }
    }

    #[test]
    fn short_writes_and_interruptions_share_one_absolute_budget() {
        let deadline = Instant::now() + Duration::from_millis(150);
        let mut calls = 0;
        let mut previous = Duration::MAX;
        let error = write_until(&[0; 100], deadline, |_, left| {
            assert!(left <= previous);
            previous = left;
            calls += 1;
            std::thread::sleep(Duration::from_millis(25));
            if calls % 2 == 0 {
                Err(io::ErrorKind::Interrupted.into())
            } else {
                Ok(1)
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(calls < 100);
    }

    #[test]
    fn slow_tcp_reader_cannot_extend_outbound_deadline() {
        for accepted in [false, true] {
            // Windows loopback may buffer a whole large write. Explicitly
            // constrain both ends before connecting; frame size alone is not
            // a portable way to establish backpressure.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let listener_ref = socket2::SockRef::from(&listener);
            listener_ref.set_send_buffer_size(4096).unwrap();
            listener_ref.set_recv_buffer_size(4096).unwrap();
            let socket =
                socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
            socket.set_send_buffer_size(4096).unwrap();
            socket.set_recv_buffer_size(4096).unwrap();
            socket
                .connect(&listener.local_addr().unwrap().into())
                .unwrap();
            let client: TcpStream = socket.into();
            let server = listener.accept().unwrap().0;
            let (stream, mut peer) = if accepted {
                (server, client)
            } else {
                (client, server)
            };
            let mut link = DeadlineTcpLink::new(stream, Duration::from_millis(150)).unwrap();
            let reader = std::thread::spawn(move || {
                peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                // Consume a little, then keep the socket open past the sender's
                // budget. The frame exceeds our explicitly bounded buffers.
                let until = Instant::now() + Duration::from_millis(500);
                while Instant::now() < until {
                    let mut byte = [0];
                    if peer.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            let start = Instant::now();
            let error = link.send(&vec![0; 32 << 20]).unwrap_err();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ));
            assert!(start.elapsed() < Duration::from_secs(3));
            assert_closed(&mut link);
            reader.join().unwrap();
        }
    }

    #[test]
    fn partial_eof_oversize_and_invalid_deadline_fail_closed() {
        for bytes in [
            vec![1, 0],
            vec![4, 0, 0, 0, 42],
            ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec(),
        ] {
            let (mut link, mut peer) = deadline_pair(true, Duration::from_secs(2));
            peer.write_all(&bytes).unwrap();
            peer.shutdown(std::net::Shutdown::Both).unwrap();
            assert!(link.recv().is_err());
            assert_closed(&mut *link);
        }
        let invalid = DeadlineTcpTransport {
            timeout: Duration::ZERO,
        };
        assert!(invalid.listen("127.0.0.1:0").is_err());
        assert!(invalid.connect("127.0.0.1:1").is_err());
        assert!(
            DeadlineTcpTransport {
                timeout: Duration::from_secs(1)
            }
            .connect("localhost:1")
            .is_err()
        );
    }

    #[test]
    fn a_shaped_link_delays_queues_and_keeps_order() {
        let mem = MemTransport::new();
        let mut listener = mem.listen("x").unwrap();
        let shaped = ShapedTransport {
            inner: Arc::new(mem.clone()),
            profile: WanProfile {
                one_way_ms: 20.0,
                jitter_ms: 5.0,
                uplink_mbit: 8.0,
                seed: 1,
            },
        };
        let mut link = shaped.connect("x").unwrap();
        let mut peer = listener.accept().unwrap();
        let start = Instant::now();
        // 10 frames of 10 kB on an 8 Mbit/s uplink: 10 ms each on the wire.
        for i in 0..10u8 {
            link.send(&vec![i; 10_000]).unwrap();
        }
        assert!(
            start.elapsed() < Duration::from_millis(15),
            "sending never waits"
        );
        let mut arrivals = Vec::new();
        for i in 0..10u8 {
            let frame = peer.recv().unwrap();
            assert_eq!(frame[0], i, "order");
            arrivals.push(start.elapsed().as_secs_f64() * 1e3);
        }
        // First frame: 10 ms on the uplink + 20 +- 5 ms; last: 100 ms + 20 +- 5.
        assert!(arrivals[0] >= 24.0, "{arrivals:?}");
        assert!(arrivals[9] >= 114.0, "{arrivals:?}");
        assert!(arrivals[9] < 400.0, "{arrivals:?}");
    }
}
