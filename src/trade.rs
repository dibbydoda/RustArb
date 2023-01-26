use std::collections::HashMap;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::{GAS_ESTIMATE, PairStorage};
use anyhow::Result;
use ethers::abi::{InvalidOutputType, Token, Tokenizable};
use ethers::prelude::{Address, U256};
use petgraph::stable_graph::NodeIndex;

use crate::graph::{create_graph, find_shortest_path, PairLookup, Path};
use crate::pair::Pair;
use crate::v2protocol::{get_all_pairs, Protocol};

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
        let gas_price = match gas {
            Gas::Legacy(price) => price,
            Gas::London(max_fee_per_gas, _) => max_fee_per_gas,
        };

        let gas_in_eth = gas_price.saturating_mul(U256::from(GAS_ESTIMATE));
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

pub fn find_best_trade<'a>(
    pair_storage: Arc<PairStorage>,
    amount: U256,
    target: Address,
) -> (Path, U256) {
    let mut nodes: HashMap<Address, NodeIndex> = HashMap::new();
    let all_pairs = get_all_pairs(pair_storage.protocols.values());

    let pairs = all_pairs.chain(&pair_storage.custom_pairs);

    let graph = create_graph(pairs, &mut nodes).unwrap();
    let shortest = find_shortest_path(&graph, nodes, &target, amount).unwrap();
    let outputs = shortest.get_amounts_out(amount, &pair_storage.protocols).unwrap();

    (shortest, outputs.last().unwrap().to_owned())
}
