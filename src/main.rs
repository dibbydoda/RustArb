#![warn(clippy::all, clippy::nursery, clippy::cargo)]

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use deadpool_sqlite::{Config, Runtime};
use ethers::prelude::Address;
use petgraph::stable_graph::NodeIndex;

use crate::graph::{create_graph, find_shortest_path};
use crate::protocols::{generate_protocols, get_all_reserves, update_all_pairs};

mod graph;
mod pair;
mod protocols;

const URL: &str = "wss://moonbeam.api.onfinality.io/ws?apikey=e1452126-1bc9-409a-b663-a7ae8e150c8b";
const PROTOCOLS_PATH: &str = "protocols.json";
const TRADED_TOKEN: &str = "0xAcc15dC74880C9944775448304B263D191c6077F";

#[tokio::main]
async fn main() {
    let provider = ethers::providers::Provider::connect(URL).await.unwrap();
    let client = Arc::new(provider);
    let cfg = Config::new("pair_data.db");
    let pool = Arc::new(cfg.create_pool(Runtime::Tokio1).unwrap());
    let protocols = generate_protocols(client.clone(), PROTOCOLS_PATH, pool.clone())
        .await
        .unwrap();
    let protocols = update_all_pairs(protocols, client.clone()).await.unwrap();
    let protocols = get_all_reserves(protocols).await.unwrap();

    let target = Address::from_str(TRADED_TOKEN).unwrap();

    let now = Instant::now();
    let mut nodes: HashMap<Address, NodeIndex> = HashMap::new();
    let graph = create_graph(&protocols, &mut nodes, target).unwrap();
    let amt: u128 = 100_000_000_000_000_000_000;
    let shortest = find_shortest_path(&graph, nodes, &target, amt.into()).unwrap();
    let outputs = shortest.get_amounts_out(amt.into()).unwrap();

    dbg!(&shortest);
    dbg!(outputs);
    let elapsed = now.elapsed();

    println!("Elapsed: {:.5?}", elapsed);
}
