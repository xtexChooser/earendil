use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use bytes::Bytes;
use earendil_crypt::RelayFingerprint;
use earendil_topology::{AdjacencyDescriptor, IdentityDescriptor};
use itertools::Itertools;
use moka::sync::{Cache, CacheBuilder};
use rand::seq::SliceRandom;
use rand::thread_rng;
use tap::TapOptional;

use crate::{
    context::{CtxField, DaemonContext, MY_RELAY_IDENTITY, RELAY_GRAPH},
    daemon::{inout_route::link_protocol::LinkClient, link::Link},
};

#[tracing::instrument(skip_all)]
pub async fn gossip_once(
    ctx: &DaemonContext,
    link: &Link,
    remote_fp: Option<RelayFingerprint>,
) -> anyhow::Result<()> {
    if let Some(remote_fp) = remote_fp {
        fetch_identity(ctx, link, remote_fp).await?;
        sign_adjacency(ctx, link, remote_fp).await?;
    }
    gossip_graph(ctx, link).await?;

    Ok(())
}

// Step 1: Fetch the identity of the neighbor.
#[tracing::instrument(skip_all)]
async fn fetch_identity(
    ctx: &DaemonContext,
    link: &Link,
    remote_fp: RelayFingerprint,
) -> anyhow::Result<()> {
    tracing::trace!("fetching identity...");
    let their_id = LinkClient(link.rpc_transport())
        .identity(remote_fp)
        .await?
        .context("relay neighbors should give us their own id!!!")?;
    ctx.get(RELAY_GRAPH).write().insert_identity(their_id)?;
    Ok(())
}

// Step 2: Sign an adjacency descriptor with the neighbor if the local node is "left" of the neighbor.
#[tracing::instrument(skip_all)]
async fn sign_adjacency(
    ctx: &DaemonContext,
    link: &Link,
    remote_fp: RelayFingerprint,
) -> anyhow::Result<()> {
    if let Some(my_sk) = ctx.get(MY_RELAY_IDENTITY).as_ref() {
        tracing::trace!("signing adjacency...");
        let my_fp = my_sk.public().fingerprint();
        if my_fp < remote_fp {
            tracing::trace!("signing adjacency with {remote_fp}");
            let mut left_incomplete = AdjacencyDescriptor {
                left: my_fp,
                right: remote_fp,
                left_sig: Bytes::new(),
                right_sig: Bytes::new(),
                unix_timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            };
            left_incomplete.left_sig = my_sk.sign(left_incomplete.to_sign().as_bytes());
            let complete = LinkClient(link.rpc_transport())
                .sign_adjacency(left_incomplete)
                .await?
                .context("remote refused to sign off")?;
            ctx.get(RELAY_GRAPH)
                .write()
                .insert_adjacency(complete.clone())?;
        }
    } else {
        tracing::trace!("skipping signing adjacency...");
    }
    Ok(())
}

// Step 3: Gossip the relay graph, by asking info about random nodes.
#[tracing::instrument(skip_all)]
async fn gossip_graph(ctx: &DaemonContext, link: &Link) -> anyhow::Result<()> {
    tracing::trace!("gossipping relay graph...");
    let all_known_nodes = ctx.get(RELAY_GRAPH).read().all_nodes().collect_vec();
    let random_sample = all_known_nodes
        .choose_multiple(&mut thread_rng(), 10.min(all_known_nodes.len()))
        .copied()
        .collect_vec();
    let adjacencies = LinkClient(link.rpc_transport())
        .adjacencies(random_sample)
        .await?;
    for adjacency in adjacencies {
        let left_fp = adjacency.left;
        let right_fp = adjacency.right;

        static IDENTITY_CACHE: CtxField<Cache<RelayFingerprint, IdentityDescriptor>> = |_| {
            CacheBuilder::default()
                .time_to_live(Duration::from_secs(60))
                .build()
        };
        let ourselves = ctx.get(MY_RELAY_IDENTITY);
        let left_id = if ourselves.is_some() && ourselves.unwrap().public().fingerprint() == left_fp
        {
            None
        } else if ctx.get(IDENTITY_CACHE).get(&left_fp).is_some() {
            None
        } else {
            let val = LinkClient(link.rpc_transport())
                .identity(left_fp)
                .await?
                .tap_some(|id| ctx.get(IDENTITY_CACHE).insert(left_fp, id.clone()));
            val
        };

        let right_id =
            if ourselves.is_some() && ourselves.unwrap().public().fingerprint() == right_fp {
                None
            } else if ctx.get(IDENTITY_CACHE).get(&right_fp).is_some() {
                None
            } else {
                let val = LinkClient(link.rpc_transport())
                    .identity(right_fp)
                    .await?
                    .tap_some(|id| ctx.get(IDENTITY_CACHE).insert(right_fp, id.clone()));
                val
            };

        // fetch and insert the identities. we unconditionally do this since identity descriptors may change over time
        if let Some(left_id) = left_id {
            ctx.get(RELAY_GRAPH).write().insert_identity(left_id)?
        }

        if let Some(right_id) = right_id {
            ctx.get(RELAY_GRAPH).write().insert_identity(right_id)?
        }

        // insert the adjacency
        ctx.get(RELAY_GRAPH).write().insert_adjacency(adjacency)?
    }
    Ok(())
}
