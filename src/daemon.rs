mod connection;
mod inout_route;
mod n2n;
mod neightable;

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::Context;
use earendil_packet::{crypt::OnionSecret, PeeledPacket};
use earendil_topology::{IdentitySecret, RelayGraph};
use futures_util::{stream::FuturesUnordered, StreamExt};
use parking_lot::RwLock;
use rand::Rng;




use crate::{
    config::{ConfigFile, InRouteConfig, OutRouteConfig},
    daemon::{
        connection::Connection,
        inout_route::{in_route_obfsudp, out_route_obfsudp, InRouteContext, OutRouteContext},
        neightable::NeighTable,
    },
};

pub fn main_daemon(config: ConfigFile) -> anyhow::Result<()> {
    fn read_identity(path: &Path) -> anyhow::Result<IdentitySecret> {
        Ok(stdcode::deserialize(&hex::decode(std::fs::read(path)?)?)?)
    }

    fn write_identity(path: &Path, identity: &IdentitySecret) -> anyhow::Result<()> {
        let encoded_identity = hex::encode(stdcode::serialize(&identity)?);
        std::fs::write(path, encoded_identity)?;
        Ok(())
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("earendil=debug"))
        .init();
    let identity = loop {
        match read_identity(&config.identity) {
            Ok(id) => break id,
            Err(err) => {
                log::warn!(
                    "(re)writing identity file at {:?} due to error reading: {:?}",
                    config.identity,
                    err
                );
                let new_id = IdentitySecret::generate();
                write_identity(&config.identity, &new_id)?;
            }
        }
    };
    log::info!(
        "daemon starting with fingerprint {}",
        identity.public().fingerprint()
    );

    smolscale::block_on(async move {
        let mut subtasks = FuturesUnordered::new();
        let table = Arc::new(NeighTable::new());

        let daemon_ctx = DaemonContext {
            table: table.clone(),
            identity: identity.into(),
            onion_sk: OnionSecret::generate(),
            relay_graph: Arc::new(RwLock::new(RelayGraph::new())),
        };

        // Run the loops
        subtasks.push({
            let table = table.clone();
            smolscale::spawn(async move {
                loop {
                    smol::Timer::after(Duration::from_secs(60)).await;
                    table.garbage_collect();
                }
            })
        });
        subtasks.push(smolscale::spawn(peel_forward_loop(daemon_ctx.clone())));
        subtasks.push(smolscale::spawn(gossip_loop(daemon_ctx.clone())));

        // For every in_routes block, spawn a task to handle incoming stuff
        for (in_route_name, config) in config.in_routes.iter() {
            let context = InRouteContext {
                in_route_name: in_route_name.clone(),
                daemon_ctx: daemon_ctx.clone(),
            };
            match config.clone() {
                InRouteConfig::Obfsudp { listen, secret } => {
                    subtasks.push(smolscale::spawn(in_route_obfsudp(context, listen, secret)));
                }
            }
        }

        // For every out_routes block, spawn a task to handle outgoing stuff
        for (out_route_name, config) in config.out_routes.iter() {
            match config {
                OutRouteConfig::Obfsudp {
                    fingerprint,
                    connect,
                    cookie,
                } => {
                    let context = OutRouteContext {
                        out_route_name: out_route_name.clone(),
                        remote_fingerprint: *fingerprint,
                        daemon_ctx: daemon_ctx.clone(),
                    };
                    subtasks.push(smolscale::spawn(out_route_obfsudp(
                        context, *connect, *cookie,
                    )));
                }
            }
        }

        while let Some(next) = subtasks.next().await {
            next?;
        }
        Ok(())
    })
}

/// Loop that takes incoming packets, peels them, and processes them
async fn peel_forward_loop(ctx: DaemonContext) -> anyhow::Result<()> {
    loop {
        let pkt = ctx.table.recv_raw_packet().await;
        let fallible = async {
            let peeled = pkt.peel(&ctx.onion_sk)?;
            match peeled {
                PeeledPacket::Forward(next_hop, inner) => {
                    let conn = ctx
                        .table
                        .lookup(&next_hop)
                        .context("could not find this next hop")?;
                    conn.send_raw_packet(inner).await?;
                }
                PeeledPacket::Receive(_) => anyhow::bail!("could not handle receiving yet"),
            }
            anyhow::Ok(())
        };
        if let Err(err) = fallible.await {
            log::warn!("could not forward incoming packet: {:?}", err)
        }
    }
}

/// Loop that gossips things around
async fn gossip_loop(ctx: DaemonContext) -> anyhow::Result<()> {
    async fn gossip_once(_conn: &Connection) -> anyhow::Result<()> {
        // Pick a random adjacency and gossip it
        anyhow::bail!("dunno how to gossip yet lol");
    }

    let mut timer = smol::Timer::interval(Duration::from_secs(1));
    loop {
        (&mut timer).await;
        let neighs = ctx.table.all_neighs();
        if neighs.is_empty() {
            log::debug!("skipping gossip due to no neighs");
            continue;
        }
        // pick a random neighbor and do sync stuff
        let rand_neigh = &neighs[rand::thread_rng().gen_range(0..neighs.len())];
        log::debug!(
            "gossiping with random neighbor {}",
            rand_neigh.remote_idpk().fingerprint()
        );
        if let Err(err) = gossip_once(rand_neigh).await {
            log::warn!(
                "gossip with {} failed: {:?}",
                rand_neigh.remote_idpk().fingerprint(),
                err
            );
        }
    }
}

#[derive(Clone)]
pub struct DaemonContext {
    table: Arc<NeighTable>,
    identity: Arc<IdentitySecret>,
    onion_sk: OnionSecret,
    relay_graph: Arc<RwLock<RelayGraph>>,
}
