use crate::graph::{create_graph, find_shortest_path, PairLookup, Path};
use crate::pair::Pair;
use crate::v2protocol::{get_all_pairs, Protocol};
use crate::{estimate_gas, TRADED_TOKEN};
use anyhow::{ensure, Result};
use ethers::abi::{Detokenize, InvalidOutputType, Token, Tokenizable};
use ethers::prelude::{Address, U256};
use ethers::types::H256;
use petgraph::stable_graph::NodeIndex;
use std::collections::HashMap;
use std::iter::zip;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum TradeParams {
    ExactInput(SwapExact),
    ExactOutput(SwapForExact),
}
#[derive(Debug, Clone)]
pub struct PossibleArbitrage {
    pub path: Path,
    pub gas: Gas,
    pub input: U256,
    pub output: U256,
    pub profit: U256,
    pub gas_in_eth: U256,
}

#[derive(Debug, Copy, Clone)]
pub enum Gas {
    Legacy(U256),
    London(U256, U256),
}

impl PossibleArbitrage {
    pub fn new(path: Path, gas: Gas, output: U256, input: U256) -> Self {
        let profit = output.saturating_sub(input);
        let gas_in_eth = estimate_gas(gas);
        Self {
            path,
            gas,
            output,
            input,
            profit,
            gas_in_eth,
        }
    }
}

impl Path {
    pub fn from_trade_tokens(tokens: Vec<Address>, protocol: Address) -> Result<Self> {
        let mut pair_order = Vec::with_capacity(tokens.len() - 1);
        for addresses in tokens.windows(2) {
            let mut array = [addresses[0], addresses[1]];
            array.sort_unstable();
            pair_order.push(PairLookup::new(protocol, (array[0], array[1])));
        }

        Ok(Self {
            token_order: tokens,
            pair_order,
        })
    }
}

impl TradeParams {
    pub fn get_path(&self) -> Vec<Address> {
        match self {
            Self::ExactInput(item) => item.path.clone(),
            Self::ExactOutput(item) => item.path.clone(),
        }
    }

    pub const fn get_deadline(&self) -> U256 {
        match self {
            Self::ExactInput(item) => item.deadline,
            Self::ExactOutput(item) => item.deadline,
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub enum TradeType {
    ExactEth,
    ExactOther,
    EthForExact,
    OtherForExact,
}

#[derive(Debug)]
pub struct Trade {
    pub tx_hash: H256,
    pub to: Address,
    pub from: Address,
    pub params: TradeParams,
    pub gas: Gas,
    pub path: Path,
    pub protocol: Address,
    pub simulated: bool,
}

impl Trade {
    pub fn new(
        tx_hash: H256,
        to: Address,
        from: Address,
        params: TradeParams,
        gas: Gas,
        protocol: Address,
    ) -> Result<Self> {
        let path = Path::from_trade_tokens(params.get_path(), protocol)?;
        Ok(Self {
            tx_hash,
            to,
            from,
            params,
            gas,
            path,
            protocol,
            simulated: false,
        })
    }

    pub fn simulate(&self, protocol: &mut Protocol, amounts: Vec<U256>) -> Vec<Pair> {
        let path = &self.path;
        let mut amounts = amounts.windows(2);
        let mut modified_pairs = Vec::new();
        for (input_token, pair_key) in zip(&path.token_order, &path.pair_order) {
            let both_amount = amounts.next().expect("Mismatched amount windows and pairs");
            let amount_in = both_amount[0].as_u128();
            let amount_out = both_amount[1].as_u128();

            let pair = protocol
                .pairs
                .get_mut(&pair_key.pair_addresses)
                .expect("Pair not found in protocol");
            modified_pairs.push(pair.clone());
            if input_token == &pair.get_tokens().0 {
                pair.reserve0 += amount_in;
                pair.reserve1 -= amount_out;
            } else {
                pair.reserve0 -= amount_out;
                pair.reserve1 += amount_in;
            }
        }
        modified_pairs
    }

    pub fn check_trade_validity(
        &self,
        protocols: &HashMap<Address, Protocol>,
    ) -> Result<Vec<U256>> {
        let cur_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs();
        ensure!(
            self.params.get_deadline() >= U256::from(cur_unix),
            "Deadline Expired"
        );

        let amounts = match &self.params {
            TradeParams::ExactInput(params) => {
                let amounts = self.path.get_amounts_out(params.amount_in, protocols)?;
                ensure!(
                    amounts.last().expect("Amounts should never be empty") > &params.amount_out_min
                );
                amounts
            }
            TradeParams::ExactOutput(params) => {
                let amounts = self.path.get_amounts_in(params.amount_out, protocols)?;
                ensure!(
                    amounts.first().expect("Amounts should never be empty") < &params.amount_in_max
                );
                amounts
            }
        };

        Ok(amounts)
    }
}

#[derive(Debug)]
pub struct SwapForExact {
    pub amount_out: U256,
    pub amount_in_max: U256,
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

#[derive(Debug)]
pub struct SwapExact {
    pub amount_in: U256,
    pub amount_out_min: U256,
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

pub fn find_best_trade<'a>(
    protocols: &'a mut HashMap<Address, Protocol>,
    amount: U256,
    custom_pairs: &'a Vec<Pair>,
) -> (Path, U256) {
    let mut nodes: HashMap<Address, NodeIndex> = HashMap::new();
    let all_pairs = get_all_pairs(protocols.values());
    let target = Address::from_str(TRADED_TOKEN.as_str()).unwrap();

    let pairs = all_pairs.chain(custom_pairs);

    let graph = create_graph(pairs, &mut nodes).unwrap();
    let shortest = find_shortest_path(&graph, nodes, &target, amount).unwrap();
    let outputs = shortest.get_amounts_out(amount, protocols).unwrap();

    (shortest, outputs.last().unwrap().to_owned())
}
