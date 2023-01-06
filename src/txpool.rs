use std::collections::HashMap;
use std::iter::zip;
use std::sync::Arc;

use crate::txpool::TradeParams::{ExactInput, ExactOutput};
use crate::v2protocol::Protocol;
use anyhow::{anyhow, ensure, Result};
use ethers::abi::{ethabi, Detokenize, InvalidOutputType, Param, Token, Tokenizable, Uint};
use ethers::prelude::*;

pub type WSClient = Arc<Provider<Ws>>;

enum TradeParams {
    ExactInput(SwapExact),
    ExactOutput(SwapForExact),
}

enum Gas {
    Legacy(),
    London()
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

        Ok(SwapForExact {
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

        Ok(SwapExact {
            amount_in,
            amount_out_min,
            path,
            to,
            deadline,
        })
    }
}

struct FilteredTransactions<'a> {
    protocol: &'a Protocol,
    transactions: Vec<Transaction>,
}

impl<'a> FilteredTransactions<'a> {
    const fn new(protocol: &'a Protocol) -> Self {
        let transactions = Vec::new();
        FilteredTransactions {
            protocol,
            transactions,
        }
    }
}

struct Trade {
    to: Address,
    from: Address,
    params: TradeParams,
    gas: Gas
}

async fn get_all_transactions(client: WSClient) -> Result<Vec<Transaction>> {
    let mut transactions = Vec::new();
    let txpool = client.txpool_content().await?;
    for sender in txpool.pending.into_values() {
        transactions.extend(sender.into_values());
    }
    Ok(transactions)
}

fn filter_router_transactions<'a>(
    transactions: Vec<Transaction>,
    protocols: Vec<&Protocol>,
) -> Vec<FilteredTransactions> {
    let mut router_addresses = HashMap::new();
    for protocol in protocols {
        let filtered = FilteredTransactions::new(protocol);
        router_addresses.insert(protocol.router.address(), filtered);
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

async fn decode_router_transactions(filtered: FilteredTransactions) -> Result<Vec<Trade>> {
    let trades = Vec::new();
    for transaction in filtered.transactions {
        let params = match decode_trade_params(&filtered.protocol.router, transaction)? {
            Some(param) => param,
            None => continue,
        };

        }
    }
}

fn decode_trade_params(
    router: &ethers::contract::Contract<WSClient>,
    transaction: Transaction,
) -> Result<Option<TradeParams>> {
    let signature: Selector = transaction.input[0..4].try_into()?;
    let function_name = &router
        .methods
        .get(&signature)
        .ok_or_else(|| anyhow!("Selector not found in function"))?
        .0;
    let mut inputs = router.decode_with_selector_raw(signature, transaction.input)?;
    let params = get_params_from_name(function_name, router)?;
    ensure!(
        inputs.len() == params.len(),
        "Inputs do not match parameters"
    );
    ensure!(
        zip(inputs.clone(), params.clone())
            .all(|(token, parameter)| token.type_check(&parameter.kind)),
        "Inputs do not match expected parameter types"
    );

    if function_name.starts_with("swapExact") {
        if params[0].name != "amountIn" {
            inputs.insert(0, transaction.value.into_token());
        }
        let outcome = SwapExact::from_tokens(inputs)?;
        Ok(Some(ExactInput(outcome)))
    } else if function_name.starts_with("swap") && (params.len() == 4 || params.len() == 5) {
        if params[1].name != "amountInMax" {
            inputs.insert(1, transaction.value.into_token());
        }
        let outcome = SwapForExact::from_tokens(inputs)?;
        Ok(Some(ExactOutput(outcome)))
    } else {
        Ok(None)
    }
}

fn get_params_from_name(
    name: &String,
    contract: &ethers::contract::Contract<WSClient>,
) -> Result<Vec<Param>> {
    let function = contract.abi().function(name)?;
    let params = function.inputs.clone();
    Ok(params)
}
