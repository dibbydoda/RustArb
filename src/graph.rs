use ordered_float::NotNan;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::iter::zip;

use crate::pair::Pair;
use crate::protocols::Protocol;
use anyhow::{anyhow, ensure, Result};
use ethers::prelude::Address;
use ethers::types::U256;
use petgraph::adj::DefaultIx;
use petgraph::prelude::{EdgeIndex, EdgeRef, NodeIndex, StableGraph};
use petgraph::Directed;

const MAX_NUM_SWAPS: usize = 4; // Num of tokens, therefore max pairs is 4

#[derive(Debug)]
pub struct Path<'a> {
    token_order: Vec<Address>,
    pair_order: Vec<&'a Pair>,
}

#[derive(Clone, Debug)]
pub struct SearchPath {
    token_order: Vec<NodeIndex>,
    pair_order: Vec<EdgeIndex>,
    weight: U256,
}

impl SearchPath {
    fn new(weight: U256) -> Self {
        let token_order = Vec::new();
        let pair_order = Vec::new();
        SearchPath {
            token_order,
            pair_order,
            weight,
        }
    }
}

impl<'a> Path<'a> {
    pub fn from_search_path(graph: &MyGraph<'a>, searched: SearchPath) -> Result<Self> {
        let token_order = searched
            .token_order
            .iter()
            .map(|node| {
                graph
                    .node_weight(*node)
                    .ok_or_else(|| anyhow!("Missing node"))
                    .map(|address| *address)
            })
            .collect::<Result<Vec<Address>>>()?;
        let pair_order = searched
            .pair_order
            .iter()
            .map(|edge| {
                graph
                    .edge_weight(*edge)
                    .ok_or_else(|| anyhow!("Missing edge"))
                    .map(|edge| *edge)
            })
            .collect::<Result<Vec<&Pair>>>()?;

        Ok(Path {
            token_order,
            pair_order,
        })
    }

    pub fn get_amounts_out(&self, input: U256) -> Result<Vec<U256>> {
        let mut amounts = Vec::with_capacity(self.pair_order.len());
        let mut current_amount = input;
        amounts.push(current_amount);

        for (input, pair) in zip(&self.token_order, &self.pair_order) {
            current_amount = pair.get_amount_out(*input, current_amount)?;
            amounts.push(current_amount);
        }

        Ok(amounts)
    }
}

type MyGraph<'a> = StableGraph<Address, &'a Pair, Directed, DefaultIx>;

fn add_pair<'a>(
    graph: &mut MyGraph<'a>,
    pair: &'a Pair,
    nodes: &mut HashMap<Address, NodeIndex>,
    start_token: Address,
) -> Result<()> {
    let (token0, token1) = pair.get_tokens();
    let node0 = match nodes.get(&token0) {
        Some(node) => *node,
        None => {
            let index = graph.add_node(token0);
            nodes.insert(token0, index);
            index
        }
    };

    let node1 = match nodes.get(&token1) {
        Some(node) => *node,
        None => {
            let index = graph.add_node(token1);
            nodes.insert(token1, index);
            index
        }
    };

    if token0 == start_token {
        let start_node = *nodes
            .get(&Address::zero())
            .ok_or_else(|| anyhow!("Missing Target Node"))?;

        graph.add_edge(start_node, node1, pair);
    } else if token1 == start_token {
        let start_node = *nodes
            .get(&Address::zero())
            .ok_or_else(|| anyhow!("Missing Target Node"))?;

        graph.add_edge(start_node, node0, pair);
    }

    graph.add_edge(node0, node1, pair);
    graph.add_edge(node1, node0, pair);

    Ok(())
}

pub fn create_graph<'a>(
    protocols: &'a Vec<Protocol>,
    nodes: &mut HashMap<Address, NodeIndex>,
    traded_token: Address,
) -> Result<MyGraph<'a>> {
    let mut allpairs = Vec::new();
    for protocol in protocols {
        allpairs.extend(protocol.pairs.values());
    }

    let mut graph: MyGraph = MyGraph::new();

    let start_index = graph.add_node(traded_token);
    nodes.insert(Address::zero(), start_index);

    for pair in allpairs {
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

pub fn find_shortest_path<'a>(
    graph: &MyGraph<'a>,
    nodes: HashMap<Address, NodeIndex>,
    target: &Address,
    amount_in: U256,
) -> Result<Path<'a>> {
    let goal = *nodes
        .get(target)
        .ok_or_else(|| anyhow!("Missing target node"))?;
    let start_index = nodes
        .get(&Address::zero())
        .ok_or_else(|| anyhow!("Missing start node"))?;

    let mut seen: HashMap<(NodeIndex, usize), U256> = HashMap::new();
    let mut best_path = SearchPath::new(0.into());
    let mut start_path = SearchPath::new(amount_in);
    start_path.token_order.push(*start_index);
    search_visit(graph, goal, start_path, &mut seen, &mut best_path);

    Path::from_search_path(graph, best_path)
}

fn search_visit(
    graph: &MyGraph,
    target_node: NodeIndex,
    cur_path: SearchPath,
    seen_nodes: &mut HashMap<(NodeIndex, usize), U256>,
    best: &mut SearchPath,
) {
    if cur_path.pair_order.len() > MAX_NUM_SWAPS {
        return;
    }
    let cur_node = cur_path.token_order[cur_path.token_order.len() - 1];
    let cur_weight = cur_path.weight;

    if cur_node == target_node {
        if cur_weight > best.weight {
            best.token_order = cur_path.token_order;
            best.pair_order = cur_path.pair_order;
            best.weight = cur_weight;
        }
        return;
    }

    match seen_nodes.entry((cur_node, cur_path.pair_order.len())) {
        Entry::Occupied(mut occupied) => {
            if *occupied.get() > cur_weight {
                return;
            } else {
                occupied.insert(cur_weight);
            }
        }
        Entry::Vacant(vacant) => {
            vacant.insert(cur_weight);
        }
    }

    for (edge, node, weight) in get_successors(graph, cur_node, cur_weight) {
        if cur_path.pair_order.contains(&edge) {
            continue;
        } else {
            let mut next_path = cur_path.clone();
            next_path.pair_order.push(edge);
            next_path.token_order.push(node);
            next_path.weight = weight;
            search_visit(graph, target_node, next_path, seen_nodes, best);
        }
    }
}

fn get_successors(
    graph: &MyGraph,
    node: NodeIndex,
    cur_weight: U256,
) -> Vec<(EdgeIndex, NodeIndex, U256)> {
    let node_token = graph.node_weight(node).unwrap();
    let edges = graph.edges(node);

    let mut successors = Vec::new();

    for edge in edges {
        let target = edge.target();
        let weight = edge.weight().calculate_weight(*node_token, cur_weight);

        successors.push((edge.id(), target, weight));
    }

    successors
}
