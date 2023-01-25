use std::borrow::Borrow;
use std::collections::HashMap;
use std::iter::zip;
use std::sync::Arc;

use crate::pair::Pair;
use crate::reload_protocols_and_pairs;
use crate::trade::TradeParams::{ExactInput, ExactOutput};
use crate::trade::{
    find_best_trade, Gas, PossibleArbitrage, SwapExact, SwapForExact, Trade, TradeParams, TradeType,
};
use crate::v2protocol::Protocol;
use anyhow::{anyhow, ensure, Result};
use deadpool_sqlite::Pool;
use ethers::abi::{Detokenize, Param, Tokenizable};
use ethers::prelude::*;
use futures::future::{join_all, try_join_all};
use futures::FutureExt;
use rustc_hash::FxHashMap;

pub type WSClient = Arc<Provider<Ws>>;

const ROUTER_MAP: &str = "router_mappings.json";

struct FilteredTransactions<'a> {
    protocol: &'a Protocol,
    transactions: Vec<Transaction>,
}

impl<'a> FilteredTransactions<'a> {
    const fn new(protocol: &'a Protocol) -> Self {
        let transactions = Vec::new();
        Self {
            protocol,
            transactions,
        }
    }

    async fn decode_transactions(
        self,
        transaction_lookup: Arc<HashMap<String, TradeType>>,
    ) -> Result<Vec<Trade>> {
        let mut trades = Vec::new();
        for transaction in &self.transactions {
            let params = match decode_trade_params(
                self.protocol.router.borrow(),
                transaction,
                transaction_lookup.clone(),
            )? {
                Some(param) => param,
                None => continue,
            };

            let gas = match transaction.transaction_type {
                None => Gas::Legacy(
                    transaction
                        .gas_price
                        .expect("Gas price expected for legacy"),
                ),
                Some(num) => {
                    if num.as_u64() == 0 {
                        Gas::Legacy(
                            transaction
                                .gas_price
                                .expect("Gas price expected for legacy"),
                        )
                    } else {
                        Gas::London(
                            transaction
                                .max_fee_per_gas
                                .expect("MFPG expected for london"),
                            transaction
                                .max_priority_fee_per_gas
                                .expect("MPFPG expected for london"),
                        )
                    }
                }
            };
            let to = transaction
                .to
                .ok_or_else(|| anyhow!("Trade should have to parameter"))?;
            let trade = Trade::new(
                transaction.hash,
                to,
                transaction.from,
                params,
                gas,
                self.protocol.factory.address(),
            )?;
            trades.push(trade);
        }

        Ok(trades)
    }
}

fn decode_trade_params(
    router: &ethers::contract::Contract<WSClient>,
    transaction: &Transaction,
    trade_type_lookup: Arc<HashMap<String, TradeType>>,
) -> Result<Option<TradeParams>> {
    let signature: &Selector = transaction.input[0..4].try_into()?;
    let function_name = &router
        .methods
        .get(signature)
        .ok_or_else(|| anyhow!("Selector not found in function"))?
        .0;
    let mut inputs = router.decode_with_selector_raw(*signature, &transaction.input)?;
    let params = get_params_from_name(function_name, router)?;
    ensure!(
        inputs.len() == params.len(),
        "Inputs do not match parameters"
    );
    ensure!(
        zip(inputs.clone(), params).all(|(token, parameter)| token.type_check(&parameter.kind)),
        "Inputs do not match expected parameter types"
    );

    let trade_type = match trade_type_lookup.get(function_name) {
        None => return Ok(None),
        Some(trade) => trade,
    };

    let outcome = match trade_type {
        TradeType::ExactEth => {
            inputs.insert(0, transaction.value.into_token());
            SwapExact::from_tokens(inputs).map(|item| Some(ExactInput(item)))?
        }
        TradeType::ExactOther => {
            SwapExact::from_tokens(inputs).map(|item| Some(ExactInput(item)))?
        }
        TradeType::EthForExact => {
            inputs.insert(1, transaction.value.into_token());
            SwapForExact::from_tokens(inputs).map(|item| Some(ExactOutput(item)))?
        }
        TradeType::OtherForExact => {
            SwapForExact::from_tokens(inputs).map(|item| Some(ExactOutput(item)))?
        }
    };

    Ok(outcome)
}

fn get_params_from_name(
    name: &str,
    contract: &ethers::contract::Contract<WSClient>,
) -> Result<Vec<Param>> {
    let function = contract.abi().function(name)?;
    let params = function.inputs.clone();
    Ok(params)
}

pub struct TxPool<'a> {
    client: WSClient,
    watcher: Watcher<'a>,
    pub(crate) protocols: HashMap<Address, Protocol>,
    tx_lookup: Arc<HashMap<String, TradeType>>,
    trades: FxHashMap<Address, Trade>,
    custom_pairs: Vec<Pair>,
}

struct Watcher<'a> {
    client: &'a Provider<Ws>,
    watcher: TransactionStream<'a, Ws, SubscriptionStream<'a, Ws, H256>>,
}

impl<'a> Watcher<'a> {
    async fn new(client: &'a Provider<Ws>) -> Result<Watcher<'a>> {
        let watcher = client
            .subscribe_pending_txs()
            .await?
            .transactions_unordered(50);

        Ok(Self { client, watcher })
    }
}

impl<'a> TxPool<'a> {
    pub async fn new(
        client_arc: WSClient,
        client_ref: &'a Provider<Ws>,
        pool: Arc<Pool>,
    ) -> Result<TxPool<'a>> {
        let tx_lookup: HashMap<String, TradeType> =
            serde_json::from_str(tokio::fs::read_to_string(ROUTER_MAP).await?.as_str())?;
        let tx_lookup = Arc::new(tx_lookup);
        let (protocols, custom_pairs) =
            reload_protocols_and_pairs(client_arc.clone(), pool.clone())
                .await
                .unwrap();

        let watcher = Watcher::new(client_ref).await?;

        Ok(Self {
            client: client_arc,
            protocols,
            tx_lookup,
            watcher,
            trades: FxHashMap::default(),
            custom_pairs,
        })
    }

    pub async fn get_arbitrages(&mut self, input: U256) -> Result<Vec<PossibleArbitrage>> {
        self.update_trades().await?;
        Ok(self.simulate_trades(input))
    }

    async fn update_trades(&mut self) -> Result<()> {
        let new_transactions = self.get_new_transactions().await;

        let mut futures = Vec::new();
        let filtered = self.filter_router_transactions(new_transactions);
        for filter in filtered.into_iter() {
            futures.push(filter.decode_transactions(self.tx_lookup.clone()));
        }

        let new_trades: Vec<Trade> = try_join_all(futures).await?.into_iter().flatten().collect();

        for trade in new_trades {
            self.trades.insert(trade.protocol, trade);
        }

        Ok(())
    }

    async fn get_new_transactions(&mut self) -> Vec<Transaction> {
        let mut new_transactions = Vec::new();
        while let Some(Some(transaction)) = self.watcher.watcher.next().now_or_never() {
            match transaction {
                Ok(tx) => {
                    new_transactions.push(tx);
                }
                Err(_) => continue,
            };
        }
        new_transactions
    }

    fn filter_router_transactions(
        &self,
        transactions: Vec<Transaction>,
    ) -> Vec<FilteredTransactions> {
        let mut router_addresses = HashMap::new();
        for protocol in self.protocols.values() {
            let router_address = protocol.router.address();
            let filtered = FilteredTransactions::new(protocol);
            router_addresses.insert(router_address, filtered);
        }

        for transaction in transactions {
            if let Some(to) = transaction.to {
                if let Some(filtered) = router_addresses.get_mut(&to) {
                    {
                        filtered.transactions.push(transaction);
                    }
                }
            }
        }
        router_addresses.into_values().collect()
    }

    fn simulate_trades(&mut self, input_amount: U256) -> Vec<PossibleArbitrage> {
        let mut possible_arbitrages = Vec::new();
        let amounts = (1..=10).map(|num| (input_amount / U256::from(10)) * num);

        for (address, mut trade) in self.trades.iter_mut() {
            if trade.simulated {
                continue;
            }
            let checked_amounts = match trade.check_trade_validity(&self.protocols) {
                Ok(amounts) => amounts,
                Err(_) => continue,
            };

            let mut_protocol = self
                .protocols
                .get_mut(address)
                .expect("Protocol not found in protocols");
            let changed = trade.simulate(mut_protocol, checked_amounts);

            for amount in amounts.clone() {
                let (path, output) =
                    find_best_trade(&mut self.protocols, amount, &self.custom_pairs);
                possible_arbitrages.push(PossibleArbitrage::new(path, trade.gas, output, amount));
            }

            let protocol = self
                .protocols
                .get_mut(address)
                .expect("Protocol not found in protocols");
            protocol.unsimualte_trade(changed);
            trade.simulated = true
        }
        possible_arbitrages
    }

    pub fn mark_unsimulated(&mut self) {
        for trade in self.trades.values_mut() {
            trade.simulated = false;
        }
    }

    pub async fn remove_done_trades(&mut self, hashes: Vec<H256>) -> Result<()> {
        self.trades
            .retain(|_address, tx| !hashes.contains(&tx.tx_hash));
        let mut handles = Vec::new();
        for trade in self.trades.values() {
            let client_copy = self.client.clone();
            let hash = trade.tx_hash;
            handles.push(tokio::spawn(async move {
                (hash, client_copy.get_transaction(hash).await)
            }))
        }

        let outcome = join_all(handles).await;
        let mut hashes_to_remove = Vec::new();
        for item in outcome {
            let (input_hash, output) = item?;
            match output? {
                None => hashes_to_remove.push(input_hash),
                Some(tx) => {
                    if tx.block_number.is_some() {
                        hashes_to_remove.push(input_hash)
                    }
                }
            }
        }

        self.trades
            .retain(|_address, tx| !hashes_to_remove.contains(&tx.tx_hash));

        Ok(())
    }
}
