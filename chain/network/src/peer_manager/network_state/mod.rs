use crate::accounts_data;
use crate::client;
use crate::concurrency::rate;
use crate::config;
use crate::network_protocol::{
    Edge, EdgeState, PartialEdgeInfo, PeerIdOrHash, PeerMessage, Ping, Pong, RawRoutedMessage,
    RoutedMessageBody, RoutedMessageV2, RoutingTableUpdate,
};
use crate::peer_manager::connection;
use crate::peer_manager::peer_store;
use crate::private_actix::{PeerToManagerMsg, ValidateEdgeList};
use crate::routing;
use crate::routing::edge_validator_actor::EdgeValidatorHelper;
use crate::routing::route_back_cache::RouteBackCache;
use crate::routing::routing_table_view::RoutingTableView;
use crate::stats::metrics;
use crate::store;
use crate::tcp;
use crate::time;
use crate::types::{ChainInfo, ReasonForBan};
use actix::Recipient;
use arc_swap::ArcSwap;
use near_primitives::block::GenesisId;
use near_primitives::hash::CryptoHash;
use near_primitives::network::{AnnounceAccount, PeerId};
use near_primitives::types::AccountId;
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

mod tier1;

/// Limit number of pending Peer actors to avoid OOM.
pub(crate) const LIMIT_PENDING_PEERS: usize = 60;

/// Send important messages three times.
/// We send these messages multiple times to reduce the chance that they are lost
const IMPORTANT_MESSAGE_RESENT_COUNT: usize = 3;

pub(crate) struct NetworkState {
    /// PeerManager config.
    pub config: Arc<config::VerifiedConfig>,
    /// GenesisId of the chain.
    pub genesis_id: GenesisId,
    pub client: client::Client,
    /// Address of the peer manager actor.
    pub peer_manager_addr: Recipient<PeerToManagerMsg>,
    /// RoutingTableActor, responsible for computing routing table, routing table exchange, etc.
    pub routing_table_addr: actix::Addr<routing::Actor>,

    /// Network-related info about the chain.
    pub chain_info: ArcSwap<ChainInfo>,
    /// AccountsData for TIER1 accounts.
    pub accounts_data: Arc<accounts_data::Cache>,
    /// Connected peers (inbound and outbound) with their full peer information.
    pub tier2: connection::Pool,
    pub tier1: connection::Pool,
    /// Semaphore limiting inflight inbound handshakes.
    pub inbound_handshake_permits: Arc<tokio::sync::Semaphore>,
    /// Peer store that provides read/write access to peers.
    pub peer_store: peer_store::PeerStore,
    /// A graph of the whole NEAR network, shared between routing::Actor
    /// and PeerManagerActor. PeerManagerActor should have read-only access to the graph.
    /// TODO: this is an intermediate step towards replacing actix runtime with a
    /// generic threadpool (or multiple pools) in the near-network crate.
    /// It the threadpool setup, inevitably some of the state will be shared.
    pub graph: Arc<RwLock<routing::GraphWithCache>>,

    /// View of the Routing table. It keeps:
    /// - routing information - how to route messages
    /// - edges adjacent to my_peer_id
    /// - account id
    /// Full routing table (that currently includes information about all edges in the graph) is now inside Routing Table.
    pub routing_table_view: RoutingTableView,
    /// Fields used for communicating with EdgeValidatorActor
    pub routing_table_exchange_helper: EdgeValidatorHelper,

    /// Hash of messages that requires routing back to respective previous hop.
    pub tier1_route_back: Mutex<RouteBackCache>,

    /// Shared counter across all PeerActors, which counts number of `RoutedMessageBody::ForwardTx`
    /// messages sincce last block.
    pub txns_since_last_block: AtomicUsize,

    pub tier1_recv_limiter: rate::Limiter,
}

impl NetworkState {
    pub fn new(
        clock: &time::Clock,
        store: store::Store,
        peer_store: peer_store::PeerStore,
        config: Arc<config::VerifiedConfig>,
        genesis_id: GenesisId,
        client: client::Client,
        peer_manager_addr: Recipient<PeerToManagerMsg>,
    ) -> Self {
        let graph = Arc::new(RwLock::new(routing::GraphWithCache::new(config.node_id())));
        Self {
            routing_table_addr: routing::Actor::spawn(clock.clone(), store.clone(), graph.clone()),
            graph,
            genesis_id,
            client,
            peer_manager_addr,
            chain_info: Default::default(),
            tier2: connection::Pool::new(config.node_id()),
            tier1: connection::Pool::new(config.node_id()),
            inbound_handshake_permits: Arc::new(tokio::sync::Semaphore::new(LIMIT_PENDING_PEERS)),
            peer_store,
            accounts_data: Arc::new(accounts_data::Cache::new()),
            routing_table_view: RoutingTableView::new(store, config.node_id()),
            routing_table_exchange_helper: Default::default(),
            tier1_route_back: Mutex::new(RouteBackCache::default()),
            tier1_recv_limiter: rate::Limiter::new(
                clock,
                rate::Limit {
                    qps: (20 * bytesize::MIB) as f64,
                    burst: (40 * bytesize::MIB) as u64,
                },
            ),
            config,
            txns_since_last_block: AtomicUsize::new(0),
        }
    }

    pub fn propose_edge(&self, peer1: &PeerId, with_nonce: Option<u64>) -> PartialEdgeInfo {
        // When we create a new edge we increase the latest nonce by 2 in case we miss a removal
        // proposal from our partner.
        let nonce = with_nonce.unwrap_or_else(|| {
            self.routing_table_view.get_local_edge(peer1).map_or(1, |edge| edge.next())
        });
        PartialEdgeInfo::new(&self.config.node_id(), peer1, nonce, &self.config.node_key)
    }

    /// Stops peer instance if it is still connected,
    /// and then mark peer as banned in the peer store.
    pub fn disconnect_and_ban(
        &self,
        clock: &time::Clock,
        peer_id: &PeerId,
        ban_reason: ReasonForBan,
    ) {
        let tier2 = self.tier2.load();
        if let Some(peer) = tier2.ready.get(peer_id) {
            peer.stop(Some(ban_reason));
        } else {
            if let Err(err) = self.peer_store.peer_ban(clock, peer_id, ban_reason) {
                tracing::error!(target: "network", ?err, "Failed to save peer data");
            }
        }
    }

    /// Removes the connection from the state.
    pub fn unregister(
        &self,
        clock: &time::Clock,
        conn: &Arc<connection::Connection>,
        ban_reason: Option<ReasonForBan>,
    ) {
        let peer_id = conn.peer_info.id.clone();
        if conn.tier == tcp::Tier::T1 {
            // There is no banning or routing table for TIER1.
            // Just remove the connection from the network_state.
            self.tier1.remove(conn);
            return;
        }
        self.tier2.remove(conn);

        // If the last edge we have with this peer represent a connection addition, create the edge
        // update that represents the connection removal.
        if let Some(edge) = self.routing_table_view.get_local_edge(&peer_id) {
            if edge.edge_type() == EdgeState::Active {
                let edge_update = edge.remove_edge(self.config.node_id(), &self.config.node_key);
                self.add_verified_edges_to_routing_table(vec![edge_update.clone()]);
                self.tier2.broadcast_message(Arc::new(PeerMessage::SyncRoutingTable(
                    RoutingTableUpdate::from_edges(vec![edge_update]),
                )));
            }
        }

        // Save the fact that we are disconnecting to the PeerStore.
        if let Err(err) = match ban_reason {
            Some(ban_reason) => self.peer_store.peer_ban(&clock, &conn.peer_info.id, ban_reason),
            None => self.peer_store.peer_disconnected(clock, &conn.peer_info.id),
        } {
            tracing::error!(target: "network", ?err, "Failed to save peer data");
        }
    }

    /// Determine if the given target is referring to us.
    pub fn message_for_me(&self, target: &PeerIdOrHash) -> bool {
        let my_peer_id = self.config.node_id();
        match target {
            PeerIdOrHash::PeerId(peer_id) => &my_peer_id == peer_id,
            PeerIdOrHash::Hash(hash) => {
                self.routing_table_view.compare_route_back(*hash, &my_peer_id)
            }
        }
    }

    pub fn send_ping(&self, clock: &time::Clock, tier: tcp::Tier, nonce: u64, target: PeerId) {
        let body = RoutedMessageBody::Ping(Ping { nonce, source: self.config.node_id() });
        let msg = RawRoutedMessage { target: PeerIdOrHash::PeerId(target), body };
        self.send_message_to_peer(clock, tier, self.sign_message(clock, msg));
    }

    pub fn send_pong(&self, clock: &time::Clock, tier: tcp::Tier, nonce: u64, target: CryptoHash) {
        let body = RoutedMessageBody::Pong(Pong { nonce, source: self.config.node_id() });
        let msg = RawRoutedMessage { target: PeerIdOrHash::Hash(target), body };
        self.send_message_to_peer(clock, tier, self.sign_message(clock, msg));
    }

    pub fn sign_message(&self, clock: &time::Clock, msg: RawRoutedMessage) -> Box<RoutedMessageV2> {
        Box::new(msg.sign(
            &self.config.node_key,
            self.config.routed_message_ttl,
            Some(clock.now_utc()),
        ))
    }

    /// Route signed message to target peer.
    /// Return whether the message is sent or not.
    pub fn send_message_to_peer(
        &self,
        clock: &time::Clock,
        tier: tcp::Tier,
        msg: Box<RoutedMessageV2>,
    ) -> bool {
        let my_peer_id = self.config.node_id();

        // Check if the message is for myself and don't try to send it in that case.
        if let PeerIdOrHash::PeerId(target) = &msg.target {
            if target == &my_peer_id {
                tracing::debug!(target: "network", account_id = ?self.config.validator.as_ref().map(|v|v.account_id()), ?my_peer_id, ?msg, "Drop signed message to myself");
                metrics::CONNECTED_TO_MYSELF.inc();
                return false;
            }
        }
        match tier {
            tcp::Tier::T1 => {
                tracing::debug!(target:"test", "sending msg over TIER1");
                let peer_id = match &msg.target {
                    PeerIdOrHash::Hash(hash) => {
                        match self.tier1_route_back.lock().remove(clock, hash) {
                            Some(peer_id) => peer_id,
                            None => return false,
                        }
                    }
                    PeerIdOrHash::PeerId(peer_id) => peer_id.clone(),
                };
                return self.tier1.send_message(peer_id, Arc::new(PeerMessage::Routed(msg)));
            }
            tcp::Tier::T2 => match self.routing_table_view.find_route(&clock, &msg.target) {
                Ok(peer_id) => {
                    // Remember if we expect a response for this message.
                    if msg.author == my_peer_id && msg.expect_response() {
                        tracing::trace!(target: "network", ?msg, "initiate route back");
                        self.routing_table_view.add_route_back(&clock, msg.hash(), my_peer_id);
                    }
                    return self.tier2.send_message(peer_id, Arc::new(PeerMessage::Routed(msg)));
                }
                Err(find_route_error) => {
                    // TODO(MarX, #1369): Message is dropped here. Define policy for this case.
                    metrics::MessageDropped::NoRouteFound.inc(&msg.body);

                    tracing::debug!(target: "network",
                          account_id = ?self.config.validator.as_ref().map(|v|v.account_id()),
                          to = ?msg.target,
                          reason = ?find_route_error,
                          known_peers = ?self.routing_table_view.reachable_peers(),
                          msg = ?msg.body,
                        "Drop signed message"
                    );
                    return false;
                }
            },
        }
    }

    /// Send message to specific account.
    /// Return whether the message is sent or not.
    pub fn send_message_to_account(
        &self,
        clock: &time::Clock,
        account_id: &AccountId,
        msg: RoutedMessageBody,
    ) -> bool {
        if tcp::Tier::T1.is_allowed_routed(&msg) {
            tracing::debug!(target:"test", "got TIER1 message to send");
            if let Some((target, conn)) = self.get_tier1_proxy(account_id) {
                tracing::debug!(target:"test", "found TIER1 proxy");
                // TODO(gprusak): in case of PartialEncodedChunk, consider stripping everything
                // but the header. This will bound the message size
                conn.send_message(Arc::new(PeerMessage::Routed(self.sign_message(
                    clock,
                    RawRoutedMessage { target: PeerIdOrHash::PeerId(target), body: msg.clone() },
                ))));
            }
        }

        let target = match self.routing_table_view.account_owner(account_id) {
            Some(peer_id) => peer_id,
            None => {
                // TODO(MarX, #1369): Message is dropped here. Define policy for this case.
                metrics::MessageDropped::UnknownAccount.inc(&msg);
                tracing::debug!(target: "network",
                       account_id = ?self.config.validator.as_ref().map(|v|v.account_id()),
                       to = ?account_id,
                       ?msg,"Drop message: unknown account",
                );
                tracing::trace!(target: "network", known_peers = ?self.routing_table_view.get_accounts_keys(), "Known peers");
                return false;
            }
        };

        let msg = RawRoutedMessage { target: PeerIdOrHash::PeerId(target), body: msg };
        let msg = self.sign_message(clock, msg);
        if msg.body.is_important() {
            let mut success = false;
            for _ in 0..IMPORTANT_MESSAGE_RESENT_COUNT {
                success |= self.send_message_to_peer(clock, tcp::Tier::T2, msg.clone());
            }
            success
        } else {
            self.send_message_to_peer(clock, tcp::Tier::T2, msg)
        }
    }

    pub fn broadcast_accounts(&self, accounts: Vec<AnnounceAccount>) {
        let new_accounts = self.routing_table_view.add_accounts(accounts);
        tracing::debug!(target: "network", account_id = ?self.config.validator.as_ref().map(|v|v.account_id()), ?new_accounts, "Received new accounts");
        if new_accounts.len() > 0 {
            self.tier2.broadcast_message(Arc::new(PeerMessage::SyncRoutingTable(
                RoutingTableUpdate::from_accounts(new_accounts),
            )));
        }
    }

    /// Sends list of edges, from peer `peer_id` to check their signatures to `EdgeValidatorActor`.
    /// Bans peer `peer_id` if an invalid edge is found.
    /// `PeerManagerActor` periodically runs `broadcast_validated_edges_trigger`, which gets edges
    /// from `EdgeValidatorActor` concurrent queue and sends edges to be added to `RoutingTableActor`.
    pub async fn add_edges_to_routing_table(&self, edges: Vec<Edge>) -> Result<(),ReasonForBan> {
        let graph = self.graph.read();
        edges.retain(|x| !graph.has(x));
        drop(graph);
        if edges.is_empty() { return; }
        // Verify the edges in parallel on rayon.
        let (edges, ok) = rayon_spawn(move || {
            try_map(edges.into_iter().par_bridge(), |e| if e.verify() { Some(e) } else { None })
        }).await;
        self.routing_table_view.add_local_edges(&edges);
        let edges = self.routing_table_addr.send(routing::actor::Message::AddVerifiedEdges { edges }).await;
        // Don't send tombstones during the initial time.
        // Most of the network is created during this time, which results
        // in us sending a lot of tombstones to peers.
        // Later, the amount of new edges is a lot smaller.
        let skip_tombstones = self.config.skip_tombstones.map(|it| /*TODO: node start + it*/ self.clock.now() + it);
        if let Some(skip_tombstones_until) = skip_tombstones_until {
            if clock.now() < skip_tombstones_until {
                edges.retain(|edge| edge.edge_type() == EdgeState::Active);
                metrics::EDGE_TOMBSTONE_SENDING_SKIPPED.inc();
            }
        }
        // Broadcast new edges to all other peers.
        // TODO: demux
        self.tier2.broadcast_message(Arc::new(
            PeerMessage::SyncRoutingTable(RoutingTableUpdate::from_edges(
                filtered_edges,
            )),
        ));
        if !ok {
            return Err(ReasonForBan::InvalidEdge);
        }
        Ok(())
    }    

    pub async fn update_routing_table(
        &self,
        prune_unreachable_since: Option<time::Instant>,
        prune_edges_older_than: Option<time::Utc>,
    ) {
        let resp = self.state.routing_table_addr.send(
            routing::actor::Message::RoutingTableUpdate {
                prune_unreachable_since,
                prune_edges_older_than,
            },
        ).await.unwrap();
        match resp {
            routing::actor::Response::RoutingTableUpdateResponse {
                pruned_edges,
                next_hops,
            } => {
                self.routing_table_view.update(&pruned_edges, next_hops.clone());
                self.config
                    .event_sink
                    .push(Event::RoutingTableUpdate { next_hops, pruned_edges });
            }
            _ => panic!("expected RoutingTableUpdateResponse"),
        }
    }

    /// Check for edges indicating that
    /// a) there is a peer we should be connected to, but we aren't
    /// b) there is an edge indicating that we should be disconnected from a peer, but we are connected.
    /// Try to resolve the inconsistency.
    pub async fn update_local_edges() {
        let local_edges = self.routing.table_view.get_local_edges(); 
        let tier2 = self.tier2.load();
        for edge in local_edges {
            let other_peer = edge.other(&self.my_peer_id).unwrap();
            match (tier2.ready.contains_key(other_peer), edge.edge_type()) {
                // This is an active connection, while the edge indicates it shouldn't.
                (true, EdgeState::Removed) => {
                    let nonce = edge.next();
                    state.tier2.send_message(
                        other.clone(),
                        Arc::new(PeerMessage::RequestUpdateNonce(PartialEdgeInfo::new(
                            &self.my_peer_id,
                            other,
                            nonce,
                            &self.config.node_key,
                        ))),
                    );
                    // wait for update with timeout. 
                    if let Some(peer) = state.tier2.load().ready.get(&other) {
                        // Send disconnect signal to this peer if we haven't edge update.
                        peer.stop(None);
                    }
                }
                // We are not connected to this peer, but routing table contains
                // information that we do. We should wait and remove that peer
                // from routing table
                (false, EdgeState::Active) => {
                    // This edge says this is an connected peer, which is currently not in the set of connected peers.
                    // Wait for some time to let the connection begin or broadcast edge removal instead.
                    wait(WAIT_PEER_BEFORE_REMOVE.try_into().unwrap());
                    let other = edge.other(&act.my_peer_id).unwrap();
                    if state.tier2.load().ready.contains_key(other) {
                        return;
                    }
                    // Peer is still not connected after waiting a timeout.
                    let new_edge = edge.remove_edge(act.my_peer_id.clone(), &act.config.node_key);
                    act.state.tier2.broadcast_message(Arc::new(PeerMessage::SyncRoutingTable(
                        RoutingTableUpdate::from_edges(vec![new_edge]),
                    )));
                }
                // OK
                _ => {}
            }
        }
    }
}
