mod spider;

use std::time::{Duration, Instant};

use anyhow::Context;
use async_recursion::async_recursion;
use dashmap::DashSet;
use earendil_crypt::{ClientId, RelayFingerprint};
use earendil_packet::{PeeledPacket, RawBody, RawPacket};
use smol::channel::Receiver;

use crate::{
    context::{CtxField, DaemonContext, MY_RELAY_IDENTITY, MY_RELAY_ONION_SK, RELAY_GRAPH},
    n2r,
};

use self::spider::Spider;

/// Dumps a raw packet onto the network with its next peeler, trying our best to have it go in the right direction.
pub async fn send_raw(
    ctx: &DaemonContext,
    packet: RawPacket,
    next_peeler: RelayFingerprint,
) -> anyhow::Result<()> {
    if ctx.init().is_client() {
        let next_hop = one_hop_closer(ctx, next_peeler).context("failed to get next hop")?;
        ctx.get(RELAY_SPIDER)
            .send(&next_hop, (packet, next_peeler))
            .context(format!("failed to send packet to next hop {next_hop}"))?;
    } else {
        let my_fp = ctx
            .get(MY_RELAY_IDENTITY)
            .expect("only relays have global identities")
            .public()
            .fingerprint();

        if next_peeler == my_fp {
            // todo: don't allow ourselves to be the first hop when choosing forward routes
            if let Err(e) = incoming_raw(ctx, next_peeler, packet).await {
                anyhow::bail!("incoming_raw failed with: {e}")
            }
        } else {
            let next_hop = one_hop_closer(ctx, next_peeler)?;
            match ctx.get(RELAY_SPIDER).send(&next_hop, (packet, next_peeler)) {
                Ok(_) => (),
                Err(e) => {
                    let relays = ctx.get(RELAY_SPIDER).keys();
                    println!("network.rs 48: RELAY_SPIDER: {:?}", relays);
                    anyhow::bail!(e)
                }
            }
        }
    }
    Ok(())
}

#[tracing::instrument(skip(ctx, pkt), fields(packet_hash=debug(blake3::hash(bytemuck::bytes_of(&pkt)))))]
#[async_recursion]
pub async fn incoming_raw(
    ctx: &DaemonContext,
    next_peeler: RelayFingerprint,
    pkt: RawPacket,
) -> anyhow::Result<()> {
    tracing::trace!("incoming raw packet!");
    static PKTS_SEEN: CtxField<DashSet<blake3::Hash>> = |_| DashSet::new();

    let my_fp = ctx
        .get(MY_RELAY_IDENTITY)
        .expect("only relays have global identities")
        .public()
        .fingerprint();

    let pkts_seen = ctx.get(PKTS_SEEN);
    let packet_hash = blake3::hash(bytemuck::bytes_of(&pkt));
    if !pkts_seen.insert(packet_hash) {
        anyhow::bail!("received replayed pkt {packet_hash}");
    }

    tracing::trace!(my_fp = my_fp.to_string(), "on raw packet");

    if next_peeler == my_fp {
        // I am the designated peeler, peel and forward towards next peeler
        let now = Instant::now();
        let peeled: PeeledPacket = pkt.peel(ctx.get(MY_RELAY_ONION_SK))?;

        scopeguard::defer!(tracing::trace!(
            "message peel forward took {:?}",
            now.elapsed()
        ));

        match peeled {
            PeeledPacket::Relay {
                next_peeler,
                pkt,
                delay_ms,
            } => {
                let emit_time = Instant::now() + Duration::from_millis(delay_ms as u64);
                // TODO delay queue here rather than this inefficient approach
                let ctx = ctx.clone();
                smolscale::spawn(async move {
                    smol::Timer::at(emit_time).await;
                    if let Err(e) = send_raw(&ctx, pkt, next_peeler).await {
                        println!("network.rs line 102 failed with next_peeler = {next_peeler}, err = {e}");
                        anyhow::bail!(e)
                    }
                    anyhow::Ok(())
                })
                .detach();
            }
            PeeledPacket::Received { from, pkt } => {
                if let Err(e) = n2r::incoming_forward(ctx, pkt, from).await {
                    anyhow::bail!(
                        "PeelPacket::Received called n2r::incoming_forward failed with: {e}"
                    )
                }
            }
            PeeledPacket::GarbledReply {
                rb_id,
                pkt,
                client_id,
            } => {
                tracing::trace!(
                    rb_id,
                    client_id,
                    "got a GARBLED REPLY to FORWARD to the CLIENT!!!"
                );
                if let Err(e) = ctx.get(CLIENT_SPIDER).send(&client_id, (pkt, rb_id)) {
                    let clients = ctx.get(CLIENT_SPIDER).keys();
                    anyhow::bail!(
                        "PeeledPacket::GarbledReply CLIENT_SPIDER.send() failed with: {e}. CLIENT_SPIDER: {:?}", clients
                    )
                }
            }
        }
    } else {
        tracing::trace!("we are not the peeler");
        // we are not peeler, forward the packet a step closer to peeler
        let next_hop = one_hop_closer(ctx, next_peeler)?;
        tracing::trace!(
            next_hop = debug(next_hop),
            "forwarding the packet one hop closer"
        );
        ctx.get(RELAY_SPIDER)
            .send(&next_hop, (pkt, next_peeler))
            .context(format!("could not find this next hop {next_hop}"))?;
    }
    Ok(())
}

fn one_hop_closer(ctx: &DaemonContext, dest: RelayFingerprint) -> anyhow::Result<RelayFingerprint> {
    let my_neighs: Vec<RelayFingerprint> = ctx.get(RELAY_SPIDER).keys();

    if my_neighs.is_empty() {
        anyhow::bail!("cannot route one hop closer since we don't have ANY neighbors!")
    }

    let mut shortest_route_len = usize::MAX;
    let mut next_hop = None;

    for neigh in my_neighs.iter() {
        if let Some(route) = ctx.get(RELAY_GRAPH).read().find_shortest_path(neigh, &dest) {
            if route.len() < shortest_route_len {
                shortest_route_len = route.len();
                next_hop = Some(*neigh);
            }
        }
    }

    next_hop
        .context(format!("cannot route one hop closer to {:?} since none of our neighbors ({:?}) could find a route there", dest, my_neighs))
}

pub fn is_relay_neigh(ctx: &DaemonContext, neigh: RelayFingerprint) -> bool {
    ctx.get(RELAY_SPIDER).contains(&neigh)
}

pub fn is_client_neigh(ctx: &DaemonContext, neigh: ClientId) -> bool {
    ctx.get(CLIENT_SPIDER).contains(&neigh)
}

pub fn all_relay_neighs(ctx: &DaemonContext) -> Vec<RelayFingerprint> {
    ctx.get(RELAY_SPIDER).keys()
}

pub fn all_client_neighs(ctx: &DaemonContext) -> Vec<ClientId> {
    ctx.get(CLIENT_SPIDER).keys()
}

pub type RelayLinkMsg = (RawPacket, RelayFingerprint);
static RELAY_SPIDER: CtxField<Spider<RelayFingerprint, RelayLinkMsg>> = |_| Spider::new();

/// Subscribe to all outgoing messages that should be routed to the given neighboring relay.
pub fn subscribe_outgoing_relay(
    ctx: &DaemonContext,
    neigh: RelayFingerprint,
) -> Receiver<RelayLinkMsg> {
    ctx.get(RELAY_SPIDER).subscribe(neigh)
}

pub type ClientLinkMsg = (RawBody, u64);
static CLIENT_SPIDER: CtxField<Spider<ClientId, ClientLinkMsg>> = |_| Spider::new();

/// Subscribe to all outgoing messages that should be routed to the given neighboring client.
pub fn subscribe_outgoing_client(ctx: &DaemonContext, neigh: ClientId) -> Receiver<ClientLinkMsg> {
    ctx.get(CLIENT_SPIDER).subscribe(neigh)
}
