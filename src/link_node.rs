mod gossip;
mod inout_route;
mod link;
mod link_protocol;
mod link_protocol_impl;
mod link_store;
mod pascal;
mod payment_system;
mod relay_loop;
mod route_util;
mod send_msg;

pub mod stats;
mod tests;
mod types;

use inout_route::process_out_route;
use link_protocol::LinkClient;
pub use link_store::*;
use moka::sync::Cache;
use payment_system::PaymentSystemSelector;
use relay_loop::relay_loop;
use send_msg::send_to_nonself_next_peeler;

use std::{collections::HashMap, sync::Arc, time::Duration};
use types::LinkNodeCtx;

use anyhow::Context as _;
use clone_macro::clone;
use dashmap::DashMap;
use earendil_crypt::{AnonEndpoint, RelayFingerprint, RemoteId};
use earendil_packet::{
    crypt::DhSecret, InnerPacket, Message, PrivacyConfig, RawPacket, ReplyDegarbler, Surb,
};
use earendil_topology::RelayGraph;
use itertools::Itertools;
use parking_lot::RwLock;
use smol::{
    channel::{Receiver, Sender},
    future::FutureExt as _,
};
use smolscale::immortal::{Immortal, RespawnStrategy};
use stdcode::StdcodeSerializeExt;

use crate::link_node::route_util::{forward_route_to, route_to_instructs};

use self::{link::LinkMessage, stats::StatsGatherer};
pub use payment_system::{Dummy, OnChain, PaymentSystem, PoW};
use rand::prelude::*;
pub use types::{IncomingMsg, LinkConfig, NeighborId, NeighborIdSecret};
/// An implementation of the link-level interface.
pub struct LinkNode {
    ctx: LinkNodeCtx,
    _task: Immortal,
    send_raw: Sender<LinkMessage>,
    recv_incoming: Receiver<IncomingMsg>,

    route_cache: Cache<(AnonEndpoint, RelayFingerprint), Arc<Vec<RelayFingerprint>>>,
}

impl LinkNode {
    /// Creates a new link node.
    pub fn new(mut cfg: LinkConfig) -> anyhow::Result<Self> {
        let (send_raw, recv_raw) = smol::channel::bounded(1);
        let (send_incoming, recv_incoming) = smol::channel::bounded(1);
        let store = smolscale::block_on(LinkStore::new(cfg.db_path.clone()))?;
        let relay_graph = match smol::future::block_on(store.get_misc("relay-graph"))
            .ok()
            .flatten()
            .and_then(|s| stdcode::deserialize(&s).ok())
        {
            Some(graph) => graph,
            None => RelayGraph::new(),
        };

        let my_idsk = if let Some((idsk, _)) = cfg.relay_config.clone() {
            NeighborIdSecret::Relay(idsk)
        } else {
            // I am a client with a persistent ClientId
            let mut rng = rand::thread_rng();
            let new_client_id = rng.gen_range(1..u64::MAX);
            let client_id = smol::future::block_on(
                store.get_or_insert_misc("my-client-id", new_client_id.to_le_bytes().to_vec()),
            )
            .map(|s| {
                let arr = s.try_into().expect("slice with incorrect length");
                u64::from_le_bytes(arr)
            })
            .unwrap();
            NeighborIdSecret::Client(client_id)
        };

        let mut payment_systems = PaymentSystemSelector::new();
        for payment_system in cfg.payment_systems.drain(..) {
            payment_systems.insert(payment_system);
        }
        let ctx = LinkNodeCtx {
            cfg: Arc::new(cfg),
            my_id: my_idsk,
            relay_graph: Arc::new(RwLock::new(relay_graph)),
            my_onion_sk: DhSecret::generate(),
            link_table: Arc::new(DashMap::new()),
            store: Arc::new(store),
            payment_systems: Arc::new(payment_systems),

            send_task_semaphores: Default::default(),
            stats_gatherer: Arc::new(StatsGatherer::default()),
        };
        let _task = Immortal::respawn(
            RespawnStrategy::Immediate,
            clone!([ctx, send_raw, recv_raw], move || link_node_loop(
                ctx.clone(),
                send_raw.clone(),
                recv_raw.clone(),
                send_incoming.clone(),
            )),
        );
        Ok(Self {
            ctx,
            send_raw,
            recv_incoming,
            route_cache: Cache::builder()
                .time_to_live(Duration::from_secs(10))
                .build(),
            _task,
        })
    }

    /// Sends a "forward" packet, which could be either a message or a batch of reply blocks.
    pub async fn send_forward(
        &self,
        packet: InnerPacket,
        src: AnonEndpoint,
        dest_relay: RelayFingerprint,
    ) -> anyhow::Result<()> {
        let privacy_config = self.privacy_config();

        let route = self
            .route_cache
            .try_get_with((src, dest_relay), || {
                let relay_graph = self.ctx.relay_graph.read();
                let route = forward_route_to(&relay_graph, dest_relay, privacy_config.max_peelers)
                    .context("failed to create forward route")?;
                tracing::debug!("route to {dest_relay}: {:?}", route);
                anyhow::Ok(Arc::new(route))
            })
            .map_err(|e| anyhow::anyhow!(e))?;

        let (first_peeler, wrapped_onion) = {
            let relay_graph = self.ctx.relay_graph.read();
            let first_peeler = *route
                .first()
                .context("empty route, cannot obtain first peeler")?;

            let instructs =
                route_to_instructs(&relay_graph, &route).context("route_to_instructs failed")?;

            let dest_opk = relay_graph
                .identity(&dest_relay)
                .context(format!(
                    "couldn't get the identity of the destination fp {dest_relay}"
                ))?
                .onion_pk;
            (
                first_peeler,
                RawPacket::new_normal(
                    &instructs,
                    &dest_opk,
                    packet,
                    RemoteId::Anon(src),
                    privacy_config,
                )?,
            )
        };

        // send the raw packet
        self.send_raw(wrapped_onion, first_peeler).await;

        Ok(())
    }

    /// Sends a "backwards" packet, which consumes a reply block.
    pub async fn send_backwards(&self, reply_block: Surb, message: Message) -> anyhow::Result<()> {
        if let NeighborIdSecret::Relay(my_idsk) = self.ctx.my_id {
            let packet = RawPacket::new_reply(
                &reply_block,
                InnerPacket::Message(message.clone()),
                &RemoteId::Relay(my_idsk.public().fingerprint()),
            )?;
            self.send_raw(packet, reply_block.first_peeler).await;

            Ok(())
        } else {
            anyhow::bail!("we must be a relay to send backwards packets")
        }
    }

    /// Constructs a reply block back from the given relay.
    pub fn new_surb(
        &self,
        my_anon_id: AnonEndpoint,
    ) -> anyhow::Result<(Surb, u64, ReplyDegarbler)> {
        let destination = if let NeighborIdSecret::Relay(my_idsk) = self.ctx.my_id {
            my_idsk.public().fingerprint()
        } else {
            let mut lala = self.ctx.cfg.out_routes.values().collect_vec();
            lala.shuffle(&mut rand::thread_rng());
            lala.first().context("no out routes")?.fingerprint
        };
        let graph = self.ctx.relay_graph.read();
        let dest_opk = graph
            .identity(&destination)
            .context(format!(
                "destination {destination} is surprisingly not in our RelayGraph"
            ))?
            .onion_pk;

        let privacy_cfg = self.privacy_config();

        let reverse_route = forward_route_to(&graph, destination, privacy_cfg.max_peelers)?;
        let reverse_instructs = route_to_instructs(&graph, &reverse_route)?;
        let my_client_id = match self.ctx.my_id {
            NeighborIdSecret::Relay(_) => 0, // special ClientId for relays
            NeighborIdSecret::Client(id) => id,
        };

        let (surb, (id, degarbler)) = Surb::new(
            &reverse_instructs,
            reverse_route[0],
            &dest_opk,
            my_client_id,
            my_anon_id,
            privacy_cfg,
        )
        .context("cannot build reply block")?;
        Ok((surb, id, degarbler))
    }

    /// Sends a raw packet.
    async fn send_raw(&self, raw: RawPacket, next_peeler: RelayFingerprint) {
        self.send_raw
            .send(LinkMessage::ToRelay {
                packet: bytemuck::bytes_of(&raw).to_vec().into(),
                next_peeler,
            })
            .await
            .unwrap();
    }

    /// Receives an incoming message. Blocks until we have something that's for us, and not to be forwarded elsewhere.
    pub async fn recv(&self) -> IncomingMsg {
        self.recv_incoming.recv().await.unwrap()
    }

    /// Gets all the currently known relays.
    pub fn all_relays(&self) -> Vec<RelayFingerprint> {
        self.ctx.relay_graph.read().all_nodes().collect_vec()
    }

    /// Gets the current relay graph.
    pub fn relay_graph(&self) -> RelayGraph {
        self.ctx.relay_graph.read().clone()
    }

    /// Gets my identity.
    pub fn my_id(&self) -> NeighborIdSecret {
        self.ctx.my_id.clone()
    }

    /// Gets all our currently connected neighbors.
    pub fn all_neighs(&self) -> Vec<NeighborId> {
        self.ctx
            .link_table
            .iter()
            .map(|entry| *entry.key())
            .collect_vec()
    }

    /// Sends a chat message to a neighbor.
    pub async fn send_chat(&self, neighbor: NeighborId, text: String) -> anyhow::Result<()> {
        let link_entry = self
            .ctx
            .link_table
            .get(&neighbor)
            .context(format!("not connected to neighbor {:?}", neighbor))?;
        LinkClient(link_entry.0.rpc_transport())
            .push_chat(text.clone())
            .await??;
        self.ctx
            .store
            .insert_chat_entry(
                neighbor,
                ChatEntry {
                    text,
                    timestamp: chrono::offset::Utc::now().timestamp(),
                    is_outgoing: true,
                },
            )
            .await?;
        Ok(())
    }

    /// Gets the entire chat history with a neighbor.
    pub async fn get_chat_history(&self, neighbor: NeighborId) -> anyhow::Result<Vec<ChatEntry>> {
        self.ctx.store.get_chat_history(neighbor).await
    }

    pub async fn get_chat_summary(&self) -> anyhow::Result<Vec<(NeighborId, ChatEntry, u32)>> {
        self.ctx.store.get_chat_summary().await
    }

    pub async fn get_debt_summary(&self) -> anyhow::Result<HashMap<String, f64>> {
        self.ctx.store.get_debt_summary().await
    }

    pub async fn get_debt(&self, neighbor: NeighborId) -> anyhow::Result<f64> {
        self.ctx.store.get_debt(neighbor).await
    }

    pub async fn timeseries_stats(&self, key: String, start: i64, end: i64) -> Vec<(i64, f64)> {
        self.ctx.stats_gatherer.get(&key, start..end)
    }

    pub fn privacy_config(&self) -> PrivacyConfig {
        self.ctx.cfg.privacy_config
    }
}

#[tracing::instrument(skip_all)]
async fn link_node_loop(
    link_node_ctx: LinkNodeCtx,
    send_raw: Sender<LinkMessage>,
    recv_raw: Receiver<LinkMessage>,
    send_incoming: Sender<IncomingMsg>,
) -> anyhow::Result<()> {
    let link_node_ctx_clone = link_node_ctx.clone();

    // ----------------------- client + relay loops ------------------------
    let save_relay_graph_loop = async {
        loop {
            // println!("syncing DB...");
            let relay_graph = link_node_ctx_clone.relay_graph.read().stdcode();
            match link_node_ctx_clone
                .store
                .insert_misc("relay_graph".to_string(), relay_graph)
                .await
            {
                Ok(_) => (),
                Err(e) => tracing::warn!("saving relay graph failed with: {e}"),
            };
            smol::Timer::after(Duration::from_secs(10)).await;
        }
    };

    let out_task = async {
        if link_node_ctx_clone.cfg.out_routes.is_empty() {
            smol::future::pending().await
        } else {
            futures_util::future::try_join_all(link_node_ctx_clone.cfg.out_routes.iter().map(
                |(name, out_route)| {
                    process_out_route(
                        link_node_ctx_clone.clone(),
                        name,
                        out_route,
                        send_raw.clone(),
                    )
                },
            ))
            .await?;
            anyhow::Ok(())
        }
    };

    // ----------------------- relay-only loops ------------------------
    let relay_loop = relay_loop(
        link_node_ctx.clone(),
        send_raw.clone(),
        recv_raw.clone(),
        send_incoming.clone(),
    );

    let client_loop = async {
        if !matches!(link_node_ctx.my_id, NeighborIdSecret::Client(_)) {
            return smol::future::pending().await;
        }
        loop {
            let fallible = async {
                match recv_raw.recv().await? {
                    LinkMessage::ToRelay {
                        packet,
                        next_peeler,
                    } => {
                        let packet: &RawPacket = bytemuck::try_from_bytes(&packet)
                            .ok()
                            .context("could not cast")?;
                        send_to_nonself_next_peeler(&link_node_ctx, None, next_peeler, *packet)
                            .await?
                    }
                    LinkMessage::ToClient { body, rb_id } => {
                        send_incoming
                            .send(IncomingMsg::Backward { rb_id, body })
                            .await?;
                    }
                }
                anyhow::Ok(())
            };
            if let Err(err) = fallible.await {
                tracing::error!(err = debug(err), "error in client_peel_loop");
            }
        }
    };

    match relay_loop
        .race(client_loop)
        .race(out_task) // for both relay + client
        .race(save_relay_graph_loop) // for both relay + client
        .await
    {
        Ok(_) => tracing::warn!("link_node_loop() returned?"),
        Err(e) => tracing::error!("link_loop() error: {e}"),
    }
    Ok(())
}
