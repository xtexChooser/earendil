use clap::{arg, Subcommand};
use earendil_crypt::{ClientId, HavenFingerprint, RelayFingerprint};
use earendil_packet::Dock;

#[derive(Subcommand)]
pub enum ControlCommand {
    /// Prints the information of all hosted havens
    HavensInfo,

    /// Send a GlobalRpc request to a destination.
    GlobalRpc {
        #[arg(long)]
        id: Option<String>,
        #[arg(short, long)]
        dest: RelayFingerprint,
        #[arg(short, long)]
        method: String,
        args: Vec<String>,
    },

    /// Insert a rendezvous haven locator into the dht.
    InsertRendezvous {
        #[arg(short, long)]
        identity_sk: String,
        #[arg(short, long)]
        onion_pk: String,
        #[arg(short, long)]
        rendezvous_fingerprint: RelayFingerprint,
    },

    /// Looks up a rendezvous haven locator.
    GetRendezvous {
        #[arg(short, long)]
        key: HavenFingerprint,
    },

    /// Insert and get a randomly generated HavenLocator.
    RendezvousHavenTest,

    /// Dumps the graph.
    GraphDump {
        #[arg(long)]
        human: bool,
    },

    /// Dumps my own routes.
    MyRoutes,

    /// Lists debts between you and your neighbors
    ListDebts,

    /// Interactive chat for talking to immediate neighbors
    Chat {
        #[command(subcommand)]
        chat_command: ChatCommand,
    },
}

#[derive(Subcommand)]
pub enum ChatCommand {
    /// print a summary of all your conversations
    List,

    /// start an interactive chat session with a neighbor
    Start {
        /// The fingerprint or client id of the neighbor to start a chat with.
        /// Accepts prefixes.
        prefix: String,
    },

    /// Pulls conversation between you and neighboring client
    GetClient {
        #[arg(short, long)]
        neighbor: ClientId,
    },

    /// Pulls conversation between you and neighboring relay
    GetRelay {
        #[arg(short, long)]
        neighbor: RelayFingerprint,
    },

    /// Sends a single chat message to client
    SendClient {
        #[arg(short, long)]
        dest: ClientId,
        #[arg(short, long)]
        msg: String,
    },

    /// Sends a single chat message to relay
    SendRelay {
        #[arg(short, long)]
        dest: RelayFingerprint,
        #[arg(short, long)]
        msg: String,
    },
}
