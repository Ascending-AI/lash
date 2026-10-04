//! Read-only protobuf projections of the pinned Restate 1.7.13 control API.
//! Tags come from crates/{core,types}/protobuf in the v1.7.13 server tree.
//! Unknown fields are retained by the server; this observer only needs identity,
//! replication and partition leadership. No admin SQL pseudo-tables are used.
use anyhow::{Context, Result, ensure};
use prost::Message;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Clone, PartialEq, Message, Serialize)]
pub struct Number {
    #[prost(uint64, tag = "1")]
    pub value: u64,
}
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct NodeId {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(uint32, optional, tag = "2")]
    pub generation: Option<u32>,
}
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct Address {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub address: String,
}
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct Ident {
    #[prost(int32, tag = "1")]
    pub status: i32,
    #[prost(message, optional, tag = "2")]
    pub node_id: Option<NodeId>,
    #[prost(string, tag = "3")]
    pub cluster_name: String,
    #[prost(string, repeated, tag = "4")]
    pub roles: Vec<String>,
    #[prost(uint32, tag = "10")]
    pub nodes_config_version: u32,
    #[prost(uint32, tag = "11")]
    pub logs_version: u32,
    #[prost(uint32, tag = "13")]
    pub partition_table_version: u32,
    #[prost(message, repeated, tag = "15")]
    pub advertised_addresses: Vec<Address>,
}
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct Partition {
    #[prost(int32, tag = "3")]
    pub effective_mode: i32,
    #[prost(message, optional, tag = "4")]
    pub epoch: Option<Number>,
    #[prost(message, optional, tag = "5")]
    pub leader: Option<NodeId>,
    #[prost(int32, tag = "17")]
    pub detailed_mode: i32,
}
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct Alive {
    #[prost(message, optional, tag = "1")]
    pub node_id: Option<NodeId>,
    #[prost(btree_map = "uint32, message", tag = "3")]
    pub partitions: BTreeMap<u32, Partition>,
}
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct NodeState {
    #[prost(message, optional, tag = "1")]
    pub alive: Option<Alive>,
}
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct State {
    #[prost(message, optional, tag = "2")]
    pub nodes_config_version: Option<Number>,
    #[prost(message, optional, tag = "3")]
    pub partition_table_version: Option<Number>,
    #[prost(btree_map = "uint32, message", tag = "4")]
    pub nodes: BTreeMap<u32, NodeState>,
}
#[derive(Clone, PartialEq, Message)]
pub struct StateResponse {
    #[prost(message, optional, tag = "1")]
    pub state: Option<State>,
}
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct Replication {
    #[prost(string, tag = "1")]
    pub property: String,
}
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct Bifrost {
    #[prost(string, tag = "1")]
    pub provider: String,
    #[prost(message, optional, tag = "2")]
    pub replication: Option<Replication>,
}
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct Configuration {
    #[prost(uint32, tag = "1")]
    pub partitions: u32,
    #[prost(message, optional, tag = "2")]
    pub replication: Option<Replication>,
    #[prost(message, optional, tag = "3")]
    pub bifrost: Option<Bifrost>,
}
#[derive(Clone, PartialEq, Message)]
pub struct ConfigurationResponse {
    #[prost(message, optional, tag = "1")]
    pub configuration: Option<Configuration>,
}

pub async fn unary<T: Message + Default>(peer: &str, method: &str) -> Result<T> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .http2_prior_knowledge()
        .timeout(Duration::from_secs(2))
        .build()?;
    let response = client
        .post(format!("{peer}/{method}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(vec![0_u8; 5])
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "control RPC {method} failed: {}",
        response.status()
    );
    if let Some(status) = response.headers().get("grpc-status") {
        ensure!(status == "0", "control RPC {method} rejected: {status:?}");
    }
    let bytes = response.bytes().await?;
    ensure!(
        bytes.len() >= 5 && bytes[0] == 0,
        "control RPC has no uncompressed protobuf response"
    );
    let length = usize::try_from(u32::from_be_bytes(bytes[1..5].try_into()?))?;
    ensure!(
        bytes.len() == length + 5,
        "control RPC returned an incomplete or non-unary response"
    );
    T::decode(&bytes[5..]).context("decode pinned control protobuf")
}
