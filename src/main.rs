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
use crate::txpool::TxPool;
use crate::v2protocol::{generate_protocols, update_all_pairs, Protocol, WSClient};

mod graph;
mod pair;
mod trade;
mod txpool;
mod v2protocol;

// const URL: &str = "wss://moonbeam.api.onfinality.io/ws?apikey=e1452126-1bc9-409a-b663-a7ae8e150c8b";

lazy_static! {
    static ref URL: String = env::var("URL").unwrap();
    static ref TRADED_TOKEN: String = env::var("TRADED").unwrap();
    static ref ARBITRAGE_CONTRACT: String = env::var("ARBITRAGE_CONTRACT").unwrap();
    static ref TRANSACTION_ATTEMPTS: u8 =
        u8::from_str(env::var("TX_ATTEMPTS").unwrap().as_str()).unwrap();
    static ref BALANCE_RESERVE: U256 =
        U256::from_dec_str(env::var("BALANCE_RESERVE").unwrap().as_str()).unwrap();
}

const PROTOCOLS_PATH: &str = "protocols.json";
const DB_PATH: &str = "pair_data.db";
const CUSTOM_PAIRS: &str = "custom_pairs.json";
const GAS_ESTIMATE: u32 = 500000;

abigen!(erc20, "abis/erc20.json");
abigen!(ArbContract, "abis/ArbContract.json");

#[tokio::main]
async fn main() {
    dotenv::dotenv().expect("MISSING .env FILE");

    let provider = ethers::providers::Provider::connect(URL.as_str())
        .await
        .unwrap();
    let client = Arc::new(provider);
    let provider_ref = client.as_ref();
    let cfg = Config::new(DB_PATH);
    let pool = Arc::new(cfg.create_pool(Runtime::Tokio1).unwrap());

    let traded_token: erc20<WSClient> = erc20::new(
        Address::from_str(TRADED_TOKEN.as_str()).unwrap(),
        Arc::new(client.clone()),
    );
    let arbitrage_contract: ArbContract<WSClient> = ArbContract::new(
        Address::from_str(ARBITRAGE_CONTRACT.as_str()).unwrap(),
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

    let mut balance_to_spend = traded_token
        .balance_of(arbitrage_contract.address())
        .call()
        .await
        .unwrap();

    let mut block_subscription = client.subscribe_blocks().await.unwrap();
    let mut last_update_time = Instant::now();
    let mut tx_pool = TxPool::new(client.clone(), provider_ref, pool.clone())
        .await
        .unwrap();
    tx_pool.get_all_reserves().await.unwrap();
    let chain_id = client.get_chainid().await.unwrap();
    loop {
        if last_update_time.elapsed() > Duration::from_secs(3600) {
            last_update_time = Instant::now();
            tx_pool = TxPool::new(client.clone(), provider_ref, pool.clone())
                .await
                .unwrap();
            tx_pool.get_all_reserves().await.unwrap();
        } else if let Some(block) = block_subscription.next().now_or_never() {
            tx_pool.get_all_reserves().await.unwrap();
            let tx_hashes = block.expect("No block?").transactions;
            tx_pool.remove_done_trades(tx_hashes).await.unwrap();
            tx_pool.mark_unsimulated();
            println!("Got new reserves");
        }

        let profitable_trade = get_profitable_arbitrage(&mut tx_pool, balance_to_spend).await;

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

                balance_to_spend = traded_token
                    .balance_of(arbitrage_contract.address())
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
    let protocol_future =
        tokio::spawn(async move { update_all_pairs(protocols, client.clone()).await });

    let (protocols, pairs) = tokio::join!(protocol_future, pairs_future);

    Ok((protocols??, pairs??))
}

fn estimate_gas(gas: Gas) -> U256 {
    let gas_price = match gas {
        Gas::Legacy(price) => price,
        Gas::London(max_fee, _max_priority_fee) => max_fee,
    };
    let gas_estimate = U256::from(GAS_ESTIMATE);
    let gas_for_success = gas_estimate.saturating_mul(gas_price);
    let gas_for_fail = gas_estimate.div(8).saturating_mul(gas_price);
    gas_for_success.saturating_add(gas_for_fail.saturating_mul((*TRANSACTION_ATTEMPTS - 1).into()))
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

async fn ensure_gas_reserves(
    client: WSClient,
    main_account: &LocalWallet,
    other_accounts: &[LocalWallet],
    arb_contract: &ArbContract<WSClient>,
) -> Result<()> {
    let current_main_reserve = client.get_balance(main_account.address(), None).await?;

    let low_accounts = futures::stream::iter(other_accounts.iter())
        .filter(|account| async {
            client.get_balance(account.address(), None).await.unwrap() < *BALANCE_RESERVE
        })
        .collect::<Vec<&LocalWallet>>()
        .await;

    let top_ups = low_accounts.len() + (current_main_reserve < *BALANCE_RESERVE) as usize;

    if top_ups > 0 {
        let gas_price = client.get_gas_price().await?;
        let amount = BALANCE_RESERVE.saturating_mul(top_ups.into());
        let tx = arb_contract.withdraw_eth(amount).gas_price(gas_price);
        let receipt: TransactionReceipt = tx.send_raw(main_account, client.clone()).await?.unwrap();
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
    let mut wallets = Vec::with_capacity(*TRANSACTION_ATTEMPTS as usize);
    let private_key = env::var("KEYMAIN")?;
    let main_wallet = LocalWallet::from_str(private_key.as_str())?;
    for i in 1..=*TRANSACTION_ATTEMPTS {
        let key_str = format!("KEY{}", i);
        let private_key = env::var(key_str)?;
        wallets.push(LocalWallet::from_str(private_key.as_str())?);
    }
    Ok((main_wallet, wallets))
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
