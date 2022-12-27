use std::collections::HashMap;
use std::convert::Into;
use std::ops::{Add, Deref};

use anyhow::{anyhow, ensure, Result};
use ethers::abi::Address;
use ethers::types::U256;
use pathfinding::num_traits::Zero;
use pathfinding::prelude::dijkstra;
use petgraph::adj::DefaultIx;
use petgraph::prelude::{EdgeRef, NodeIndex, StableGraph};
use petgraph::Directed;

#[derive(Eq, PartialEq, PartialOrd, Ord, Copy, Clone)]
struct WrappedU256(U256);

impl Deref for WrappedU256 {
    type Target = U256;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Add for WrappedU256 {
    type Output = WrappedU256;

    fn add(self, rhs: Self) -> Self::Output {
        WrappedU256(self.saturating_add(*rhs))
    }
}

impl Zero for WrappedU256 {
    fn zero() -> Self {
        WrappedU256(U256::zero())
    }

    fn is_zero(&self) -> bool {
        **self == U256::zero()
    }
}

use crate::pair::Pair;

type MyGraph<'a> = StableGraph<Address, &'a Pair, Directed, DefaultIx>;

const OUTPUT: u32 = 1000000000;

fn add_pair<'a>(
    graph: &mut MyGraph<'a>,
    pair: &'a Pair,
    nodes: &mut HashMap<Address, NodeIndex>,
    traded_token: Address,
) -> Result<()> {
    let (token0, token1) = pair.get_tokens();
    let node0 = match nodes.get(&token0) {
        Some(node) => *node,
        None => {
            let index = graph.add_node(token0);
            nodes.insert(token0, index);
            *nodes.get(&token0).ok_or_else(|| anyhow!("Missing node"))?
        }
    };

    let node1 = match nodes.get(&token1) {
        Some(node) => *node,
        None => {
            let index = graph.add_node(token1);
            nodes.insert(token1, index);
            *nodes.get(&token0).ok_or_else(|| anyhow!("Missing node"))?
        }
    };

    if token0 == traded_token {
        let traded = *nodes
            .get(&Address::zero())
            .ok_or_else(|| anyhow!("Missing Target Node"))?;

        graph.add_edge(traded, node1, pair);
    } else if token1 == traded_token {
        let traded = *nodes
            .get(&Address::zero())
            .ok_or_else(|| anyhow!("Missing Target Node"))?;
        graph.add_edge(traded, node0, pair);
    }

    graph.add_edge(node0, node1, pair);
    graph.add_edge(node1, node0, pair);

    Ok(())
}

pub fn create_graph<'a>(
    pairs: Vec<&'a Pair>,
    nodes: &mut HashMap<Address, NodeIndex>,
    traded_token: Address,
) -> Result<MyGraph<'a>> {
    let mut graph: MyGraph = MyGraph::new();

    let target_index = graph.add_node(traded_token);
    nodes.insert(Address::zero(), target_index);

    for pair in pairs {
        add_pair(&mut graph, pair, nodes, traded_token)?;
    }
    Ok(graph)
}

pub fn remove_token(
    token: &Address,
    graph: &mut MyGraph,
    nodes: &mut HashMap<Address, NodeIndex>,
) -> Result<()> {
    let node_index = *nodes
        .get(token)
        .ok_or_else(|| anyhow!("Missing Node for removal"))?;
    let node = graph
        .remove_node(node_index)
        .ok_or_else(|| anyhow!("Missing Node for removal"))?;
    ensure!(&node == token, "Mismatched Index and Node when Removing");
    Ok(())
}

pub fn find_shortest(
    graph: &MyGraph,
    nodes: HashMap<Address, NodeIndex>,
    target: &Address,
) -> Result<(Vec<NodeIndex>, U256)> {
    let goal = nodes
        .get(target)
        .ok_or_else(|| anyhow!("Missing target node"))?;
    let start = nodes
        .get(&Address::zero())
        .ok_or_else(|| anyhow!("Missing start node"))?;
    dijkstra(start, |p| get_successors(graph, p), |p| p == goal)
        .ok_or_else(|| anyhow!("No path!!"))
        .map(|tuple| (tuple.0, *tuple.1))
}

fn get_successors(graph: &MyGraph, node: &NodeIndex) -> Vec<(NodeIndex, WrappedU256)> {
    let node_token = graph.node_weight(*node).unwrap();
    let edges = graph.edges(*node);

    let mut successors = Vec::new();

    for edge in edges {
        let target = edge.target();
        let amount_out = edge
            .weight()
            .get_amount_in(*node_token, OUTPUT.into())
            .unwrap();
        successors.push((target, WrappedU256(amount_out)));
    }

    successors
}
