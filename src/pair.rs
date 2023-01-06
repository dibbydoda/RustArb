use crate::v2protocol::{SwapPool, WSClient};
use anyhow::Result;
use ethers::prelude::Address;
use ethers::types::U256;
use std::fmt::Debug;
use std::panic::panic_any;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug)]
pub struct Pair {
    pub contract: SwapPool<WSClient>,
    token0: Address,
    token1: Address,
    pub reserve0: u128,
    pub reserve1: u128,
    fee: u32,
}

#[derive(serde::Deserialize)]
pub struct JsonPair {
    address: PairAddress,
    token0: Address,
    token1: Address,
    reserve0: u128,
    reserve1: u128,
    fee: u32,
}

#[derive(Error, Debug)]
pub enum ArbitrageError {
    #[error("No Liquidity")]
    NoLiquidity,
    #[error("Math Overflow")]
    MathOverflow,
    #[error("Math Underflow")]
    MathUnderflow,
    #[error("Divide by zero")]
    DivideByZero,
    #[error("Token not in pair")]
    TokenNotInPair,
}

impl Pair {
    pub const fn new(
        contract: SwapPool<WSClient>,
        token0: Address,
        token1: Address,
        fee: u32,
    ) -> Self {
        Self {
            contract,
            token0,
            token1,
            reserve0: 0,
            reserve1: 0,
            fee,
        }
    }

    pub fn from_jsonpair(json: JsonPair, client: WSClient) -> Self {
        let contract = json.address.generate_pool_contract(client);
        Self {
            contract,
            token0: json.token0,
            token1: json.token1,
            reserve0: json.reserve0,
            reserve1: json.reserve1,
            fee: json.fee,
        }
    }

    pub fn contains(self, token: &Address) -> bool {
        *token == self.token0 || *token == self.token1
    }

    pub const fn get_tokens(&self) -> (Address, Address) {
        (self.token0, self.token1)
    }

    pub fn get_amount_out(&self, input: Address, amount_in: U256) -> Result<U256, ArbitrageError> {
        let reserves = self.get_ordered_reserves(input)?;
        if reserves.input == 0.into() || reserves.output == 0.into() {
            return Err(ArbitrageError::NoLiquidity);
        }

        let fee_base: u32 = 10000;
        let fee_ratio = fee_base
            .checked_sub(self.fee)
            .ok_or(ArbitrageError::MathUnderflow)?;
        let amount_in_with_fee = amount_in
            .checked_mul(fee_ratio.into())
            .ok_or(ArbitrageError::MathOverflow)?;
        let numerator = amount_in_with_fee
            .checked_mul(reserves.output)
            .ok_or(ArbitrageError::MathOverflow)?;
        let denom_multi = reserves
            .input
            .checked_mul(fee_base.into())
            .ok_or(ArbitrageError::MathOverflow)?;
        let denominator = amount_in_with_fee
            .checked_add(denom_multi)
            .ok_or(ArbitrageError::MathUnderflow)?;
        let output = numerator
            .checked_div(denominator)
            .ok_or(ArbitrageError::DivideByZero)?;

        Ok(output)
    }

    pub fn get_amount_in(&self, input: Address, amount_out: U256) -> Result<U256, ArbitrageError> {
        let reserves = self.get_ordered_reserves(input)?;
        if reserves.input == 0.into() || reserves.output == 0.into() {
            return Err(ArbitrageError::NoLiquidity);
        }
        let fee_base: u32 = 10000;
        let fee_ratio = fee_base
            .checked_sub(self.fee)
            .ok_or(ArbitrageError::MathUnderflow)?;
        let numerator = reserves
            .input
            .checked_mul(amount_out)
            .ok_or(ArbitrageError::MathOverflow)?
            .checked_mul(fee_base.into())
            .ok_or(ArbitrageError::MathOverflow)?;
        let denom_sub = reserves.output.saturating_sub(amount_out);
        let denominator = denom_sub
            .checked_mul(fee_ratio.into())
            .ok_or(ArbitrageError::MathOverflow)?;
        let division = numerator
            .checked_div(denominator)
            .unwrap_or_else(U256::max_value);
        Ok(division.saturating_add(1.into()))
    }

    fn get_ordered_reserves(&self, input: Address) -> Result<OrderedReserves, ArbitrageError> {
        if input == self.token0 {
            Ok(OrderedReserves::new(self.reserve0, self.reserve1))
        } else if input == self.token1 {
            Ok(OrderedReserves::new(self.reserve1, self.reserve0))
        } else {
            Err(ArbitrageError::TokenNotInPair)
        }
    }

    pub fn calculate_weight(&self, input: Address, amount_in: U256) -> U256 {
        match self.get_amount_out(input, amount_in) {
            Ok(weight) => weight,
            Err(error) => match error {
                ArbitrageError::NoLiquidity => U256::zero(),
                _ => panic_any(error),
            },
        }
    }
}

#[derive(serde::Deserialize, Copy, Clone)]
pub struct PairAddress(pub Address);

impl PairAddress {
    pub fn generate_pool_contract(self, client: WSClient) -> SwapPool<WSClient> {
        SwapPool::new(self.0, client.into())
    }
}

pub struct PartialPair {
    pub address: PairAddress,
    pub token0: Address,
    pub token1: Address,
}

impl PartialPair {
    pub fn new(address: String, token0: String, token1: String) -> Result<Self> {
        let address = PairAddress(Address::from_str(address.as_str())?);
        let token0 = Address::from_str(token0.as_str())?;
        let token1 = Address::from_str(token1.as_str())?;
        Ok(Self {
            address,
            token0,
            token1,
        })
    }
}

struct OrderedReserves {
    input: U256,
    output: U256,
}

impl OrderedReserves {
    fn new(input: u128, output: u128) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
        }
    }
}

pub async fn generate_custom_pairs(pair_file: &str, client: WSClient) -> Result<Vec<Pair>> {
    let custom_pairs: Vec<JsonPair> =
        serde_json::from_str(tokio::fs::read_to_string(pair_file).await?.as_str())?;
    Ok(custom_pairs
        .into_iter()
        .map(|json| Pair::from_jsonpair(json, client.clone()))
        .collect())
}
