use crate::find_best_trade;
use crate::graph::{PairLookup, Path};
use crate::pair::Pair;
use crate::v2protocol::Protocol;
use anyhow::{ensure, Result};
use ethers::abi::{Detokenize, InvalidOutputType, Token, Tokenizable};
use ethers::prelude::{Address, U256};
use std::collections::HashMap;
use std::iter::zip;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum TradeParams {
    ExactInput(SwapExact),
    ExactOutput(SwapForExact),
}

#[derive(Debug)]
pub struct FoundTrades {
    pub protocol: Address,
    pub trades: Vec<Trade>,
}

impl FoundTrades {
    pub fn simulate_trades<'a>(
        &'a self,
        protocols: &'a mut HashMap<Address, Protocol>,
        input_amount: U256,
        custom_pairs: &Vec<Pair>,
    ) -> Vec<PossibleArbitrage> {
        let mut possible_arbitrages = Vec::new();
        for trade in &self.trades {
            let checked_amounts = match trade.check_trade_validity(protocols) {
                Ok(amounts) => amounts,
                Err(_) => continue,
            };

            let mut_protocol = protocols
                .get_mut(&self.protocol)
                .expect("Protocol not found in protocols");
            let changed = trade.simulate(mut_protocol, checked_amounts);

            let (path, output) = find_best_trade(protocols, input_amount, custom_pairs);
            possible_arbitrages.push(PossibleArbitrage::new(path, trade.gas, output));
            let protocol = protocols
                .get_mut(&self.protocol)
                .expect("Protocol not found in protocols");
            protocol.unsimualte_trade(changed);
        }
        possible_arbitrages
    }
}

#[derive(Debug, Clone)]
pub struct PossibleArbitrage {
    pub path: Path,
    pub gas: U256,
    pub output: U256,
}

impl PossibleArbitrage {
    pub fn new(path: Path, gas: U256, output: U256) -> Self {
        Self { path, gas, output }
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

impl FoundTrades {
    pub fn new(protocol: Address, trades: Vec<Trade>) -> Self {
        Self { protocol, trades }
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
    pub to: Address,
    pub from: Address,
    pub params: TradeParams,
    pub gas: U256,
    pub path: Path,
}

impl Trade {
    pub fn new(
        to: Address,
        from: Address,
        params: TradeParams,
        gas: U256,
        protocol: Address,
    ) -> Result<Self> {
        let path = Path::from_trade_tokens(params.get_path(), protocol)?;
        Ok(Self {
            to,
            from,
            params,
            gas,
            path,
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
            self.params.get_deadline().as_u64() >= cur_unix,
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
