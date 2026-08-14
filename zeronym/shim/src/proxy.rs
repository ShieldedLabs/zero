//! The transparent HTTP/2 (h2c) reverse proxy.
//!
//! Every request is forwarded to the backing indexer with its method, path,
//! headers and streaming body intact, and the response is streamed back with
//! its status, headers, body and TRAILERS intact. Trailers are the part that is
//! easy to lose and expensive to lose: gRPC carries `grpc-status` in the
//! trailers, so a proxy that drops them turns every successful call into
//! "server closed the stream without sending a status".
//!
//! Two rules keep that property, and they are the review checklist for this
//! file:
//!
//! 1. On the pass-through path, bodies are only ever re-wrapped, never read.
//!    `Response::map` rewraps the body value without polling it, and `map_err`
//!    and `boxed` are frame-preserving adapters, so DATA frames stay lazy
//!    (streaming survives) and the trailers frame survives with them.
//! 2. No `collect()` and no `Full::new()` anywhere outside
//!    [`crate::intercept`]. Either one silently converts a stream into a buffer
//!    AND discards the trailers.
//!
//! The single intercepted path is `SendTransaction`, handled by
//! [`crate::intercept`]. Both paths egress through the same [`forward`], so the
//! intercept path cannot drift away from the pass-through path.
//!
//! The third rule, and the one this file got wrong once already:
//!
//! 3. **The interception set must be a SUPERSET of every routing predicate any
//!    supported backend uses, never a subset.** A predicate narrower than the
//!    backend's fails OPEN: the backend acts on a request the classifier never
//!    saw. Concretely, the vendored tonic server Zaino is built from dispatches
//!    on `req.uri().path()` alone, with no HTTP-method guard
//!    (`zaino/packages/zaino-proto/src/proto/service.rs:1384`), so a `GET` to
//!    the `SendTransaction` path reaches its `send_transaction` handler. An
//!    earlier version of [`route_for`] also required `POST`, which made exactly
//!    that request bypass the classifier. See [`Route`].
//!
//! Transport is plaintext h2c with prior knowledge: no TLS, no HTTP/1.1, no
//! upgrade dance. `curl http://...` will look broken; `grpcurl -plaintext` and
//! tonic channels over `http://` both work, because both use prior knowledge.

use std::convert::Infallible;
use std::future::Future;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{HeaderMap, HeaderValue, Request, Response, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::client::conn::http1 as client_h1;
use hyper::client::conn::http2 as client_h2;
use hyper::server::conn::http2 as server_h2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};

use crate::intercept;
use crate::tls::{BackendTls, ServerTls};
use crate::BoxError;

/// One body type for both legs and both paths, so [`forward`] is shared
/// verbatim between the pass-through and the intercept.
pub type ProxyBody = BoxBody<Bytes, BoxError>;

/// The one method the shim decodes. Everything else is opaque.
pub const SEND_TRANSACTION: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction";

/// The method a wallet calls to fetch one transaction by txid. When it names a
/// diverted migration, forwarding it hands the operator the exact txid the hub
/// and the diversion removed from the link, so it is intercepted and answered
/// from the bytes the shim holds. See `crate::intercept::get_transaction`.
pub const GET_TRANSACTION: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTransaction";

/// Caution platform control-plane paths, served on the SAME host the shim serves.
/// Normally the in-enclave proxy answers these before our process, but under h2c
/// the platform routes them to the app, so we own them defensively: a transparent
/// proxy that forwarded Caution's own endpoints to the Zcash indexer failed the
/// attestation health check (the h2c blocker, `a6063ef`). `/.well-known/caution/health`
/// is answered locally; the attestation POST is relayed to bootproofd, the
/// platform's real NSM source. Whether the shim owns these at all, and where the
/// relay dials, are governed by [`CautionRelay`] — off makes the shim a pure proxy.
pub const CAUTION_HEALTH: &str = "/.well-known/caution/health";
pub const CAUTION_ATTESTATION: &str = "/attestation";

/// Whether, and how, the shim owns Caution's in-enclave control-plane paths
/// ([`CAUTION_HEALTH`], [`CAUTION_ATTESTATION`]).
///
/// This is a workaround for the platform routing `/attestation` to the app under
/// h2c; it is scoped behind a flag so it can be turned off for BYOC or non-h2c
/// deployments, or removed entirely once Caution serves these paths itself. When
/// `enabled` is false, both paths route as [`Route::PassThrough`] and the
/// `bootproofd_addr` is never dialled. `bootproofd_addr` is configurable so the
/// platform's internal port is not hardcoded here.
#[derive(Debug, Clone)]
pub struct CautionRelay {
    pub enabled: bool,
    pub bootproofd_addr: Arc<str>,
}

impl Default for CautionRelay {
    /// On by default, matching the managed-Caution-under-h2c deployment where the
    /// shim MUST answer these paths to boot. Off-Caution the paths are simply
    /// never requested, so owning them is harmless.
    fn default() -> Self {
        CautionRelay {
            enabled: true,
            bootproofd_addr: Arc::from(crate::config::DEFAULT_BOOTPROOFD_ADDR),
        }
    }
}

/// gRPC status code 14, UNAVAILABLE.
pub const GRPC_UNAVAILABLE: u16 = 14;

/// gRPC status code 8, RESOURCE_EXHAUSTED.
pub const GRPC_RESOURCE_EXHAUSTED: u16 = 8;

/// gRPC status code 1, CANCELLED.
pub const GRPC_CANCELLED: u16 = 1;

/// gRPC status code 5, NOT_FOUND. What a wallet gets for a txid the hub's lookup
/// does not know, mirroring lightwalletd's answer for an unknown transaction.
pub const GRPC_NOT_FOUND: u16 = 5;

/// gRPC status code 3, INVALID_ARGUMENT. A malformed or empty `TxFilter`: caught
/// locally so a bad filter never becomes a hub round trip or a dialled operator.
pub const GRPC_INVALID_ARGUMENT: u16 = 3;

/// Per-stream HTTP/2 flow-control window, both legs.
///
/// The h2 default is 64 KiB, which throttles `GetBlockRange` to a crawl through
/// a proxy and reads as a shim performance bug rather than a config default.
const STREAM_WINDOW: u32 = 2 * 1024 * 1024;

/// Per-connection HTTP/2 flow-control window, both legs.
const CONNECTION_WINDOW: u32 = 8 * 1024 * 1024;

/// How long a graceful shutdown waits for in-flight connections to finish.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one inbound connection waits before dialling the backing indexer
/// again after a failed dial, so a hard-down indexer cannot turn every request
/// into a fresh connect syscall.
const DIAL_BACKOFF: Duration = Duration::from_millis(100);

/// How long a dial to the backing indexer may take before it is a failure.
/// [`UpstreamPool::get`] holds its lock across the dial, so an indexer that
/// blackholes SYNs would otherwise stall every request on that client
/// connection with no error and no timeout of its own.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the accept loop pauses after a file-descriptor exhaustion error, so
/// it does not spin at full tilt while the process is out of descriptors.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// A pooled HTTP/2 connection to the backing indexer.
///
/// One of these is established per inbound client connection; requests on it
/// are multiplexed by cloning the cheap [`client_h2::SendRequest`] handle.
#[derive(Clone)]
pub struct Upstream {
    sender: client_h2::SendRequest<ProxyBody>,
    authority: Authority,
}

/// Where the backing indexer is, and how to authenticate it.
///
/// The address and the verified name are separate on purpose. The enclave dials
/// a literal address and never resolves DNS, so a poisoned answer cannot
/// redirect it; the TLS name is what the certificate must actually say. See
/// `crate::tls`.
#[derive(Clone)]
pub struct Backend {
    pub addr: SocketAddr,
    pub tls: Option<BackendTls>,
}

impl From<SocketAddr> for Backend {
    /// Plaintext h2c, which is what the tests and a local demo use.
    fn from(addr: SocketAddr) -> Self {
        Backend { addr, tls: None }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.tls {
            Some(_) => write!(f, "{} (tls)", self.addr),
            None => write!(f, "{} (plaintext)", self.addr),
        }
    }
}

/// Drive an upstream connection to completion in the background.
///
/// This spawn is mandatory. The connection future is what moves bytes on the
/// socket; without it every request hangs forever with no error, which looks
/// exactly like a proxy deadlock. Generic because the TLS and plaintext
/// handshakes produce different connection types.
fn spawn_connection_driver<C, E>(conn: C)
where
    C: Future<Output = Result<(), E>> + Send + 'static,
    E: std::fmt::Display,
{
    tokio::spawn(async move {
        if let Err(err) = conn.await {
            tracing::debug!(%err, "backing indexer connection closed");
        }
    });
}

impl Upstream {
    /// Dial the backing indexer and spawn its connection task.
    pub async fn connect(backend: &Backend) -> Result<Self, BoxError> {
        let stream = TcpStream::connect(backend.addr).await?;
        stream.set_nodelay(true)?;

        // The handshake is the same in both arms, but its connection future is
        // a different concrete type per stream type, so the arms cannot be
        // unified into one value. Each spawns its own driver and yields only
        // the sender, which IS the same type either way; past this point
        // nothing knows whether the hop is encrypted.
        let sender = match &backend.tls {
            Some(tls) => {
                let stream = tls.connect(backend.addr, stream).await?;
                let (sender, conn) = client_h2::Builder::new(TokioExecutor::new())
                    .initial_stream_window_size(STREAM_WINDOW)
                    .initial_connection_window_size(CONNECTION_WINDOW)
                    .handshake(TokioIo::new(stream))
                    .await?;
                spawn_connection_driver(conn);
                sender
            }
            None => {
                let (sender, conn) = client_h2::Builder::new(TokioExecutor::new())
                    .initial_stream_window_size(STREAM_WINDOW)
                    .initial_connection_window_size(CONNECTION_WINDOW)
                    .handshake(TokioIo::new(stream))
                    .await?;
                spawn_connection_driver(conn);
                sender
            }
        };

        // The :authority must be the TLS name when there is one, not the
        // address. Found by testing rather than by reading: with the address
        // here, a host-routing backend (our Traefik ingress) matched no rule
        // and answered 404, because ":authority: 66.42.124.202:443" is not
        // "lwd.shieldedinfra.net". TLS had succeeded; the request was simply
        // addressed to nobody. Plaintext backends keep the address, which is
        // what a directly-dialled indexer expects.
        let authority = match &backend.tls {
            Some(tls) => tls.authority(backend.addr.port()),
            None => backend.addr.to_string(),
        };

        Ok(Upstream {
            sender,
            authority: authority.parse()?,
        })
    }

    /// Whether the underlying connection is gone. [`UpstreamPool::get`] redials
    /// on this.
    ///
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

/// The backing-indexer connection belonging to one inbound client connection.
///
/// Two properties, both of which used to be missing:
///
/// * **Lazy.** The indexer is dialled on the first request that needs it, not
///   on TCP accept. A port scan or a health probe that never sends a request
///   therefore costs no upstream socket, and the shim stops doubling its file
///   descriptor consumption on connections that never speak. (In production,
///   where a migration is DIVERTED, dialling on accept would also hand the
///   operator a connection-level trace of a wallet that was never going to talk
///   to them. Classifying before any upstream connection exists is the
///   direction this wants to go.)
/// * **Redialled.** When the backing indexer restarts, the cached connection is
///   dead. Without a redial the shim answers UNAVAILABLE forever on a perfectly
///   healthy HTTP/2 connection to the wallet, and because that is a clean
///   application-level status rather than a transport error, the wallet's own
///   reconnect logic never fires and it stays stuck.
pub(crate) struct UpstreamPool {
    backend: Backend,
    state: tokio::sync::Mutex<PoolState>,
}

#[derive(Default)]
struct PoolState {
    live: Option<Upstream>,
    /// Set only after a FAILED dial. A successful dial clears it, so a healthy
    /// indexer is never delayed.
    backoff_until: Option<Instant>,
}

impl UpstreamPool {
    fn new(backend: Backend) -> Self {
        UpstreamPool {
            backend,
            state: tokio::sync::Mutex::new(PoolState::default()),
        }
    }

    /// A live upstream, dialling or redialling if needed.
    ///
    /// The lock is held across the dial on purpose: it is what stops the
    /// requests multiplexed on one client connection from opening a fistful of
    /// upstream connections the moment the indexer comes back.
    ///
    /// Reachable from [`crate::intercept`] because the dial happens there now,
    /// not in [`handle`]: a `SendTransaction` bound for the hub must reach a
    /// verdict before any connection to the operator's indexer exists, so the
    /// intercept path holds the pool and dials only on a pass-through verdict.
    pub(crate) async fn get(&self) -> Result<Upstream, BoxError> {
        let mut state = self.state.lock().await;

        if let Some(upstream) = state.live.take() {
            if !upstream.is_closed() {
                state.live = Some(upstream.clone());
                return Ok(upstream);
            }
            tracing::debug!(
                backend = %self.backend.addr,
                "backing indexer connection is gone, redialling"
            );
        }

        if let Some(until) = state.backoff_until {
            if Instant::now() < until {
                return Err("backing indexer unreachable (dial backoff)".into());
            }
        }

        let dialled = match tokio::time::timeout(DIAL_TIMEOUT, Upstream::connect(&self.backend))
            .await
        {
            Ok(dialled) => dialled,
            Err(_) => {
                Err(format!("dialling the backing indexer timed out after {DIAL_TIMEOUT:?}").into())
            }
        };

        match dialled {
            Ok(upstream) => {
                state.backoff_until = None;
                state.live = Some(upstream.clone());
                Ok(upstream)
            }
            Err(err) => {
                state.backoff_until = Some(Instant::now() + DIAL_BACKOFF);
                Err(err)
            }
        }
    }
}

/// Serve until the listener errors. Equivalent to [`serve_with_shutdown`] with
/// a shutdown signal that never fires.
pub async fn serve(listener: TcpListener, backend: impl Into<Backend>) -> Result<(), BoxError> {
    serve_with_shutdown(
        listener,
        backend,
        None,
        None,
        CautionRelay::default(),
        std::future::pending::<()>(),
    )
    .await
}

/// Serve until `shutdown` resolves, then stop accepting and drain.
///
/// The listener is bound by the caller so a bind failure (EADDRINUSE, EACCES)
/// surfaces as a clean startup error instead of dying inside a spawned task,
/// and so tests can bind port 0.
/// `tls` terminates the wallet-facing link. When it is `None` the shim serves
/// plaintext h2c; there is deliberately no fallback in the other direction,
/// because a TLS listener that quietly downgraded on handshake failure would
/// serve wallet traffic in the clear while looking healthy.
pub async fn serve_with_shutdown<S>(
    listener: TcpListener,
    backend: impl Into<Backend>,
    tls: Option<Arc<ServerTls>>,
    diversion: Option<Arc<crate::intercept::Diversion>>,
    caution: CautionRelay,
    shutdown: S,
) -> Result<(), BoxError>
where
    S: Future<Output = ()>,
{
    let backend = backend.into();
    // Connection tracker: every connection task holds a clone of the sender and
    // nothing is ever sent, so `recv()` resolves to `None` exactly when the last
    // connection has finished.
    let (live_tx, mut live_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                // A transient accept() error must not take the whole proxy
                // down. ECONNABORTED (the peer went away between the SYN and
                // the accept), EINTR, and descriptor exhaustion are all
                // reachable on a public listener, and none of them says the
                // listener is unusable.
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(err) if is_transient_accept_error(&err) => {
                        tracing::debug!(%err, "transient accept error, continuing");
                        continue;
                    }
                    Err(err) if is_fd_exhaustion(&err) => {
                        tracing::warn!(%err, "out of file descriptors, pausing the accept loop");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                };
                let live = live_tx.clone();
                let backend = backend.clone();
                let tls = tls.clone();
                let diversion = diversion.clone();
                let caution = caution.clone();
                tokio::spawn(async move {
                    let _live = live;
                    match tls {
                        None => serve_connection(stream, peer, backend, diversion, caution).await,
                        Some(tls) => match tls.accept(stream).await {
                            // A TLS-ALPN-01 validation, already answered and
                            // closed by the acceptor. Not wallet traffic.
                            Ok(None) => {}
                            Ok(Some(stream)) => {
                                serve_connection(stream, peer, backend, diversion, caution).await
                            }
                            // Handshake failures are ordinary on a public
                            // listener (scanners, a wallet that gave up, or a
                            // certificate that has not been issued yet) and
                            // must not be retried in the clear.
                            Err(err) => {
                                tracing::debug!(%peer, %err, "tls handshake failed");
                            }
                        },
                    }
                });
            }
        }
    }

    drop(live_tx);
    tracing::info!("shutdown requested, draining in-flight connections");
    match tokio::time::timeout(DRAIN_TIMEOUT, live_rx.recv()).await {
        Ok(_) => tracing::info!("drained, exiting"),
        Err(_) => tracing::warn!(
            timeout_secs = DRAIN_TIMEOUT.as_secs(),
            "drain timed out, exiting with connections still open"
        ),
    }
    Ok(())
}

/// A client that vanished between the SYN and the accept, or a signal that
/// interrupted the syscall. Neither says anything about the listener.
fn is_transient_accept_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted
    )
}

/// EMFILE (24, this process is out of descriptors) and ENFILE (23, the system
/// is). Both are transient and both are self-inflicted denial of service if the
/// accept loop either dies or spins on them. `std::io::ErrorKind` has no stable
/// variant for either, and the raw numbers agree on Linux and macOS.
fn is_fd_exhaustion(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(23) | Some(24))
}

/// Serve one inbound client connection.
async fn serve_connection<IO>(
    stream: IO,
    peer: SocketAddr,
    backend: Backend,
    diversion: Option<Arc<intercept::Diversion>>,
    caution: CautionRelay,
) where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // set_nodelay moved to the accept site: once the stream may be a TLS
    // wrapper there is no TcpStream here to set it on.

    // One upstream connection per inbound connection, dialled lazily and
    // redialled when it dies. If the backing indexer is down we still serve the
    // client, answering UNAVAILABLE, which is a far better failure mode than
    // dropping the TCP connection on the floor.
    let pool = Arc::new(UpstreamPool::new(backend));

    let service = service_fn(move |req: Request<Incoming>| {
        let pool = pool.clone();
        let diversion = diversion.clone();
        let caution = caution.clone();
        async move { handle(req, pool, diversion, caution).await }
    });

    if let Err(err) = server_h2::Builder::new(TokioExecutor::new())
        .initial_stream_window_size(STREAM_WINDOW)
        .initial_connection_window_size(CONNECTION_WINDOW)
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        tracing::debug!(%peer, %err, "client connection ended");
    }
}

/// Route one request, converting any internal failure into a gRPC status so the
/// client sees a clean error instead of a reset stream.
async fn handle(
    req: Request<Incoming>,
    pool: Arc<UpstreamPool>,
    diversion: Option<Arc<intercept::Diversion>>,
    caution: CautionRelay,
) -> Result<Response<ProxyBody>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    // Classify BEFORE connecting. `route_for` is a pure function of the path, so
    // a request bound for the intercept path reaches `send_transaction` with no
    // upstream connection in existence, and only a pass-through verdict dials the
    // operator's indexer. A diverted migration therefore never opens even a TCP
    // connection to the operator: the reason the pool is lazy and the dial lives
    // here rather than at accept time.
    let result = match route_for(&path) {
        // Caution's control-plane paths. Owned here (NEVER proxied to the indexer:
        // health answered locally, attestation relayed to bootproofd) ONLY when
        // the relay is enabled; see `CautionRelay`. With it disabled the shim is a
        // pure proxy and these fall through to `pass_through` below.
        Route::CautionHealth if caution.enabled => Ok(caution_health_ok()),
        Route::CautionAttestation if caution.enabled => {
            forward_to_bootproofd(req, &caution.bootproofd_addr).await
        }
        Route::PassThrough | Route::CautionHealth | Route::CautionAttestation => {
            pass_through(req, pool).await
        }
        // GetTransaction may name a diverted migration; the interceptor decides,
        // and forwards (dialling the operator) only when it does not.
        Route::GetTransaction => intercept::get_transaction(req, pool, diversion).await,
        route @ (Route::Intercept | Route::InterceptNearMiss) => {
            if route == Route::InterceptNearMiss {
                tracing::warn!(
                    target: "zis::classify",
                    %method,
                    path = %path,
                    "path is not the SendTransaction method but spells it: classifying anyway"
                );
            }
            intercept::send_transaction(req, pool, diversion).await
        }
    };

    match result {
        Ok(resp) => Ok(resp),
        Err(err) => {
            tracing::warn!(%method, %path, %err, "proxying failed");
            Ok(grpc_error(
                GRPC_UNAVAILABLE,
                &format!("zero-indexer-shim: {err}"),
            ))
        }
    }
}

/// Where one request goes.
///
/// The decision is a pure function of the request PATH, and deliberately cannot
/// see the HTTP method: the backends this shim fronts do not look at the method
/// either, and a predicate narrower than the backend's fails open (see rule 3
/// in the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Exactly [`SEND_TRANSACTION`]: decode, classify, log, forward.
    Intercept,
    /// Not [`SEND_TRANSACTION`], but the final path segment spells
    /// `sendtransaction` in some other case. No backend we have checked routes
    /// these (tonic's dispatch is an exact string match, and so is
    /// lightwalletd's), so this arm should be dead. It exists because the two
    /// mistakes are not symmetric: classifying a request the backend would have
    /// rejected costs one log line, while NOT classifying one the backend
    /// accepts is the privacy leak this component exists to prevent.
    InterceptNearMiss,
    /// Exactly [`GET_TRANSACTION`]: buffer the `TxFilter`, and if it names a
    /// diverted migration serve it from held bytes; otherwise forward.
    GetTransaction,
    /// Caution's [`CAUTION_HEALTH`]. When the relay is enabled ([`CautionRelay`]),
    /// answered locally with HTTP 200 and never proxied to the indexer; when it is
    /// disabled the shim is a pure proxy and this falls through to `PassThrough`.
    CautionHealth,
    /// Caution's [`CAUTION_ATTESTATION`]. When the relay is enabled, relayed to
    /// bootproofd (the platform's NSM source) and never proxied to the indexer;
    /// when it is disabled this falls through to `PassThrough`.
    CautionAttestation,
    /// Opaque. Relayed without being read.
    PassThrough,
}

/// The whole routing predicate, as a pure function so it can be audited and
/// tested without a socket.
///
/// Known limit, deliberately not papered over: the comparison is on the path as
/// received, so a backend that percent-DECODED before matching (`%53end...`)
/// would route a request this returns [`Route::PassThrough`] for. tonic does
/// not, which is why there is no decoder here; a backend that does would need
/// one adding.
pub fn route_for(path: &str) -> Route {
    if path == SEND_TRANSACTION {
        return Route::Intercept;
    }
    if path == GET_TRANSACTION {
        return Route::GetTransaction;
    }
    // Caution's own endpoints, served on our host: never hand them to the indexer.
    if path == CAUTION_HEALTH {
        return Route::CautionHealth;
    }
    if path == CAUTION_ATTESTATION {
        return Route::CautionAttestation;
    }

    // Trailing slashes are tolerated here, not because tonic accepts them (it
    // answers UNIMPLEMENTED), but because normalizing them is one line and
    // guessing wrong about a future backend is not recoverable. ANY number of
    // them: stripping only one left `SendTransaction//` falling out of this arm
    // into PassThrough, which is the fail-OPEN direction rule 3 forbids, in the
    // one arm that exists to absorb exactly this class of mistake.
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some(last) if last.eq_ignore_ascii_case("sendtransaction") => Route::InterceptNearMiss,
        _ => Route::PassThrough,
    }
}

/// Forward a request the shim does not decode: every method except
/// `SendTransaction`, including streams, unknown methods and other services.
pub(crate) async fn pass_through(
    req: Request<Incoming>,
    pool: Arc<UpstreamPool>,
) -> Result<Response<ProxyBody>, BoxError> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    // Dial the operator HERE, not in `handle`: this path is the one meant to
    // reach the operator, so connecting on it is correct. The classify-before-
    // connect property lives in the intercept paths, which never call this.
    let upstream = pool.get().await?;

    // `map` rewraps the body value without polling it, so a client-streaming
    // request body is relayed frame by frame and is never buffered here.
    let req = req.map(|body| body.map_err(BoxError::from).boxed());
    let resp = forward(upstream, req).await?;

    // A trailers-only gRPC response carries its status in the response HEADERS
    // with no body at all. Copying the head verbatim handles that for free;
    // this only reads the status out for the log line.
    let grpc_status = resp
        .headers()
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // DEBUG, not INFO, and that is a privacy decision rather than a noise one.
    // A per-request line naming the method a wallet called is exactly the
    // metadata this component exists to deny the operator, and it would be
    // sitting in a log file on the operator's box. `RUST_LOG=zis::proxy=debug`
    // turns it on for a demo or a debugging session; nothing turns it on by
    // default. The classifier's own `zis::classify` lines stay at INFO, because
    // in this proof of concept they are the only visible output.
    tracing::debug!(
        target: "zis::proxy",
        %method,
        %path,
        status = resp.status().as_u16(),
        grpc_status = grpc_status.as_deref().unwrap_or("(in trailers)"),
        "proxied"
    );

    // Same `map` discipline on the way back: this is what keeps GetBlockRange
    // streaming and what carries the grpc-status trailer to the client.
    Ok(resp.map(|body| body.map_err(BoxError::from).boxed()))
}

/// The single egress point to the backing indexer, shared by the pass-through
/// path and by the intercept path after inspection.
///
/// Only the origin is retargeted. `:method`, `:path` and every header stay
/// byte-identical, which is why unknown and future CompactTxStreamer methods
/// keep working without a shim release.
pub async fn forward(
    mut upstream: Upstream,
    req: Request<ProxyBody>,
) -> Result<Response<Incoming>, BoxError> {
    let (mut parts, body) = req.into_parts();

    // h2 emits the `:scheme` and `:authority` pseudo-headers only if the URI
    // carries them, so both must be set explicitly. A path-only URI produces a
    // malformed request whose failure reads as a protocol error rather than as
    // a missing header.
    let mut uri_parts = std::mem::take(&mut parts.uri).into_parts();
    uri_parts.scheme = Some(Scheme::HTTP);
    uri_parts.authority = Some(upstream.authority.clone());
    if uri_parts.path_and_query.is_none() {
        uri_parts.path_and_query = Some(PathAndQuery::from_static("/"));
    }
    parts.uri = Uri::from_parts(uri_parts)?;

    // Headers pass through untouched: content-type, `te: trailers`,
    // grpc-timeout, grpc-encoding, user-agent, authorization and any custom or
    // `-bin` metadata. hyper itself strips only the headers HTTP/2 forbids, and
    // it deliberately preserves `te: trailers`. The one removal is Host, which
    // would now contradict the rewritten `:authority`.
    parts.headers.remove(http::header::HOST);

    upstream.sender.ready().await?;
    let mut resp = upstream
        .sender
        .send_request(Request::from_parts(parts, body))
        .await?;
    normalize_response_encoding(resp.headers_mut());
    Ok(resp)
}

/// The one response header the shim cannot afford to relay verbatim.
///
/// `grpc-accept-encoding` on a RESPONSE is the server telling the client which
/// message encodings it will accept on future REQUESTS. If the operator's
/// indexer advertises `gzip` there, wallets start compressing `SendTransaction`
/// bodies, the shim can no longer decode any of them, and every send lands in
/// the compression fail-safe: the classifier stops discriminating at all. In a
/// component whose threat model is "the operator is the adversary" that is an
/// operator-controlled lever on the classifier, so it is closed here, at the
/// single ingress point from the backing indexer.
///
/// This does NOT touch response compression, which is `grpc-encoding` and is
/// relayed untouched, and it does not touch the request direction, where
/// `grpc-accept-encoding` is the wallet's own statement about what it accepts.
/// The header is only rewritten when the indexer actually sent one; absent, the
/// gRPC default is already identity.
fn normalize_response_encoding(headers: &mut HeaderMap) {
    if headers.contains_key("grpc-accept-encoding") {
        headers.insert("grpc-accept-encoding", HeaderValue::from_static("identity"));
    }
}

/// A trailers-only gRPC error response: HTTP 200 with the status in the header
/// map and no message body, which is exactly the shape gRPC specifies for a
/// call that fails before producing a message.
pub(crate) fn grpc_error(code: u16, message: &str) -> Response<ProxyBody> {
    let body = Empty::<Bytes>::new().map_err(BoxError::from).boxed();
    let mut resp = Response::new(body);

    // gRPC failures are HTTP 200. Never map a gRPC error onto an HTTP status.
    let headers = resp.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    headers.insert(
        "grpc-status",
        HeaderValue::from_str(&code.to_string()).expect("status code is ASCII digits"),
    );
    if let Ok(value) = HeaderValue::from_str(&sanitize_grpc_message(message)) {
        headers.insert("grpc-message", value);
    }
    resp
}

/// `grpc-message` is percent-encoded UTF-8 on the wire. The shim only ever
/// produces its own ASCII messages, so rather than pull in a percent-encoder,
/// anything outside printable ASCII is replaced.
fn sanitize_grpc_message(message: &str) -> String {
    message
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '.'
            }
        })
        .collect()
}

/// Caution's `/.well-known/caution/health`, answered locally with HTTP 200 so it
/// is never proxied to the indexer. The platform normally serves this itself; we
/// own it defensively (and in case the platform routes it to us under h2c).
fn caution_health_ok() -> Response<ProxyBody> {
    let body = Empty::<Bytes>::new().map_err(BoxError::from).boxed();
    Response::builder()
        .status(200)
        .body(body)
        .expect("static health response builds")
}

/// Relay Caution's `/attestation` POST to bootproofd at `addr` over the enclave
/// loopback, so the attestation stays genuine (bootproofd is the platform's NSM
/// source) rather than being handed to the Zcash indexer. bootproofd speaks
/// HTTP/1.1. Reached only when [`CautionRelay::enabled`] is set; `addr` is that
/// relay's `bootproofd_addr`.
///
/// WORKAROUND: this exists because the platform routes `/attestation` to the app
/// under h2c. If Caution serves it itself, disable the relay and remove this.
async fn forward_to_bootproofd(
    req: Request<Incoming>,
    addr: &str,
) -> Result<Response<ProxyBody>, BoxError> {
    let stream = TcpStream::connect(addr).await?;
    let (mut sender, conn) = client_h1::handshake(TokioIo::new(stream)).await?;
    spawn_connection_driver(conn);

    let (mut parts, body) = req.into_parts();
    // HTTP/1.1 origin-form: the request target is the path only, plus a Host
    // header. The inbound h2 request carries scheme+authority pseudo-headers that
    // would make it absolute-form, so reduce the URI to its path-and-query.
    let path_and_query = parts
        .uri
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| PathAndQuery::from_static("/attestation"));
    parts.uri = Uri::from(path_and_query);
    parts.headers.remove(http::header::HOST);
    parts
        .headers
        .insert(http::header::HOST, HeaderValue::from_str(addr)?);

    let body = body.map_err(BoxError::from).boxed();
    let resp = sender.send_request(Request::from_parts(parts, body)).await?;
    Ok(resp.map(|body| body.map_err(BoxError::from).boxed()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_transaction_path_matches_the_generated_client() {
        // The literal the vendored tonic client sends
        // (zaino/packages/zaino-proto/src/proto/service.rs:678). If these ever
        // disagree the shim silently stops intercepting.
        assert_eq!(
            SEND_TRANSACTION,
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction"
        );
    }

    #[test]
    fn grpc_error_is_trailers_only_with_http_200() {
        let resp = grpc_error(GRPC_UNAVAILABLE, "backing indexer unreachable");
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("grpc-status").unwrap(), "14");
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/grpc"
        );
    }

    #[test]
    fn the_intercepted_path_is_routed_on_path_alone() {
        // The regression this pins: an earlier version required POST, while the
        // tonic server Zaino is built from dispatches on path alone. A GET to
        // this path reaches the indexer's send_transaction handler, so it must
        // reach the classifier too.
        //
        // `route_for` takes only a path, so the property is structural: the
        // routing predicate cannot see the HTTP method even if someone wants it
        // to.
        assert_eq!(route_for(SEND_TRANSACTION), Route::Intercept);
    }

    #[test]
    fn near_miss_paths_fail_safe_toward_the_classifier() {
        for path in [
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/sendtransaction",
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SENDTRANSACTION",
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction/",
            // Two trailing slashes, and any number: normalizing only one used to
            // drop this into PassThrough, unclassified.
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction//",
            "/some.other.Service/SendTransaction",
        ] {
            assert_eq!(
                route_for(path),
                Route::InterceptNearMiss,
                "path {path} must not be handed to the indexer unclassified"
            );
        }
    }

    #[test]
    fn everything_else_passes_through() {
        for path in [
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange",
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/NoSuchMethodYet",
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransactionButNotReally",
            "/",
            "",
        ] {
            assert_eq!(route_for(path), Route::PassThrough, "path {path}");
        }
    }

    #[test]
    fn transient_accept_errors_are_not_fatal() {
        use std::io::Error;

        // A client that vanished between the SYN and the accept, and a signal
        // that interrupted the syscall. Neither says the listener is unusable,
        // and treating either as fatal kills the whole proxy.
        for kind in [
            ErrorKind::ConnectionAborted,
            ErrorKind::ConnectionReset,
            ErrorKind::Interrupted,
        ] {
            assert!(is_transient_accept_error(&Error::new(kind, "transient")));
        }

        // EMFILE and ENFILE: transient too, but they need a pause rather than
        // an immediate retry, or the accept loop spins.
        assert!(is_fd_exhaustion(&Error::from_raw_os_error(24)));
        assert!(is_fd_exhaustion(&Error::from_raw_os_error(23)));
        assert!(!is_fd_exhaustion(&Error::new(
            ErrorKind::ConnectionAborted,
            "no errno"
        )));
    }

    #[test]
    fn an_advertised_request_encoding_is_normalized_to_identity() {
        // Otherwise the operator can make every wallet compress its
        // SendTransaction bodies and blind the classifier.
        let mut headers = HeaderMap::new();
        headers.insert("grpc-accept-encoding", HeaderValue::from_static("gzip"));
        normalize_response_encoding(&mut headers);
        assert_eq!(headers.get("grpc-accept-encoding").unwrap(), "identity");

        // An indexer that said nothing keeps saying nothing: identity is
        // already the gRPC default, so there is no header to add.
        let mut headers = HeaderMap::new();
        normalize_response_encoding(&mut headers);
        assert!(headers.get("grpc-accept-encoding").is_none());
    }

    #[test]
    fn grpc_message_is_header_safe() {
        let resp = grpc_error(GRPC_RESOURCE_EXHAUSTED, "body too large\r\ninjected: yes");
        let message = resp.headers().get("grpc-message").unwrap();
        assert!(!message.as_bytes().contains(&b'\r'));
        assert!(!message.as_bytes().contains(&b'\n'));
    }
}
