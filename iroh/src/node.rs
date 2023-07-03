//! Node API
//!
//! A node is a server that serves various protocols.
//!
//! You can monitor what is happening in the node using [`Node::subscribe`].
//!
//! To shut down the node, call [`Node::shutdown`].

use std::any::Any;
use std::fmt::Debug;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::future::{BoxFuture, Shared};
use futures::{FutureExt, Stream, TryFutureExt};
use iroh_bytes::provider::database::BaoCollection;
use iroh_bytes::provider::RequestAuthorizationHandler;
use iroh_bytes::{
    blobs::Collection,
    protocol::Closed,
    provider::{CustomGetHandler, Database, ProvideProgress, Ticket, ValidateProgress},
    runtime,
    util::{Hash, Progress},
};
use iroh_net::{
    hp::{cfg::Endpoint, derp::DerpMap},
    tls::{self, Keypair, PeerId},
};
use quic_rpc::server::RpcChannel;
use quic_rpc::transport::flume::FlumeConnection;
use quic_rpc::transport::misc::DummyServerEndpoint;
use quic_rpc::{RpcClient, RpcServer, ServiceConnection, ServiceEndpoint};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use crate::rpc_protocol::{
    AddrsRequest, AddrsResponse, IdRequest, IdResponse, ListBlobsRequest, ListBlobsResponse,
    ListCollectionsRequest, ListCollectionsResponse, ProvideRequest, ProviderRequest,
    ProviderResponse, ProviderService, ShutdownRequest, ValidateRequest, VersionRequest,
    VersionResponse, WatchRequest, WatchResponse,
};

const MAX_CONNECTIONS: u32 = 1024;
const MAX_STREAMS: u64 = 10;
const HEALTH_POLL_WAIT: Duration = Duration::from_secs(1);

/// Default bind address for the node.
/// 11204 is "iroh" in leetspeak https://simple.wikipedia.org/wiki/Leet
pub const DEFAULT_BIND_ADDR: (Ipv4Addr, u16) = (Ipv4Addr::LOCALHOST, 11204);

/// How long we wait at most for some endpoints to be discovered.
const ENDPOINT_WAIT: Duration = Duration::from_secs(5);

/// Builder for the [`Node`].
///
/// You must supply a database which can be created using [`iroh_bytes::provider::create_collection`], everything else is
/// optional.  Finally you can create and run the node by calling [`Builder::spawn`].
///
/// The returned [`Node`] is awaitable to know when it finishes.  It can be terminated
/// using [`Node::shutdown`].
#[derive(Debug)]
pub struct Builder<D = Database, E = DummyServerEndpoint, C = (), A = ()>
where
    D: BaoCollection,
    E: ServiceEndpoint<ProviderService>,
    C: CustomGetHandler<D>,
    A: RequestAuthorizationHandler<D>,
{
    bind_addr: SocketAddr,
    keypair: Keypair,
    rpc_endpoint: E,
    db: D,
    keylog: bool,
    custom_get_handler: C,
    auth_handler: A,
    derp_map: Option<DerpMap>,
    rt: Option<runtime::Handle>,
}

const PROTOCOLS: [&[u8]; 1] = [&iroh_bytes::P2P_ALPN];

impl<D: BaoCollection> Builder<D> {
    /// Creates a new builder for [`Node`] using the given [`Database`].
    pub fn with_db(db: D) -> Self {
        Self {
            bind_addr: DEFAULT_BIND_ADDR.into(),
            keypair: Keypair::generate(),
            db,
            keylog: false,
            derp_map: None,
            rpc_endpoint: Default::default(),
            custom_get_handler: Default::default(),
            auth_handler: Default::default(),
            rt: None,
        }
    }
}

impl<E, C, A, D> Builder<D, E, C, A>
where
    D: BaoCollection,
    E: ServiceEndpoint<ProviderService>,
    C: CustomGetHandler<D>,
    A: RequestAuthorizationHandler<D>,
{
    /// Configure rpc endpoint, changing the type of the builder to the new endpoint type.
    pub fn rpc_endpoint<E2: ServiceEndpoint<ProviderService>>(
        self,
        value: E2,
    ) -> Builder<D, E2, C, A> {
        // we can't use ..self here because the return type is different
        Builder {
            bind_addr: self.bind_addr,
            keypair: self.keypair,
            db: self.db,
            keylog: self.keylog,
            custom_get_handler: self.custom_get_handler,
            auth_handler: self.auth_handler,
            rpc_endpoint: value,
            derp_map: self.derp_map,
            rt: self.rt,
        }
    }

    /// Sets the `[DerpMap]`
    pub fn derp_map(mut self, dm: DerpMap) -> Self {
        self.derp_map = Some(dm);
        self
    }

    /// Configure the custom get handler, changing the type of the builder to the new handler type.
    pub fn custom_get_handler<C2: CustomGetHandler<D>>(
        self,
        custom_handler: C2,
    ) -> Builder<D, E, C2, A> {
        // we can't use ..self here because the return type is different
        Builder {
            bind_addr: self.bind_addr,
            keypair: self.keypair,
            db: self.db,
            keylog: self.keylog,
            rpc_endpoint: self.rpc_endpoint,
            custom_get_handler: custom_handler,
            auth_handler: self.auth_handler,
            derp_map: self.derp_map,
            rt: self.rt,
        }
    }

    pub fn custom_auth_handler<A2: RequestAuthorizationHandler<D>>(
        self,
        auth_handler: A2,
    ) -> Builder<D, E, C, A2> {
        // we can't use ..self here because the return type is different
        Builder {
            bind_addr: self.bind_addr,
            keypair: self.keypair,
            db: self.db,
            keylog: self.keylog,
            rpc_endpoint: self.rpc_endpoint,
            custom_get_handler: self.custom_get_handler,
            auth_handler,
            derp_map: self.derp_map,
            rt: self.rt,
        }
    }

    /// Binds the node service to a different socket.
    ///
    /// By default it binds to `127.0.0.1:11204`.
    pub fn bind_addr(mut self, addr: SocketAddr) -> Self {
        self.bind_addr = addr;
        self
    }

    /// Uses the given [`Keypair`] for the [`PeerId`] instead of a newly generated one.
    pub fn keypair(mut self, keypair: Keypair) -> Self {
        self.keypair = keypair;
        self
    }

    /// Whether to log the SSL pre-master key.
    ///
    /// If `true` and the `SSLKEYLOGFILE` environment variable is the path to a file this
    /// file will be used to log the SSL pre-master key.  This is useful to inspect captured
    /// traffic.
    pub fn keylog(mut self, keylog: bool) -> Self {
        self.keylog = keylog;
        self
    }

    /// Sets the tokio runtime to use.
    ///
    /// If not set, the current runtime will be picked up.
    pub fn runtime(mut self, rt: &runtime::Handle) -> Self {
        self.rt = Some(rt.clone());
        self
    }

    /// Spawns the [`Node`] in a tokio task.
    ///
    /// This will create the underlying network server and spawn a tokio task accepting
    /// connections.  The returned [`Node`] can be used to control the task as well as
    /// get information about it.
    pub async fn spawn(self) -> Result<Node<D>> {
        trace!("spawning node");
        let rt = self.rt.context("runtime not set")?;
        let tls_server_config = tls::make_server_config(
            &self.keypair,
            PROTOCOLS.iter().map(|p| p.to_vec()).collect(),
            self.keylog,
        )?;
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_server_config));
        let mut transport_config = quinn::TransportConfig::default();
        transport_config
            .max_concurrent_bidi_streams(MAX_STREAMS.try_into()?)
            .max_concurrent_uni_streams(0u32.into());

        server_config
            .transport_config(Arc::new(transport_config))
            .concurrent_connections(MAX_CONNECTIONS);

        let (endpoints_update_s, endpoints_update_r) = flume::bounded(1);
        let conn = iroh_net::hp::magicsock::Conn::new(iroh_net::hp::magicsock::Options {
            port: self.bind_addr.port(),
            private_key: self.keypair.secret().clone().into(),
            on_endpoints: Some(Box::new(move |eps| {
                if !endpoints_update_s.is_disconnected() && !eps.is_empty() {
                    endpoints_update_s.send(()).ok();
                }
            })),
            ..Default::default()
        })
        .await?;
        trace!("created magicsock");

        let derp_map = self.derp_map.unwrap_or_default();
        conn.set_derp_map(Some(derp_map))
            .await
            .context("setting derp map")?;

        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(server_config),
            conn.clone(),
            Arc::new(quinn::TokioRuntime),
        )?;

        trace!("created quinn endpoint");

        // the size of this channel must be large because the producer can be on
        // a different thread than the consumer, and can produce a lot of events
        // in a short time
        let (events_sender, _events_receiver) = broadcast::channel(512);
        let events = events_sender.clone();
        let cancel_token = CancellationToken::new();

        debug!("rpc listening on: {:?}", self.rpc_endpoint.local_addr());

        let (internal_rpc, controller) = quic_rpc::transport::flume::connection(1);
        let rt2 = rt.clone();
        let rt3 = rt.clone();
        let inner = Arc::new(NodeInner {
            db: self.db,
            conn,
            keypair: self.keypair,
            events,
            controller,
            cancel_token,
            rt,
        });
        let task = {
            let handler = RpcHandler {
                inner: inner.clone(),
            };
            rt2.main().spawn(async move {
                Self::run(
                    endpoint,
                    events_sender,
                    handler,
                    self.rpc_endpoint,
                    internal_rpc,
                    self.custom_get_handler,
                    self.auth_handler,
                    rt3,
                )
                .await
            })
        };
        let node = Node {
            inner,
            task: task.map_err(Arc::new).boxed().shared(),
        };

        // Wait for a single endpoint update, to make sure
        // we found some endpoints
        tokio::time::timeout(ENDPOINT_WAIT, async move {
            endpoints_update_r.recv_async().await
        })
        .await
        .context("waiting for endpoint")??;

        Ok(node)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run(
        server: quinn::Endpoint,
        events: broadcast::Sender<Event>,
        handler: RpcHandler<D>,
        rpc: E,
        internal_rpc: impl ServiceEndpoint<ProviderService>,
        custom_get_handler: C,
        auth_handler: A,
        rt: runtime::Handle,
    ) {
        let rpc = RpcServer::new(rpc);
        let internal_rpc = RpcServer::new(internal_rpc);
        if let Ok(addr) = server.local_addr() {
            debug!("listening at: {addr}");
        }
        let cancel_token = handler.inner.cancel_token.clone();
        loop {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => break,
                // handle rpc requests. This will do nothing if rpc is not configured, since
                // accept is just a pending future.
                request = rpc.accept() => {
                    match request {
                        Ok((msg, chan)) => {
                            handle_rpc_request(msg, chan, &handler, &rt);
                        }
                        Err(e) => {
                            tracing::info!("rpc request error: {:?}", e);
                        }
                    }
                },
                // handle internal rpc requests.
                request = internal_rpc.accept() => {
                    match request {
                        Ok((msg, chan)) => {
                            handle_rpc_request(msg, chan, &handler, &rt);
                        }
                        Err(_) => {
                            tracing::info!("last controller dropped, shutting down");
                            break;
                        }
                    }
                },
                // handle incoming p2p connections
                Some(mut connecting) = server.accept() => {

                    let alpn = match get_alpn(&mut connecting).await {
                        Ok(alpn) => alpn,
                        Err(err) => {
                            tracing::error!("invalid handshake: {:?}", err);
                            continue;
                        }
                    };
                    if alpn.as_bytes() == iroh_bytes::P2P_ALPN.as_ref() {
                        let db = handler.inner.db.clone();
                        let events = MappedSender(events.clone());
                        let custom_get_handler = custom_get_handler.clone();
                        let auth_handler = auth_handler.clone();
                        let rt2 = rt.clone();
                        rt.main().spawn(iroh_bytes::provider::handle_connection(connecting, db, events, custom_get_handler, auth_handler, rt2));
                    } else {
                        tracing::error!("unknown protocol: {}", alpn);
                        continue;
                    }
                }
                else => break,
            }
        }

        // Closing the Endpoint is the equivalent of calling Connection::close on all
        // connections: Operations will immediately fail with
        // ConnectionError::LocallyClosed.  All streams are interrupted, this is not
        // graceful.
        let error_code = Closed::ProviderTerminating;
        server.close(error_code.into(), error_code.reason());
    }
}

async fn get_alpn(connecting: &mut quinn::Connecting) -> Result<String> {
    let data = connecting.handshake_data().await?;
    match data.downcast::<quinn::crypto::rustls::HandshakeData>() {
        Ok(data) => match data.protocol {
            Some(protocol) => std::string::String::from_utf8(protocol).map_err(Into::into),
            None => anyhow::bail!("no ALPN protocol available"),
        },
        Err(_) => anyhow::bail!("unknown handshake type"),
    }
}

#[derive(Debug, Clone)]
struct MappedSender(broadcast::Sender<Event>);

impl iroh_bytes::provider::EventSender for MappedSender {
    fn send(&self, event: iroh_bytes::provider::Event) -> Option<iroh_bytes::provider::Event> {
        match self.0.send(Event::ByteProvide(event)) {
            Ok(_) => None,
            Err(broadcast::error::SendError(Event::ByteProvide(e))) => Some(e),
        }
    }
}

/// A server which implements the iroh node.
///
/// Clients can connect to this server and requests hashes from it.
///
/// The only way to create this is by using the [`Builder::spawn`].  [`Node::builder`]
/// is a shorthand to create a suitable [`Builder`].
///
/// This runs a tokio task which can be aborted and joined if desired.  To join the task
/// await the [`Node`] struct directly, it will complete when the task completes.  If
/// this is dropped the node task is not stopped but keeps running.
#[derive(Debug, Clone)]
pub struct Node<D: BaoCollection> {
    inner: Arc<NodeInner<D>>,
    task: Shared<BoxFuture<'static, Result<(), Arc<JoinError>>>>,
}

#[derive(Debug)]
struct NodeInner<D> {
    db: D,
    conn: iroh_net::hp::magicsock::Conn,
    keypair: Keypair,
    events: broadcast::Sender<Event>,
    cancel_token: CancellationToken,
    controller: FlumeConnection<ProviderResponse, ProviderRequest>,
    rt: runtime::Handle,
}

/// Events emitted by the [`Node`] informing about the current status.
#[derive(Debug, Clone)]
pub enum Event {
    ByteProvide(iroh_bytes::provider::Event),
}

impl<D: BaoCollection> Node<D> {
    /// Returns a new builder for the [`Node`].
    ///
    /// Once the done with the builder call [`Builder::spawn`] to create the node.
    pub fn builder(db: D) -> Builder<D> {
        Builder::with_db(db)
    }

    /// The address on which the node socket is bound.
    ///
    /// Note that this could be an unspecified address, if you need an address on which you
    /// can contact the node consider using [`Node::local_endpoint_addresses`].  However the
    /// port will always be the concrete port.
    pub fn local_address(&self) -> Result<Vec<SocketAddr>> {
        self.inner.local_address()
    }

    /// Lists the local endpoint of this node.
    pub async fn local_endpoints(&self) -> Result<Vec<Endpoint>> {
        self.inner.local_endpoints().await
    }

    /// Convenience method to get just the addr part of [`Node::local_endpoints`].
    pub async fn local_endpoint_addresses(&self) -> Result<Vec<SocketAddr>> {
        self.inner.local_endpoint_addresses().await
    }

    /// Returns the [`PeerId`] of the node.
    pub fn peer_id(&self) -> PeerId {
        self.inner.keypair.public().into()
    }

    /// Subscribe to [`Event`]s emitted from the node, informing about connections and
    /// progress.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Returns a handle that can be used to do RPC calls to the node internally.
    pub fn controller(
        &self,
    ) -> RpcClient<ProviderService, impl ServiceConnection<ProviderService>> {
        RpcClient::new(self.inner.controller.clone())
    }

    /// Return a single token containing everything needed to get a hash.
    ///
    /// See [`Ticket`] for more details of how it can be used.
    pub async fn ticket(&self, hash: Hash) -> Result<Ticket> {
        // TODO: Verify that the hash exists in the db?
        let addrs = self.local_endpoint_addresses().await?;
        Ticket::new(hash, self.peer_id(), addrs, None)
    }

    /// Aborts the node.
    ///
    /// This does not gracefully terminate currently: all connections are closed and
    /// anything in-transit is lost.  The task will stop running and awaiting this
    /// [`Node`] will complete.
    ///
    /// The shutdown behaviour will become more graceful in the future.
    pub fn shutdown(&self) {
        self.inner.cancel_token.cancel();
    }

    /// Returns a token that can be used to cancel the node.
    pub fn cancel_token(&self) -> CancellationToken {
        self.inner.cancel_token.clone()
    }
}

impl<D: BaoCollection> NodeInner<D> {
    async fn local_endpoints(&self) -> Result<Vec<Endpoint>> {
        self.conn.local_endpoints().await
    }

    async fn local_endpoint_addresses(&self) -> Result<Vec<SocketAddr>> {
        let endpoints = self.local_endpoints().await?;
        Ok(endpoints.into_iter().map(|x| x.addr).collect())
    }

    fn local_address(&self) -> Result<Vec<SocketAddr>> {
        let (v4, v6) = self.conn.local_addr()?;
        let mut addrs = vec![v4];
        if let Some(v6) = v6 {
            addrs.push(v6);
        }
        Ok(addrs)
    }
}

/// The future completes when the spawned tokio task finishes.
impl<D: BaoCollection> Future for Node<D> {
    type Output = Result<(), Arc<JoinError>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.task).poll(cx)
    }
}

#[derive(Debug, Clone)]
struct RpcHandler<D> {
    inner: Arc<NodeInner<D>>,
}

impl<D: BaoCollection> RpcHandler<D> {
    fn rt(&self) -> runtime::Handle {
        self.inner.rt.clone()
    }

    fn concrete_db(&self) -> Option<Database> {
        let db: Box<dyn Any> = Box::new(self.inner.db.clone());
        db.downcast_ref::<Database>().cloned()
    }

    fn list_blobs(
        self,
        _msg: ListBlobsRequest,
    ) -> impl Stream<Item = ListBlobsResponse> + Send + 'static {
        let items = if let Some(db) = self.concrete_db() {
            db.external()
                .map(|(hash, path, size)| ListBlobsResponse { hash, path, size })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        futures::stream::iter(items)
    }

    fn list_collections(
        self,
        _msg: ListCollectionsRequest,
    ) -> impl Stream<Item = ListCollectionsResponse> + Send + 'static {
        // collections are always stored internally, so we take everything that is stored internally
        // and try to parse it as a collection
        let items = if let Some(db) = self.concrete_db() {
            db.internal()
                .filter_map(|(hash, collection)| {
                    Collection::from_bytes(&collection).ok().map(|collection| {
                        ListCollectionsResponse {
                            hash,
                            total_blobs_count: collection.blobs().len(),
                            total_blobs_size: collection.total_blobs_size(),
                        }
                    })
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        futures::stream::iter(items)
    }

    /// Invoke validate on the database and stream out the result
    fn validate(
        self,
        _msg: ValidateRequest,
    ) -> impl Stream<Item = ValidateProgress> + Send + 'static {
        let (tx, rx) = mpsc::channel(1);
        let tx2 = tx.clone();
        if let Some(db) = self.concrete_db() {
            self.rt().main().spawn(async move {
                if let Err(e) = db.validate(tx).await {
                    tx2.send(ValidateProgress::Abort(e.into())).await.unwrap();
                }
            });
        }
        tokio_stream::wrappers::ReceiverStream::new(rx)
    }

    fn provide(self, msg: ProvideRequest) -> impl Stream<Item = ProvideProgress> {
        let (tx, rx) = mpsc::channel(1);
        let tx2 = tx.clone();
        self.rt().main().spawn(async move {
            if let Err(e) = self.provide0(msg, tx).await {
                tx2.send(ProvideProgress::Abort(e.into())).await.unwrap();
            }
        });
        tokio_stream::wrappers::ReceiverStream::new(rx)
    }

    async fn provide0(
        self,
        msg: ProvideRequest,
        progress: tokio::sync::mpsc::Sender<ProvideProgress>,
    ) -> anyhow::Result<()> {
        let root = msg.path;
        anyhow::ensure!(
            root.is_dir() || root.is_file(),
            "path must be either a Directory or a File"
        );
        let data_sources = iroh_bytes::provider::create_data_sources(root)?;
        // create the collection
        // todo: provide feedback for progress
        let (db, hash) = iroh_bytes::provider::collection::create_collection(
            data_sources,
            Progress::new(progress),
        )
        .await?;
        if let Some(current) = self.concrete_db() {
            current.union_with(db);
        }

        if let Err(e) = self.inner.events.send(Event::ByteProvide(
            iroh_bytes::provider::Event::CollectionAdded { hash },
        )) {
            warn!("failed to send CollectionAdded event: {:?}", e);
        };

        Ok(())
    }
    async fn version(self, _: VersionRequest) -> VersionResponse {
        VersionResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
    async fn id(self, _: IdRequest) -> IdResponse {
        IdResponse {
            peer_id: Box::new(self.inner.keypair.public().into()),
            listen_addrs: self
                .inner
                .local_endpoint_addresses()
                .await
                .unwrap_or_default(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
    async fn addrs(self, _: AddrsRequest) -> AddrsResponse {
        AddrsResponse {
            addrs: self
                .inner
                .local_endpoint_addresses()
                .await
                .unwrap_or_default(),
        }
    }
    async fn shutdown(self, request: ShutdownRequest) {
        if request.force {
            tracing::info!("hard shutdown requested");
            std::process::exit(0);
        } else {
            // trigger a graceful shutdown
            tracing::info!("graceful shutdown requested");
            self.inner.cancel_token.cancel();
        }
    }
    fn watch(self, _: WatchRequest) -> impl Stream<Item = WatchResponse> {
        futures::stream::unfold((), |()| async move {
            tokio::time::sleep(HEALTH_POLL_WAIT).await;
            Some((
                WatchResponse {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                (),
            ))
        })
    }
}

fn handle_rpc_request<D: BaoCollection, C: ServiceEndpoint<ProviderService>>(
    msg: ProviderRequest,
    chan: RpcChannel<ProviderService, C>,
    handler: &RpcHandler<D>,
    rt: &runtime::Handle,
) {
    let handler = handler.clone();
    rt.main().spawn(async move {
        use ProviderRequest::*;
        match msg {
            ListBlobs(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::list_blobs)
                    .await
            }
            ListCollections(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::list_collections)
                    .await
            }
            Provide(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::provide)
                    .await
            }
            Watch(msg) => chan.server_streaming(msg, handler, RpcHandler::watch).await,
            Version(msg) => chan.rpc(msg, handler, RpcHandler::version).await,
            Id(msg) => chan.rpc(msg, handler, RpcHandler::id).await,
            Addrs(msg) => chan.rpc(msg, handler, RpcHandler::addrs).await,
            Shutdown(msg) => chan.rpc(msg, handler, RpcHandler::shutdown).await,
            Validate(msg) => {
                chan.server_streaming(msg, handler, RpcHandler::validate)
                    .await
            }
        }
    });
}

/// Create a [`quinn::ServerConfig`] with the given keypair and limits.
pub fn make_server_config(
    keypair: &Keypair,
    max_streams: u64,
    max_connections: u32,
    alpn_protocols: Vec<Vec<u8>>,
) -> anyhow::Result<quinn::ServerConfig> {
    let tls_server_config = tls::make_server_config(keypair, alpn_protocols, false)?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_server_config));
    let mut transport_config = quinn::TransportConfig::default();
    transport_config
        .max_concurrent_bidi_streams(max_streams.try_into()?)
        .max_concurrent_uni_streams(0u32.into());

    server_config
        .transport_config(Arc::new(transport_config))
        .concurrent_connections(max_connections);
    Ok(server_config)
}

#[cfg(test)]
mod tests {
    use anyhow::bail;
    use futures::StreamExt;
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::path::Path;

    use super::*;

    /// Pick up the tokio runtime from the thread local and add a
    /// thread per core runtime.
    fn test_runtime() -> runtime::Handle {
        runtime::Handle::from_currrent(1).unwrap()
    }

    #[tokio::test]
    async fn test_ticket_multiple_addrs() {
        let rt = test_runtime();
        let readme = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
        let (db, hash) = iroh_bytes::provider::create_collection(vec![readme.into()])
            .await
            .unwrap();
        let node = Node::builder(db)
            .bind_addr((Ipv4Addr::UNSPECIFIED, 0).into())
            .runtime(&rt)
            .spawn()
            .await
            .unwrap();
        let _drop_guard = node.cancel_token().drop_guard();
        let ticket = node.ticket(hash).await.unwrap();
        println!("addrs: {:?}", ticket.addrs());
        assert!(!ticket.addrs().is_empty());
    }

    #[tokio::test]
    async fn test_node_add_collection_event() -> Result<()> {
        let db = Database::from(HashMap::new());
        let node = Builder::with_db(db)
            .bind_addr((Ipv4Addr::UNSPECIFIED, 0).into())
            .runtime(&test_runtime())
            .spawn()
            .await?;

        let _drop_guard = node.cancel_token().drop_guard();

        let mut events = node.subscribe();
        let provide_handle = tokio::spawn(async move {
            while let Ok(msg) = events.recv().await {
                if let Event::ByteProvide(iroh_bytes::provider::Event::CollectionAdded { hash }) =
                    msg
                {
                    return Some(hash);
                }
            }
            None
        });

        let got_hash = tokio::time::timeout(Duration::from_secs(1), async move {
            let mut stream = node
                .controller()
                .server_streaming(ProvideRequest {
                    path: Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"),
                })
                .await?;

            while let Some(item) = stream.next().await {
                match item? {
                    ProvideProgress::AllDone { hash } => {
                        return Ok(hash);
                    }
                    ProvideProgress::Abort(e) => {
                        bail!("Error while adding data: {e}");
                    }
                    _ => {}
                }
            }
            bail!("stream ended without providing data");
        })
        .await
        .context("timeout")?
        .context("get failed")?;

        let event_hash = provide_handle.await?.expect("missing collection event");
        assert_eq!(got_hash, event_hash);

        Ok(())
    }
}
