use crate::commands::{ChatCommand, ControlCommand};
use crate::socket::Endpoint;
use crate::{daemon::ControlProtErr, haven_util::HavenLocator};
use anyhow::Context;
use async_std::io::{self, WriteExt};
use async_trait::async_trait;
use blake3::Hash;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use colored::{ColoredString, Colorize};
use earendil_crypt::{Fingerprint, IdentitySecret};
use earendil_packet::{
    crypt::{OnionPublic, OnionSecret},
    Dock, PacketConstructError,
};
use nanorpc::nanorpc_derive;
use nanorpc_http::client::HttpRpcTransport;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use smol::Timer;
use smolscale::reaper::TaskReaper;
use std::marker::Send;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use std::{net::SocketAddr, str::FromStr};
use thiserror::Error;

pub async fn main_control(
    control_command: ControlCommand,
    connect: SocketAddr,
) -> anyhow::Result<()> {
    let client = ControlClient::from(HttpRpcTransport::new(connect));
    match control_command {
        ControlCommand::BindN2r {
            skt_id,
            anon_id,
            dock,
        } => {
            client.bind_n2r(skt_id, anon_id, dock).await?;
        }
        ControlCommand::BindHaven {
            skt_id,
            anon_id,
            dock,
            rendezvous,
        } => {
            client.bind_haven(skt_id, anon_id, dock, rendezvous).await?;
        }
        ControlCommand::SktInfo { skt_id } => {
            let skt_info = client.skt_info(skt_id).await??;
            println!("{skt_info}")
        }
        ControlCommand::SendMsg {
            skt_id: socket_id,
            dest: destination,
            msg: message,
        } => {
            client
                .send_message(SendMessageArgs {
                    socket_id,
                    destination,
                    content: Bytes::copy_from_slice(message.as_bytes()),
                })
                .await??;
        }
        ControlCommand::RecvMsg { skt_id: socket_id } => {
            match client.recv_message(socket_id.clone()).await? {
                Ok((msg, src)) => println!("{:?} from {}", msg, src),
                Err(e) => println!("error receiving message: {e}"),
            }
        }
        ControlCommand::GlobalRpc {
            id,
            dest: destination,
            method,
            args,
        } => {
            let args: Result<Vec<serde_json::Value>, _> =
                args.into_iter().map(|a| serde_yaml::from_str(&a)).collect();
            let args = args.context("arguments not YAML")?;
            let res = client
                .send_global_rpc(GlobalRpcArgs {
                    id,
                    destination,
                    method,
                    args,
                })
                .await??;
            println!("{res}");
        }
        ControlCommand::InsertRendezvous {
            identity_sk,
            onion_pk,
            rendezvous_fingerprint,
        } => {
            let locator = HavenLocator::new(
                IdentitySecret::from_str(&identity_sk)?,
                OnionPublic::from_str(&onion_pk)?,
                rendezvous_fingerprint,
            );
            client.insert_rendezvous(locator).await??;
        }
        ControlCommand::GetRendezvous { key } => {
            let locator = client.get_rendezvous(key).await??;
            if let Some(locator) = locator {
                println!("{:?}", locator);
            } else {
                println!("No haven locator found for fingerprint {key}")
            }
        }
        ControlCommand::RendezvousHavenTest => {
            let mut fingerprint_bytes = [0; 20];
            rand::thread_rng().fill_bytes(&mut fingerprint_bytes);
            let fingerprint = Fingerprint::from_bytes(&fingerprint_bytes);
            let id_sk = IdentitySecret::generate();
            let id_pk = id_sk.public();
            let locator = HavenLocator::new(id_sk, OnionSecret::generate().public(), fingerprint);
            eprintln!("created haven locator: {:?}", &locator);

            client.insert_rendezvous(locator.clone()).await??;
            eprintln!("inserted haven locator... sleeping for 5s");

            if let Some(fetched_locator) = client.get_rendezvous(id_pk.fingerprint()).await?? {
                eprintln!("got haven locator: {:?}", &fetched_locator);
                assert_eq!(locator.rendezvous_point, fetched_locator.rendezvous_point);
            } else {
                eprintln!("oh no couldn't find locator");
            }
        }
        ControlCommand::GraphDump { human } => {
            let res = client.graph_dump(human).await?;
            println!("{res}");
        }
        ControlCommand::MyRoutes => {
            let routes = client.my_routes().await?;
            println!("{}", serde_yaml::to_string(&routes)?);
        }
        ControlCommand::HavensInfo => {
            let havens_info = client.havens_info().await?;
            for info in havens_info {
                println!("{} - {}", info.0, info.1);
            }
        }
        ControlCommand::Chat { chat_command } => match chat_command {
            ChatCommand::List => {
                let res = client.list_chats().await?;
                println!("{res}");
            }
            ChatCommand::Start { fp_prefix } => {
                let neighbors = client.list_neighbors().await?;
                let neighbor = neigh_by_prefix(neighbors, fp_prefix);

                if let Some(neigh) = neighbor {
                    println!("<starting chat with {}>", earendil_blue(&neigh.to_string()));

                    let entries = client.get_chat(neigh).await?;
                    let mut current_hash = entries
                        .iter()
                        .last()
                        .map(|(is_mine, text, time)| {
                            let bytes = stdcode::serialize(&(is_mine, text.clone(), time)).unwrap();
                            blake3::hash(&bytes)
                        })
                        .unwrap_or(Hash::from([0; 32]));

                    for (is_mine, text, time) in entries {
                        println!("{}", pretty_entry(is_mine, text, time));
                    }

                    let client = Arc::new(client);
                    let listen_client = client.clone();

                    let reaper = TaskReaper::new();
                    let listen_loop = smolscale::spawn(async move {
                        loop {
                            let last_msg = listen_client.clone().get_latest_msg(neigh).await;
                            if let Ok(Some((is_mine, text, time))) = last_msg {
                                if is_mine {
                                    let last_bytes =
                                        stdcode::serialize(&(is_mine, text.clone(), time)).unwrap();
                                    let last_hash = blake3::hash(&last_bytes);

                                    if last_hash != current_hash {
                                        current_hash = last_hash;
                                        print!("\r");
                                        println!("{:>120}", pretty_time(time));
                                        print!("{} ", right_arrow());
                                        let _ = io::stdout().flush().await;
                                    }
                                } else {
                                    let last_bytes =
                                        stdcode::serialize(&(is_mine, text.clone(), time)).unwrap();
                                    let last_hash = blake3::hash(&last_bytes);

                                    if last_hash != current_hash {
                                        current_hash = last_hash;
                                        print!("\r");
                                        println!("{}", pretty_entry(is_mine, text, time));
                                        print!("{} ", right_arrow());
                                        let _ = io::stdout().flush().await;
                                    }
                                }
                            }

                            Timer::after(Duration::from_secs(1)).await;
                        }
                    });

                    reaper.attach(listen_loop);

                    loop {
                        print!("{} ", right_arrow());
                        io::stdout().flush().await?;

                        let mut message = String::new();
                        io::stdin()
                            .read_line(&mut message)
                            .await
                            .expect("Failed to read line");

                        let message = message.trim();

                        if !message.is_empty() {
                            let msg = message.to_string();
                            let _ = client.send_chat_msg(neigh, msg).await;
                        }
                    }
                }
            }
            ChatCommand::Get { neighbor } => {
                let entries = client.get_chat(neighbor).await?;
                for (is_mine, text, time) in entries {
                    println!("{}", pretty_entry(is_mine, text, time));
                }
            }
            ChatCommand::Send { dest, msg } => client.send_chat_msg(dest, msg).await?,
        },
    }
    Ok(())
}

fn earendil_blue(string: &str) -> ColoredString {
    string
        .custom_color(colored::CustomColor {
            r: 0,
            g: 129,
            b: 162,
        })
        .bold()
}

fn left_arrow() -> ColoredString {
    earendil_blue("<-")
}

fn right_arrow() -> ColoredString {
    earendil_blue("->")
}

fn neigh_by_prefix(fingerprints: Vec<Fingerprint>, prefix: String) -> Option<Fingerprint> {
    for fp in fingerprints {
        let fp_string = format!("{}", fp);
        if fp_string.starts_with(&prefix) {
            return Some(fp);
        }
    }
    None
}

fn pretty_entry(is_mine: bool, text: String, time: SystemTime) -> String {
    let arrow = if is_mine { right_arrow() } else { left_arrow() };

    format!("{} {} {}", arrow, text, pretty_time(time))
}

fn pretty_time(time: SystemTime) -> ColoredString {
    let datetime: DateTime<Utc> = time.into();

    format!("[{}]", datetime.format("%Y-%m-%d %H:%M:%S")).bright_yellow()
}

#[nanorpc_derive]
#[async_trait]
pub trait ControlProtocol {
    async fn bind_n2r(&self, socket_id: String, anon_id: Option<String>, dock: Option<Dock>);

    async fn bind_haven(
        &self,
        socket_id: String,
        anon_id: Option<String>,
        dock: Option<Dock>,
        rendezvous_point: Option<Fingerprint>,
    );

    async fn skt_info(&self, skt_id: String) -> Result<Endpoint, ControlProtErr>;

    async fn havens_info(&self) -> Vec<(String, String)>;

    async fn send_message(&self, args: SendMessageArgs) -> Result<(), ControlProtErr>;

    async fn recv_message(&self, socket_id: String) -> Result<(Bytes, Endpoint), ControlProtErr>;

    async fn send_global_rpc(
        &self,
        args: GlobalRpcArgs,
    ) -> Result<serde_json::Value, GlobalRpcError>;

    async fn graph_dump(&self, human: bool) -> String;

    async fn my_routes(&self) -> serde_json::Value;

    async fn insert_rendezvous(&self, locator: HavenLocator) -> Result<(), DhtError>;

    async fn get_rendezvous(
        &self,
        fingerprint: Fingerprint,
    ) -> Result<Option<HavenLocator>, DhtError>;

    async fn list_neighbors(&self) -> Vec<Fingerprint>;

    async fn list_chats(&self) -> String;

    async fn get_chat(&self, neigh: Fingerprint) -> Vec<(bool, String, SystemTime)>;

    async fn get_latest_msg(&self, neigh: Fingerprint) -> Option<(bool, String, SystemTime)>;

    async fn send_chat_msg(&self, dest: Fingerprint, msg: String);
}

#[derive(Error, Serialize, Deserialize, Debug)]
pub enum SendMessageError {
    #[error("no route to the given destination {0}")]
    NoRoute(Fingerprint),
    #[error(transparent)]
    PacketConstructError(#[from] PacketConstructError),
    #[error("no onion public key for fingerprint {0}")]
    NoOnionPublic(Fingerprint),
    #[error("failed to construct reply block")]
    ReplyBlockFailed,
    #[error("cannot use anonymous id to communicate with anonymous id")]
    NoAnonId,
}

#[derive(Error, Serialize, Deserialize, Debug)]
pub enum DhtError {
    #[error("failed to verify descriptor retrieved from DHT")]
    VerifyFailed,
    #[error("network failed: {0}")]
    NetworkFailure(String),
}

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct SendMessageArgs {
    pub socket_id: String,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub destination: Endpoint,
    #[serde_as(as = "serde_with::base64::Base64")]
    pub content: Bytes,
}

#[serde_as]
#[derive(Serialize, Deserialize)]
pub struct GlobalRpcArgs {
    pub id: Option<String>,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub destination: Fingerprint,
    pub method: String,
    pub args: Vec<serde_json::Value>,
}

#[derive(Error, Serialize, Deserialize, Debug)]
pub enum GlobalRpcError {
    #[error("error sending GlobalRpc request")]
    SendError,
}
