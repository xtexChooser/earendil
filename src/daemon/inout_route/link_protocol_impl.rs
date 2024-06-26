use async_trait::async_trait;

use earendil_crypt::{ClientId, RelayFingerprint};

use earendil_topology::{AdjacencyDescriptor, IdentityDescriptor};

use itertools::Itertools;

use crate::daemon::chat::{ChatEntry, CHATS};
use crate::settlement::{Seed, SettlementRequest, SettlementResponse};
use crate::{
    context::{DaemonContext, MY_RELAY_IDENTITY, RELAY_GRAPH},
    network::is_relay_neigh,
};

use super::link_protocol::{InfoResponse, LinkProtocol};

const LABEL_LINK_RPC: &str = "link-rpc";

pub struct LinkProtocolImpl {
    pub ctx: DaemonContext,

    pub remote_client_id: ClientId,
    pub remote_relay_fp: Option<RelayFingerprint>,
}

#[async_trait]
impl LinkProtocol for LinkProtocolImpl {
    async fn info(&self) -> InfoResponse {
        InfoResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    #[tracing::instrument(skip(self))]
    async fn sign_adjacency(
        &self,
        mut left_incomplete: AdjacencyDescriptor,
    ) -> Option<AdjacencyDescriptor> {
        let my_sk = self
            .ctx
            .get(MY_RELAY_IDENTITY)
            .expect("only relays have global identities");
        let my_fp = my_sk.public().fingerprint();
        // This must be a neighbor that is "left" of us
        let valid = left_incomplete.left < left_incomplete.right
            && left_incomplete.right == my_fp
            && is_relay_neigh(&self.ctx, left_incomplete.left);
        if !valid {
            tracing::debug!("neighbor not right of us! Refusing to sign adjacency x_x");
            return None;
        }
        // Fill in the right-hand-side
        let signature = my_sk.sign(left_incomplete.to_sign().as_bytes());
        left_incomplete.right_sig = signature;

        self.ctx
            .get(RELAY_GRAPH)
            .write()
            .insert_adjacency(left_incomplete.clone())
            .map_err(|e| {
                tracing::warn!("could not insert here: {:?}", e);
                e
            })
            .ok()?;
        Some(left_incomplete)
    }

    async fn identity(&self, fp: RelayFingerprint) -> Option<IdentityDescriptor> {
        self.ctx.get(RELAY_GRAPH).read().identity(&fp)
    }

    #[tracing::instrument(skip(self))]
    async fn adjacencies(&self, fps: Vec<RelayFingerprint>) -> Vec<AdjacencyDescriptor> {
        let rg = self.ctx.get(RELAY_GRAPH).read();
        fps.into_iter()
            .flat_map(|fp| rg.adjacencies(&fp).into_iter().flatten())
            .dedup()
            .collect()
    }

    #[tracing::instrument(skip(self))]
    async fn start_settlement(&self, _req: SettlementRequest) -> Option<SettlementResponse> {
        todo!()
        // let settlements = self.ctx.get(SETTLEMENTS);

        // match req.payment_proof {
        //     SettlementProof::Automatic(_) => {
        //         tracing::debug!("handling auto_settlement req: {:?}", req);
        //         if let Ok(res) = settlements.verify_auto_settle(&self.ctx, req) {
        //             res
        //         } else {
        //             None
        //         }
        //     }
        //     SettlementProof::Manual => {
        //         tracing::debug!("handling manual settlement req: {:?}", req);
        //         let recv_res = settlements.insert_pending(req);

        //         if let Ok(recv_res) = recv_res {
        //             match recv_res.recv().timeout(Duration::from_secs(300)).await {
        //                 Some(Ok(res)) => res,
        //                 Some(Err(e)) => {
        //                     log::warn!("settlement response receive error: {e}");
        //                     None
        //                 }
        //                 None => None,
        //             }
        //         } else {
        //             None
        //         }
        //     }
        // }
    }

    #[tracing::instrument(skip(self))]
    async fn push_chat(&self, msg: String) {
        if let Some(fingerprint) = self.remote_relay_fp {
            self.ctx
                .get(CHATS)
                .record(either::Right(fingerprint), ChatEntry::new_incoming(msg));
        } else {
            self.ctx.get(CHATS).record(
                either::Left(self.remote_client_id),
                ChatEntry::new_incoming(msg),
            );
        }
    }

    #[tracing::instrument(skip(self))]
    async fn request_seed(&self) -> Option<Seed> {
        todo!()
        // let seed = rand::thread_rng().gen();
        // let seed_cache = &self.ctx.get(SETTLEMENTS).seed_cache;

        // match seed_cache.get(&self.remote_client_id) {
        //     Some(mut seeds) => {
        //         seeds.insert(seed);
        //         seed_cache.insert(self.remote_client_id, seeds);
        //         Some(seed)
        //     }
        //     None => {
        //         let mut seed_set = HashSet::new();
        //         seed_set.insert(seed);
        //         seed_cache.insert(self.remote_client_id, seed_set);
        //         Some(seed)
        //     }
        // }
    }
}
