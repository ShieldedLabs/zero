//! Implements methods for testing [`Handshake`]

#![allow(clippy::unwrap_in_result)]

use super::*;

impl<S, C> Handshake<S, C>
where
    S: Service<Request, Response = Response, Error = BoxError> + Clone + Send + 'static,
    S::Future: Send,
    C: ChainTip + Clone + Send + 'static,
{
    /// Returns a count of how many connection nonces are stored in this [`Handshake`]
    pub async fn nonce_count(&self) -> usize {
        self.nonces.lock().await.len()
    }
}

#[cfg(test)]
mod vectors {
    use tokio::io::{duplex, DuplexStream};
    use tower::ServiceExt;

    use crate::peer_set::ActiveConnectionCounter;

    use super::*;

    /// The test peer address for direct connections.
    const TEST_ADDR: &str = "127.0.0.1:8233";

    /// Spawns a fake remote peer on `peer_side` that completes a handshake,
    /// advertising `remote_services` in its version message.
    ///
    /// The fake peer waits for Zebra's version message, replies with its own
    /// version message and a verack, then drains the stream. Sending the
    /// verack unconditionally is harmless: rejecting handshakes return before
    /// reading it.
    fn spawn_fake_peer(
        network: &Network,
        peer_side: DuplexStream,
        remote_services: PeerServices,
    ) -> tokio::task::JoinHandle<()> {
        let mut peer_conn = Framed::new(peer_side, Codec::builder().for_network(network).finish());
        let addr: PeerSocketAddr = TEST_ADDR.parse().unwrap();

        tokio::spawn(async move {
            // Wait for Zebra's version message.
            loop {
                match peer_conn.next().await {
                    Some(Ok(Message::Version(_))) => break,
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return,
                }
            }

            let version = VersionMessage {
                version: constants::CURRENT_NETWORK_PROTOCOL_VERSION,
                services: remote_services,
                timestamp: Utc::now(),
                address_recv: AddrInVersion::new(addr, PeerServices::NODE_NETWORK),
                address_from: AddrInVersion::new(addr, remote_services),
                nonce: Nonce::default(),
                user_agent: "/fake-peer/".to_string(),
                start_height: block::Height(0),
                relay: false,
            };

            if peer_conn.send(version.into()).await.is_err() {
                return;
            }
            if peer_conn.send(Message::Verack).await.is_err() {
                return;
            }

            // Drain the stream so Zebra's writes don't block.
            while let Some(Ok(_)) = peer_conn.next().await {}
        })
    }

    /// Drives [`negotiate_version`] against a fake peer advertising
    /// `remote_services`, using `connected_addr` as the connection direction.
    async fn negotiate_with_remote_services(
        network: &Network,
        connected_addr: ConnectedAddr,
        remote_services: PeerServices,
    ) -> Result<Arc<ConnectionInfo>, HandshakeError> {
        let (zebra_side, peer_side) = duplex(4096);
        let fake_peer = spawn_fake_peer(network, peer_side, remote_services);

        let mut peer_conn = Framed::new(zebra_side, Codec::builder().for_network(network).finish());

        let config = Config {
            network: network.clone(),
            ..Config::default()
        };
        let nonces = Arc::new(futures::lock::Mutex::new(IndexSet::new()));
        let minimum_peer_version = MinimumPeerVersion::new(NoChainTip, network);

        let result = timeout(
            Duration::from_secs(10),
            negotiate_version(
                &mut peer_conn,
                &connected_addr,
                config,
                nonces,
                "/test-zebra/".to_string(),
                PeerServices::NODE_NETWORK,
                false,
                minimum_peer_version,
            ),
        )
        .await
        .expect("handshake must not time out");

        fake_peer.abort();

        result
    }

    /// Outbound handshakes with peers that don't advertise `NODE_NETWORK` must
    /// be rejected: such peers can't serve us blocks, and would otherwise
    /// occupy outbound slots and time out block requests, stalling sync.
    #[tokio::test]
    async fn outbound_handshake_rejects_peer_without_node_network() {
        let _init_guard = zebra_test::init();
        let addr: PeerSocketAddr = TEST_ADDR.parse().unwrap();

        for network in Network::iter() {
            let result = negotiate_with_remote_services(
                &network,
                ConnectedAddr::new_outbound_direct(addr),
                PeerServices::empty(),
            )
            .await;
            assert!(
                matches!(
                    result,
                    Err(HandshakeError::MissingRequiredServices { advertised })
                        if advertised == PeerServices::empty()
                ),
                "outbound handshake with services=0 must be rejected, got: {result:?}",
            );

            let result = negotiate_with_remote_services(
                &network,
                ConnectedAddr::new_outbound_proxy(
                    "127.0.0.2:9050".parse().unwrap(),
                    "127.0.0.1:0".parse().unwrap(),
                ),
                PeerServices::empty(),
            )
            .await;
            assert!(
                matches!(result, Err(HandshakeError::MissingRequiredServices { .. })),
                "proxied outbound handshake with services=0 must be rejected, got: {result:?}",
            );

            // Positive control: outbound peers advertising NODE_NETWORK are accepted.
            let result = negotiate_with_remote_services(
                &network,
                ConnectedAddr::new_outbound_direct(addr),
                PeerServices::NODE_NETWORK,
            )
            .await;
            let connection_info =
                result.expect("outbound handshake with NODE_NETWORK must succeed");
            assert_eq!(connection_info.remote.services, PeerServices::NODE_NETWORK);
        }
    }

    /// Inbound and isolated handshakes must accept peers without
    /// `NODE_NETWORK`: light clients advertise no services, and isolated
    /// connections are deliberately chosen by the caller.
    #[tokio::test]
    async fn inbound_and_isolated_handshakes_accept_peer_without_node_network() {
        let _init_guard = zebra_test::init();
        let addr: PeerSocketAddr = TEST_ADDR.parse().unwrap();

        for network in Network::iter() {
            let result = negotiate_with_remote_services(
                &network,
                ConnectedAddr::new_inbound_direct(addr),
                PeerServices::empty(),
            )
            .await;
            let connection_info = result.expect("inbound handshake with services=0 must succeed");
            assert_eq!(connection_info.remote.services, PeerServices::empty());

            let result = negotiate_with_remote_services(
                &network,
                ConnectedAddr::new_isolated(),
                PeerServices::empty(),
            )
            .await;
            result.expect("isolated handshake with services=0 must succeed");
        }
    }

    /// A rejected outbound handshake must record the peer's advertised
    /// services in the address book, so the crawler skips the peer instead of
    /// re-dialing it forever (dial failures alone are recorded with
    /// `services: None`, which counts as a full node in
    /// `MetaAddr::last_known_info_is_valid_for_outbound`).
    #[tokio::test]
    async fn rejected_outbound_handshake_records_services_in_address_book() {
        let _init_guard = zebra_test::init();
        let addr: PeerSocketAddr = TEST_ADDR.parse().unwrap();
        let network = Network::Mainnet;

        let (zebra_side, peer_side) = duplex(4096);
        let fake_peer = spawn_fake_peer(&network, peer_side, PeerServices::empty());

        let (updater_tx, mut updater_rx) = tokio::sync::mpsc::channel(10);

        let handshake = Handshake::builder()
            .with_config(Config {
                network: network.clone(),
                ..Config::default()
            })
            .with_inbound_service(tower::service_fn(|_req| async {
                unreachable!("inbound service must not be called during a rejected handshake")
            }))
            .with_address_book_updater(updater_tx)
            .finish()
            .expect("handshake builder must succeed");

        let connected_addr = ConnectedAddr::new_outbound_direct(addr);
        let connection_tracker = ActiveConnectionCounter::new_counter().track_connection();

        let result = timeout(
            Duration::from_secs(10),
            handshake.oneshot(HandshakeRequest::<DuplexStream> {
                data_stream: zebra_side,
                connected_addr,
                connection_tracker,
            }),
        )
        .await
        .expect("handshake must not time out");
        assert!(
            result.is_err(),
            "outbound handshake with services=0 must fail",
        );

        fake_peer.abort();

        let change = updater_rx
            .try_recv()
            .expect("rejected handshake must record an address book change");
        let book_addr = connected_addr
            .get_address_book_addr()
            .expect("outbound direct connections have an address book address");
        assert!(
            matches!(
                change,
                MetaAddrChange::UpdateFailed { addr, services }
                    if addr == book_addr && services == Some(PeerServices::empty())
            ),
            "rejection must record UpdateFailed with the advertised services, got: {change:?}",
        );
    }
}
