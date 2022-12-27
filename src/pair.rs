use crate::protocols::{SwapPool, WSClient};
use anyhow::{anyhow, bail, Result};
use ethers::prelude::Address;
use ethers::types::U256;
use std::str::FromStr;

#[derive(Debug)]
pub struct Pair {
    pub contract: SwapPool<WSClient>,
    token0: Address,
    token1: Address,
    pub reserve0: u128,
    pub reserve1: u128,
    fee: u32,
}

impl Pair {
    pub fn new(contract: SwapPool<WSClient>, token0: Address, token1: Address, fee: u32) -> Self {
        Pair {
            contract,
            token0,
            token1,
            reserve0: 0,
            reserve1: 0,
            fee,
        }
    }

    pub fn contains(self, token: &Address) -> bool {
        *token == self.token0 || *token == self.token1
    }

    pub fn get_tokens(&self) -> (Address, Address) {
        (self.token0, self.token1)
    }

    pub fn get_amount_out(&self, input: Address, amount_in: U256) -> Result<U256> {
        let reserves = self.get_ordered_reserves(input)?;
        let fee_base: u32 = 10000;
        let fee_ratio = fee_base
            .checked_sub(self.fee)
            .ok_or_else(|| anyhow!("Math Underflow"))?;
        let amount_in_with_fee = amount_in
            .checked_mul(fee_ratio.into())
            .ok_or_else(|| anyhow!("Math Overflow"))?;
        let numerator = amount_in_with_fee
            .checked_mul(reserves.output)
            .ok_or_else(|| anyhow!("Math Overflow"))?;
        let denom_multi = reserves
            .input
            .checked_mul(fee_base.into())
            .ok_or_else(|| anyhow!("Math Overflow"))?;
        let denominator = amount_in_with_fee
            .checked_add(denom_multi)
            .ok_or_else(|| anyhow!("Math Overflow"))?;
        numerator
            .checked_div(denominator)
            .ok_or_else(|| anyhow!("Divide by zero"))
    }

    pub fn get_amount_in(&self, input: Address, amount_out: U256) -> Result<U256> {
        let reserves = self.get_ordered_reserves(input)?;
        let fee_base: u32 = 10000;
        let fee_ratio = fee_base
            .checked_sub(self.fee)
            .ok_or_else(|| anyhow!("Math Underflow1"))?;
        let numerator = reserves
            .input
            .checked_mul(amount_out)
            .ok_or_else(|| anyhow!("Math Overflow"))?
            .checked_mul(fee_base.into())
            .ok_or_else(|| anyhow!("Math Overflow"))?;
        let denom_sub = reserves.output.saturating_sub(amount_out);
        let denominator = denom_sub
            .checked_mul(fee_ratio.into())
            .ok_or_else(|| anyhow!("Math Overflow"))?;
        let division = numerator
            .checked_div(denominator)
            .unwrap_or_else(|| U256::max_value());
        Ok(division.saturating_add(1.into()))
    }

    fn get_ordered_reserves(&self, input: Address) -> Result<OrderedReserves> {
        if input == self.token0 {
            Ok(OrderedReserves::new(self.reserve0, self.reserve1))
        } else if input == self.token1 {
            Ok(OrderedReserves::new(self.reserve1, self.reserve0))
        } else {
            bail!("Input token not in pair");
        }
    }
}

pub struct PairAddress(pub Address);

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
        Ok(PartialPair {
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
        OrderedReserves {
            input: input.into(),
            output: output.into(),
        }
    }
}
