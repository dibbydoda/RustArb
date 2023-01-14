use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};
use std::iter::zip;
use std::sync::Arc;

use crate::trade::TradeParams::{ExactInput, ExactOutput};
use crate::trade::{FoundTrades, SwapExact, SwapForExact, Trade, TradeParams, TradeType};
use crate::v2protocol::Protocol;
use anyhow::{anyhow, ensure, Result};
use ethers::abi::{Detokenize, Param, Tokenizable};
use ethers::prelude::*;
use serde::{Deserialize, Serialize};

pub type WSClient = Arc<Provider<Ws>>;

const ROUTER_MAP: &str = "router_mappings.json";

struct FilteredTransactions<'a> {
    protocol: &'a mut Protocol,
    transactions: Vec<Transaction>,
}

impl<'a> FilteredTransactions<'a> {
    fn new(protocol: &'a mut Protocol) -> Self {
        let transactions = Vec::new();
        Self {
            protocol,
            transactions,
        }
    }

    async fn decode_transactions(
        self,
        transaction_lookup: Arc<HashMap<String, TradeType>>,
    ) -> Result<FoundTrades> {
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
            let gas = transaction.gas;
            let to = transaction
                .to
                .ok_or_else(|| anyhow!("Trade should have to parameter"))?;
            let trade = Trade::new(to, transaction.from, params, gas, self.protocol.factory.address())?;
            trades.push(trade);
        }

        Ok(FoundTrades::new(self.protocol.factory.address(), trades))
    }
}

async fn get_all_transactions(client: WSClient) -> Result<Vec<Transaction>> {
    let mut transactions = Vec::new();
    let txpool: TxpoolContent = client.request("txpool_content", ()).await?;
    for sender in txpool.pending.into_values() {
        transactions.extend(sender.into_values());
    }
    Ok(transactions)
}

fn filter_router_transactions(
    transactions: Vec<Transaction>,
    protocols: Vec<&mut Protocol>,
) -> Vec<FilteredTransactions> {
    let mut router_addresses = HashMap::new();
    for protocol in protocols {
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

pub async fn get_all_trades(
    client: WSClient,
    protocols: Vec<&mut Protocol>,
) -> Result<Vec<FoundTrades>> {
    let tx_lookup: HashMap<String, TradeType> =
        serde_json::from_str(tokio::fs::read_to_string(ROUTER_MAP).await?.as_str())?;
    let transactions = get_all_transactions(client.clone()).await?;
    let mut found_trades = Vec::with_capacity(protocols.len());
    let mut futures = Vec::new();

    let filtered = filter_router_transactions(transactions, protocols);
    let tx_arc = Arc::new(tx_lookup);

    for filter in filtered.into_iter() {
        futures.push(filter.decode_transactions(tx_arc.clone()));
    }

    let outcome = futures::future::join_all(futures).await;

    for item in outcome {
        found_trades.push(item?);
    }
    Ok(found_trades)
}

#[derive(Deserialize, Serialize, Debug)]
struct TxpoolContent {
    pub pending: BTreeMap<H160, BTreeMap<String, Transaction>>,
    pub queued: BTreeMap<H160, BTreeMap<String, Transaction>>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub hash: H256,
    pub nonce: U256,
    pub block_hash: Option<H256>,
    pub block_number: Option<U256>,
    pub from: H160,
    pub to: Option<H160>,
    pub value: U256,
    pub gas_price: U256,
    pub gas: U256,
    pub input: Bytes,
    pub transaction_index: Option<U256>,
}
