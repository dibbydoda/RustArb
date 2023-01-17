#![warn(clippy::all, clippy::nursery, clippy::cargo)]

use anyhow::Result;
use std::collections::HashMap;
use std::env;

use std::ops::Div;
use std::str::FromStr;

use std::sync::Arc;

use crate::graph::{create_graph, find_shortest_path, Path};
use crate::pair::{generate_custom_pairs, Pair};
use crate::trade::PossibleArbitrage;
use crate::txpool::get_all_trades;
use crate::v2protocol::{
    generate_protocols, get_all_pairs, get_all_reserves, update_all_pairs, Protocol, WSClient,
};
use async_trait::async_trait;
use deadpool_sqlite::{Config, Pool, Runtime};
use ethers::abi::Detokenize;
use ethers::contract::abigen;
use ethers::prelude::builders::ContractCall;
use ethers::prelude::{Address, LocalWallet, Middleware, Signer, TransactionRequest, U256};
use ethers::types::TransactionReceipt;
use ethers::utils::parse_units;
use futures::future::join_all;
use futures::stream::StreamExt;
use petgraph::stable_graph::NodeIndex;

mod graph;
mod pair;
mod trade;
mod txpool;
mod v2protocol;

// const URL: &str = "wss://moonbeam.api.onfinality.io/ws?apikey=e1452126-1bc9-409a-b663-a7ae8e150c8b";

const URL: &str = "ws://127.0.0.1:9944";
const PROTOCOLS_PATH: &str = "protocols.json";
const TRADED_TOKEN: &str = "0xAcc15dC74880C9944775448304B263D191c6077F";
const ARBITRAGE_CONTRACT: &str = "0xa29Ab7ffe5Ecc7468A2626bd2B44f9998F043213";
const DB_PATH: &str = "pair_data.db";
const CUSTOM_PAIRS: &str = "custom_pairs.json";
const GAS_ESTIMATE: u32 = 500000;
const TRANSACTION_ATTEMPTS: u8 = 5;

abigen!(erc20, "abis/erc20.json");
abigen!(ArbContract, "abis/ArbContract.json");

#[tokio::main]
async fn main() {
    dotenv::dotenv().expect("MISSING .env FILE");

    let provider = ethers::providers::Provider::connect(URL).await.unwrap();
    let client = Arc::new(provider);
    let cfg = Config::new(DB_PATH);
    let pool = Arc::new(cfg.create_pool(Runtime::Tokio1).unwrap());

    let traded_token: erc20<WSClient> = erc20::new(
        Address::from_str(TRADED_TOKEN).unwrap(),
        Arc::new(client.clone()),
    );
    let arbitrage_contract: ArbContract<WSClient> = ArbContract::new(
        Address::from_str(ARBITRAGE_CONTRACT).unwrap(),
        Arc::new(client.clone()),
    );

    let (main_wallet, other_wallets) = get_wallets().unwrap();
    ensure_gas_reserves(
        client.clone(),
        &main_wallet,
        &other_wallets,
        &arbitrage_contract,
    )
    .await
    .unwrap();

    let (mut protocols, pairs) = reload_protocols_and_pairs(client.clone(), pool.clone())
        .await
        .unwrap();

    let mut balance_to_spend = traded_token
        .method::<Address, U256>("balanceOf", arbitrage_contract.address())
        .unwrap()
        .call()
        .await
        .unwrap();

    dbg!(&balance_to_spend);

    loop {
        let profitable_trade =
            get_profitable_arbitrage(client.clone(), &mut protocols, &pairs, 100000.into()).await;
        dbg!(&profitable_trade);

        match profitable_trade {
            None => continue,
            Some(trade) => {
                balance_to_spend = traded_token
                    .method::<Address, U256>("balanceOf", arbitrage_contract.address())
                    .unwrap()
                    .call()
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
    let protocol_future = tokio::spawn(async move {
        let protocols = update_all_pairs(protocols, client.clone()).await?;
        get_all_reserves(protocols).await
    });

    let (protocols, pairs) = tokio::join!(protocol_future, pairs_future);
    let mut protocols_map = HashMap::new();
    for protocol in protocols?? {
        protocols_map.insert(protocol.factory.address(), protocol);
    }

    Ok((protocols_map, pairs??))
}

fn find_best_trade(
    protocols: &mut HashMap<Address, Protocol>,
    amount: U256,
    custom_pairs: &Vec<Pair>,
) -> (Path, U256) {
    let mut nodes: HashMap<Address, NodeIndex> = HashMap::new();
    let mut all_pairs = get_all_pairs(protocols.values().collect());
    let target = Address::from_str(TRADED_TOKEN).unwrap();

    all_pairs.extend(custom_pairs);

    let graph = create_graph(all_pairs, &mut nodes, target).unwrap();
    let shortest = find_shortest_path(&graph, nodes, &target, amount).unwrap();
    let outputs = shortest.get_amounts_out(amount, protocols).unwrap();

    (shortest, outputs.last().unwrap().to_owned())
}

fn estimate_gas(gas_price: U256) -> U256 {
    let gas_estimate = U256::from(GAS_ESTIMATE);
    let gas_for_success = gas_estimate.saturating_mul(gas_price);
    let gas_for_fail = gas_estimate.div(8).saturating_mul(gas_price);
    gas_for_success.saturating_add(gas_for_fail.saturating_mul((TRANSACTION_ATTEMPTS - 1).into()))
}

async fn get_profitable_arbitrage(
    client: WSClient,
    protocols: &mut HashMap<Address, Protocol>,
    custom_pairs: &Vec<Pair>,
    input_amount: U256,
) -> Option<PossibleArbitrage> {
    let trades = get_all_trades(client.clone(), protocols.values_mut().collect())
        .await
        .unwrap();

    let mut arbitrages = Vec::new();
    for trade in trades {
        arbitrages.extend(trade.simulate_trades(protocols, input_amount, custom_pairs));
    }

    let best_arbitrage = arbitrages
        .into_iter()
        .max_by_key(|arbitrage| arbitrage.output.saturating_sub(estimate_gas(arbitrage.gas)));

    match best_arbitrage {
        None => None,
        Some(arbitrage) => {
            if input_amount
                .saturating_sub(arbitrage.output)
                .saturating_sub(estimate_gas(arbitrage.gas))
                > 0.into()
            {
                Some(arbitrage)
            } else {
                None
            }
        }
    }
}

async fn ensure_gas_reserves(
    client: WSClient,
    main_account: &LocalWallet,
    other_accounts: &[LocalWallet],
    arb_contract: &ArbContract<WSClient>,
) -> Result<()> {
    let current_main_reserve = client.get_balance(main_account.address(), None).await?;

    let balance_reserve = env::var("BALANCE_RESERVE").expect("BALANCE_RESERVE missing from .env");
    let balance_reserve = U256::from_dec_str(balance_reserve.as_str())
        .expect("Balance reserve could not be interpreted as U256");

    let low_accounts = futures::stream::iter(other_accounts.iter())
        .filter(|account| async {
            client.get_balance(account.address(), None).await.unwrap() < balance_reserve
        })
        .collect::<Vec<&LocalWallet>>()
        .await;

    let top_ups = low_accounts.len() + (current_main_reserve < balance_reserve) as usize;

    if top_ups > 0 {
        let amount = balance_reserve.saturating_mul(top_ups.into());
        let tx = arb_contract.withdraw_eth(amount);
        let receipt: TransactionReceipt = tx
            .send_raw(main_account.clone(), client.clone())
            .await?
            .unwrap();
        assert_eq!(receipt.status.unwrap().as_u64(), 1);

        println!(
            "Withdrew {} wrapped token for gas.",
            parse_units(amount, "wei").unwrap()
        );

        let mut futures = Vec::with_capacity(low_accounts.len());
        for account in low_accounts {
            futures.push(pay(account.address(), amount, main_account, client.clone()))
        }

        join_all(futures).await;
    }

    Ok(())
}

async fn pay(
    receiver: Address,
    amount: U256,
    sender: &LocalWallet,
    client: WSClient,
) -> Result<TransactionReceipt> {
    let request = TransactionRequest::pay(receiver, amount);
    let signature = sender.sign_transaction(&request.clone().into()).await?;
    let tx = request.rlp_signed(&signature);
    Ok(client.send_raw_transaction(tx).await?.await?.unwrap())
}

fn get_wallets() -> Result<(LocalWallet, Vec<LocalWallet>)> {
    let mut wallets = Vec::with_capacity(TRANSACTION_ATTEMPTS as usize);
    let private_key = env::var("KEYMAIN")?;
    let main_wallet = LocalWallet::from_str(private_key.as_str())?;
    for i in 1..=TRANSACTION_ATTEMPTS {
        let key_str = format!("KEY{}", i);
        let private_key = env::var(key_str)?;
        wallets.push(LocalWallet::from_str(private_key.as_str())?);
    }
    Ok((main_wallet, wallets))
}

#[async_trait]
trait SendRaw {
    async fn send_raw(
        &self,
        signer: LocalWallet,
        client: WSClient,
    ) -> Result<Option<TransactionReceipt>>;
}

#[async_trait]
impl<D: Detokenize + Send + Sync, C: Sync + Send> SendRaw for ContractCall<C, D> {
    async fn send_raw(
        &self,
        signer: LocalWallet,
        client: WSClient,
    ) -> Result<Option<TransactionReceipt>> {
        let signature = signer.sign_transaction(&self.tx).await?;
        let tx = self.tx.rlp_signed(&signature);

        let pending = client.send_raw_transaction(tx).await?.await?;
        Ok(pending)
    }
}
