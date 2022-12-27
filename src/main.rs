use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use deadpool_sqlite::{Config, Runtime};
use ethers::prelude::Address;
use petgraph::stable_graph::NodeIndex;

use crate::graph::{create_graph, find_shortest};
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

    let mut allpairs = Vec::new();
    for protocol in &protocols {
        allpairs.extend(protocol.pairs.values());
    }

    let target = Address::from_str(TRADED_TOKEN).unwrap();
    let mut nodes: HashMap<Address, NodeIndex> = HashMap::new();
    let graph = create_graph(allpairs, &mut nodes, target).unwrap();

    let start = *nodes.get(&Address::zero()).unwrap();
    let end = *nodes.get(&target).unwrap();

    let now = Instant::now();
    let shortest = find_shortest(&graph, nodes, &target).unwrap();
    let elapsed = now.elapsed();

    let bad_edge = graph.find_edge(start, end).unwrap();
    let bad_pair = graph.edge_weight(bad_edge).unwrap();

    dbg!(bad_edge);
    dbg!(bad_pair);

    println!("Elapsed: {:.5?}", elapsed);
}
