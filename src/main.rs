#![warn(clippy::all, clippy::nursery)]

use std::collections::HashMap;
use std::env;
use std::ops::Div;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use async_trait::async_trait;
use deadpool_sqlite::{Config, Pool, Runtime};
use ethers::abi::Detokenize;
use ethers::contract::abigen;
use ethers::prelude::builders::ContractCall;
use ethers::prelude::{Address, LocalWallet, Middleware, Signer, TransactionRequest, U256};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::TransactionReceipt;
use ethers::utils::parse_units;
use futures::future::join_all;
use futures::stream::StreamExt;
use futures::FutureExt;
use lazy_static::lazy_static;
use tokio::time::Instant;

use crate::pair::{generate_custom_pairs, Pair};
use crate::trade::{Gas, PossibleArbitrage};
use crate::v2protocol::{generate_protocols, update_all_pairs, Protocol, WSClient};

mod graph;
mod pair;
mod trade;
mod v2protocol;

// const URL: &str = "wss://moonbeam.api.onfinality.io/ws?apikey=e1452126-1bc9-409a-b663-a7ae8e150c8b";

lazy_static! {
    static ref URL: String = env::var("URL").unwrap();
    static ref ARBITRAGE_CONTRACT: String = env::var("ARBITRAGE_CONTRACT").unwrap();
}

const PROTOCOLS_PATH: &str = "protocols.json";
const DB_PATH: &str = "pair_data.db";
const CUSTOM_PAIRS: &str = "custom_pairs.json";
const GAS_ESTIMATE: u32 = 500000;

const TOKENS_TO_TRY: [&str; 6] = ["", "", "", "", "", ""];

abigen!(erc20, "abis/erc20.json");
abigen!(ArbContract, "abis/BlockStartArb.json");

#[tokio::main]
async fn main() {
    dotenv::dotenv().expect("MISSING .env FILE");

    let provider = ethers::providers::Provider::connect(URL.as_str())
        .await
        .unwrap();
    let client = Arc::new(provider);
    let cfg = Config::new(DB_PATH);
    let pool = Arc::new(cfg.create_pool(Runtime::Tokio1).unwrap());

    let arbitrage_contract: ArbContract<WSClient> = ArbContract::new(
        Address::from_str(ARBITRAGE_CONTRACT.as_str()).unwrap(),
        Arc::new(client.clone()),
    );

    let main_wallet = get_wallet().unwrap();

    let mut block_subscription = client.subscribe_blocks().await.unwrap();
    let mut last_update_time = Instant::now();

    .get_all_reserves().await.unwrap();
    let chain_id = client.get_chainid().await.unwrap();
    loop {
        if last_update_time.elapsed() > Duration::from_secs(3600) {
            last_update_time = Instant::now();
            updtate protocols etc
            .get_all_reserves().await.unwrap();
        } else if let Some(_block) = block_subscription.next().now_or_never() {
            .get_all_reserves().await.unwrap();
            println!("Got new reserves");
        }

        match profitable_trade {
            None => continue,
            Some(trade) => {
                execute_trade(
                    trade,
                    client.clone(),
                    &tx_pool.protocols,
                    &arbitrage_contract,
                    &other_wallets,
                    chain_id,
                )
                .await
                .unwrap();
            }
        }
    }
}

async fn reload_protocols_and_pairs(
    client: WSClient,
    pool: Arc<Pool>,
) -> Result<(HashMap<Address, Protocol>, Vec<Pair>)> {
    let protocols = generate_protocols(client.clone(), PROTOCOLS_PATH, pool.clone())
        .await
        .unwrap();
    let pairs_future = tokio::spawn(generate_custom_pairs(CUSTOM_PAIRS, client.clone()));
    let protocol_future =
        tokio::spawn(async move { update_all_pairs(protocols, client.clone()).await });

    let (protocols, pairs) = tokio::join!(protocol_future, pairs_future);

    Ok((protocols??, pairs??))
}

async fn get_profitable_arbitrage<'a>(
    tx_pool: &mut TxPool<'a>,
    input_amount: U256,
) -> Option<PossibleArbitrage> {
    let arbitrages = tx_pool.get_arbitrages(input_amount).await.unwrap();
    let best_arbitrage = arbitrages
        .into_iter()
        .max_by_key(|arbitrage| arbitrage.profit.saturating_sub(arbitrage.gas_in_eth));

    match best_arbitrage {
        None => None,
        Some(arbitrage) => {
            if arbitrage.profit.saturating_sub(arbitrage.gas_in_eth) > 0.into() {
                Some(arbitrage)
            } else {
                None
            }
        }
    }
}

fn get_wallet() -> Result<LocalWallet> {
    let private_key = env::var("KEYMAIN")?;
    let main_wallet = LocalWallet::from_str(private_key.as_str())?;
    Ok(main_wallet)
}

#[async_trait]
trait SendRaw {
    async fn send_raw(
        self,
        signer: &LocalWallet,
        client: WSClient,
    ) -> Result<Option<TransactionReceipt>>;
}

#[async_trait]
impl<D: Detokenize + Send + Sync, C: Sync + Send> SendRaw for ContractCall<C, D> {
    async fn send_raw(
        mut self,
        signer: &LocalWallet,
        client: WSClient,
    ) -> Result<Option<TransactionReceipt>> {
        let nonce = client.get_transaction_count(signer.address(), None).await?;
        self.tx.set_nonce(nonce);
        let signature = signer.sign_transaction(&self.tx).await?;
        let tx = self.tx.rlp_signed(&signature);

        let pending = client.send_raw_transaction(tx).await?.await?;
        Ok(pending)
    }
}

async fn execute_trade(
    arb: PossibleArbitrage,
    client: WSClient,
    protocols: &HashMap<Address, Protocol>,
    arb_contract: &ArbContract<WSClient>,
    accounts: &[LocalWallet],
    chain_id: U256,
) -> Result<()> {
    let balance_to_spend = arb.input;
    let min_output = balance_to_spend.saturating_add(arb.gas_in_eth);
    let pool_path: Vec<(Address, u32)> = arb
        .path
        .pair_order
        .iter()
        .map(|lookup| {
            let pair = protocols
                .get(&lookup.factory_address)
                .unwrap()
                .pairs
                .get(&lookup.pair_addresses)
                .unwrap();
            (pair.contract.address(), pair.fee)
        })
        .collect();

    let pool_order: Vec<Address> = pool_path.iter().map(|item| item.0).collect();
    let fee_order: Vec<U256> = pool_path.iter().map(|item| U256::from(item.1)).collect();

    let deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs()
        + 120;

    let mut futures = Vec::with_capacity(accounts.len());
    let mut call = arb_contract.attempt_arbitrage(
        balance_to_spend,
        min_output,
        arb.path.token_order,
        pool_order,
        fee_order,
        U256::from(deadline),
    );

    let gassed_call = match arb.gas {
        Gas::Legacy(price) => call.legacy().gas_price(price),
        Gas::London(max_fee, max_priority_fee) => match call.tx {
            TypedTransaction::Eip1559(tx) => {
                call.tx = TypedTransaction::Eip1559(
                    tx.max_fee_per_gas(max_fee)
                        .max_priority_fee_per_gas(max_priority_fee),
                );
                call
            }
            _ => {
                bail!("Typed transaction should only be EIP1559")
            }
        },
    };
    let mut call = gassed_call.gas(GAS_ESTIMATE);
    call.tx.set_chain_id(chain_id.as_u64());

    for account in accounts {
        let fut = call.clone().send_raw(account, client.clone());
        futures.push(fut);
    }

    let receipts = join_all(futures).await;

    dbg!(receipts);

    Ok(())
}
