#![warn(clippy::all, clippy::nursery, clippy::cargo)]

use anyhow::Result;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use deadpool_sqlite::{Config, Pool, Runtime};
use ethers::prelude::Address;
use petgraph::stable_graph::NodeIndex;

use crate::graph::{create_graph, find_shortest_path};
use crate::pair::{generate_custom_pairs, Pair};
use crate::v2protocol::{
    generate_protocols, get_all_reserves, update_all_pairs, Protocol, WSClient,
};

mod graph;
mod pair;
mod txpool;
mod v2protocol;

const URL: &str = "wss://moonbeam.api.onfinality.io/ws?apikey=e1452126-1bc9-409a-b663-a7ae8e150c8b";
const PROTOCOLS_PATH: &str = "protocols.json";
const TRADED_TOKEN: &str = "0xAcc15dC74880C9944775448304B263D191c6077F";
const DB_PATH: &str = "pair_data.db";
const CUSTOM_PAIRS: &str = "custom_pairs.json";

#[tokio::main]
async fn main() {
    let provider = ethers::providers::Provider::connect(URL).await.unwrap();
    let client = Arc::new(provider);
    let cfg = Config::new(DB_PATH);
    let pool = Arc::new(cfg.create_pool(Runtime::Tokio1).unwrap());
    let (protocols, _) = reload_protocols_and_pairs(client.clone(), pool.clone())
        .await
        .unwrap();

    dbg!(protocols[3]
        .router
        .abi()
        .functions
        .keys()
        .collect::<Vec<&String>>());
}

async fn reload_protocols_and_pairs(
    client: WSClient,
    pool: Arc<Pool>,
) -> Result<(Vec<Protocol>, Vec<Pair>)> {
    let protocols = generate_protocols(client.clone(), PROTOCOLS_PATH, pool.clone())
        .await
        .unwrap();
    let pairs_future = tokio::spawn(generate_custom_pairs(CUSTOM_PAIRS, client.clone()));
    let protocol_future = tokio::spawn(async move {
        let protocols = update_all_pairs(protocols, client.clone()).await?;
        get_all_reserves(protocols).await
    });

    let (protocol, pairs) = tokio::join!(protocol_future, pairs_future);
    Ok((protocol??, pairs??))
}

/*
let mut nodes: HashMap<Address, NodeIndex> = HashMap::new();

let graph = create_graph(&protocols, &mut nodes, target).unwrap();
let amt: u128 = 100_000_000_000_000_000_000;
let shortest = find_shortest_path(&graph, nodes, &target, amt.into()).unwrap();
let outputs = shortest.get_amounts_out(amt.into()).unwrap();

dbg!(&shortest);
dbg!(outputs);
*/
