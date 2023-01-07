use std::collections::HashMap;
use std::iter::zip;
use std::sync::Arc;

use crate::txpool::Gas::{Legacy, London};
use crate::txpool::TradeParams::{ExactInput, ExactOutput};
use crate::v2protocol::Protocol;
use anyhow::{anyhow, ensure, Result};
use ethers::abi::{Detokenize, InvalidOutputType, Param, Token, Tokenizable};
use ethers::prelude::*;
use serde::{Deserialize, Serialize};

pub type WSClient = Arc<Provider<Ws>>;

const ROUTER_MAP: &str = "router_mappings.json";

enum TradeParams {
    ExactInput(SwapExact),
    ExactOutput(SwapForExact),
}

#[derive(Deserialize, Serialize)]
pub enum TradeType {
    ExactEth,
    ExactOther,
    EthForExact,
    OtherForExact,
}

enum Gas {
    Legacy(U256),
    London(U256, U256),
}

struct Trade {
    to: Address,
    from: Address,
    params: TradeParams,
    gas: Gas,
    protocol: Arc<Protocol>,
}

impl Trade {
    fn new(
        to: Address,
        from: Address,
        params: TradeParams,
        gas: Gas,
        protocol: Arc<Protocol>,
    ) -> Self {
        Self {
            to,
            from,
            params,
            gas,
            protocol,
        }
    }
}

struct SwapForExact {
    amount_out: U256,
    amount_in_max: U256,
    path: Vec<Address>,
    to: Address,
    deadline: U256,
}

impl Detokenize for SwapForExact {
    fn from_tokens(tokens: Vec<Token>) -> std::result::Result<Self, InvalidOutputType>
    where
        Self: Sized,
    {
        if tokens.len() != 5 {
            return Err(InvalidOutputType("Incorrect number of tokens".to_string()));
        }
        let amount_out: U256 = U256::from_token(tokens[0].clone())?;
        let amount_in_max: U256 = U256::from_token(tokens[1].clone())?;
        let path: Vec<Address> = Vec::from_token(tokens[2].clone())?;
        let to: Address = Address::from_token(tokens[3].clone())?;
        let deadline: U256 = U256::from_token(tokens[4].clone())?;

        Ok(Self {
            amount_out,
            amount_in_max,
            path,
            to,
            deadline,
        })
    }
}

struct SwapExact {
    amount_in: U256,
    amount_out_min: U256,
    path: Vec<Address>,
    to: Address,
    deadline: U256,
}

impl Detokenize for SwapExact {
    fn from_tokens(tokens: Vec<Token>) -> std::result::Result<Self, InvalidOutputType>
    where
        Self: Sized,
    {
        if tokens.len() != 5 {
            return Err(InvalidOutputType("Incorrect number of tokens".to_string()));
        }
        let amount_in: U256 = U256::from_token(tokens[0].clone())?;
        let amount_out_min: U256 = U256::from_token(tokens[1].clone())?;
        let path: Vec<Address> = Vec::from_token(tokens[2].clone())?;
        let to: Address = Address::from_token(tokens[3].clone())?;
        let deadline: U256 = U256::from_token(tokens[4].clone())?;

        Ok(Self {
            amount_in,
            amount_out_min,
            path,
            to,
            deadline,
        })
    }
}

struct FilteredTransactions {
    protocol: Arc<Protocol>,
    transactions: Vec<Transaction>,
}

impl FilteredTransactions {
    const fn new(protocol: Arc<Protocol>) -> Self {
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
                &self.protocol.router,
                transaction,
                transaction_lookup.clone(),
            )? {
                Some(param) => param,
                None => continue,
            };
            let gas = match &transaction.transaction_type {
                None => Legacy(
                    transaction
                        .gas_price
                        .ok_or_else(|| anyhow!("Legacy transaction must have gas price"))?,
                ),
                Some(t) => {
                    if t.as_u64() == 2 {
                        London(
                            transaction
                                .max_fee_per_gas
                                .ok_or_else(|| anyhow!("London transaction must have max fee"))?,
                            transaction.max_priority_fee_per_gas.ok_or_else(|| {
                                anyhow!("London transaction must have max priority fee")
                            })?,
                        )
                    } else {
                        continue;
                    }
                }
            };
            let to = transaction
                .to
                .ok_or_else(|| anyhow!("Trade should have to parameter"))?;
            let trade = Trade::new(to, transaction.from, params, gas, self.protocol.clone());
            trades.push(trade);
        }

        Ok(trades)
    }
}

async fn get_all_transactions(client: WSClient) -> Result<Vec<Transaction>> {
    let mut transactions = Vec::new();
    let txpool = client.txpool_content().await?;
    for sender in txpool.pending.into_values() {
        transactions.extend(sender.into_values());
    }
    Ok(transactions)
}

fn filter_router_transactions(
    transactions: Vec<Transaction>,
    protocols: Vec<Arc<Protocol>>,
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

async fn get_all_trades(client: WSClient, protocols: Vec<Arc<Protocol>>) -> Result<Vec<Trade>> {
    let transactions = get_all_transactions(client.clone()).await?;
    let filtered = filter_router_transactions(transactions, protocols);
    let tx_lookup: HashMap<String, TradeType> =
        serde_json::from_str(tokio::fs::read_to_string(ROUTER_MAP).await?.as_str())?;

    let tx_arc = Arc::new(tx_lookup);
    let mut handles = Vec::new();

    for filter in filtered {
        handles.push(tokio::spawn(filter.decode_transactions(tx_arc.clone())));
    }

    let mut trades = Vec::new();
    let outcome = futures::future::join_all(handles).await;

    for item in outcome {
        trades.extend(item??);
    }
    Ok(trades)
}
