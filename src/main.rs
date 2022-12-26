mod protocols;
mod arbitrage;

use std::sync::Arc;
use deadpool_sqlite::{Config, Runtime};
use std::time::Instant;
use crate::protocols::{generate_protocols, get_all_reserves, update_all_pairs};

const URL: &str = "wss://moonbeam.api.onfinality.io/ws?apikey=e1452126-1bc9-409a-b663-a7ae8e150c8b";
const PROTOCOLS_PATH: &str = "protocols.json";

#[tokio::main]
async fn main() {
    let now = Instant::now();
    let provider = ethers::providers::Provider::connect(URL).await.unwrap();
    let client = Arc::new(provider);
    let cfg = Config::new("pair_data.db");
    let pool = Arc::new(cfg.create_pool(Runtime::Tokio1).unwrap());
    let protocols = generate_protocols(client.clone(), PROTOCOLS_PATH, pool.clone()).await.unwrap();
    let protocols = update_all_pairs(protocols, client.clone()).await;
    let protocols = get_all_reserves(protocols).await;

    dbg!(protocols.len());

    let elapsed = now.elapsed();
    println!("Elapsed: {:.5?}", elapsed);
}





