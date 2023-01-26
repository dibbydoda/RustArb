use std::collections::hash_map::Values;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::iter::{zip, FlatMap};
use std::sync::Arc;

use anyhow::{anyhow, ensure, Result};
use deadpool_sqlite::rusqlite::params;
use deadpool_sqlite::Pool;
use ethers::abi::{Abi, Token};
use ethers::prelude::*;
use futures::future::try_join_all;

use crate::pair::{Pair, PairAddress, PartialPair};

pub type WSClient = Arc<Provider<Ws>>;

abigen!(SwapPool, "abis/pool.json");
const BAD_TOKENS_PATH: &str = "bad_tokens.json";

struct GetPairCall<'a> {
    protocol: &'a Protocol,
    range: std::ops::Range<u32>,
}

impl<'a> GetPairCall<'a> {
    fn new(protocol: &'a Protocol, bounds: (u32, u32)) -> Option<Self> {
        let range = bounds.0..bounds.1;
        if range.is_empty() {
            None
        } else {
            Some(GetPairCall { protocol, range })
        }
    }

    async fn get_pair_addresses(
        &self,
        multicall: &mut Multicall<WSClient>,
    ) -> Result<Vec<PairAddress>> {
        for pair_no in self.range.clone() {
            multicall.add_call(
                self.protocol
                    .factory
                    .method::<u32, Address>("allPairs", pair_no)?,
                false,
            );
        }
        let address_tokens: Vec<Token> = multicall.call_raw().await?;
        multicall.clear_calls();
        let mut addresses = Vec::with_capacity(address_tokens.len());
        for token in address_tokens {
            addresses.push(PairAddress(
                token
                    .into_address()
                    .ok_or_else(|| anyhow!("Token cannot convert into address"))?,
            ))
        }

        Ok(addresses)
    }
}

#[derive(Debug)]
struct DbAddition {
    address: Address,
    token0: Address,
    token1: Address,
}

impl DbAddition {
    const fn new(address: Address, token0: Address, token1: Address) -> Self {
        Self {
            address,
            token0,
            token1,
        }
    }
}

#[derive(serde::Deserialize)]
struct RawProtocol {
    factory_addr: Address,
    factory_abi: String,
    swap_fee: u32,
    name: String,
    router_address: Address,
    router_abi: String,
}

#[derive(Debug)]
pub struct Protocol {
    pub factory: ethers::contract::Contract<WSClient>,
    pub router: ethers::contract::Contract<WSClient>,
    swap_fee: u32,
    name: String,
    pub pairs: HashMap<(Address, Address), Pair>,
    pool: Arc<Pool>,
}

impl Hash for Protocol {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.factory.address().hash(state);
    }
}

// Private Functions
impl Protocol {
    async fn new(raw: RawProtocol, client: WSClient, pool: Arc<Pool>) -> Result<Self> {
        let factory_abi: Abi =
            serde_json::from_str(tokio::fs::read_to_string(raw.factory_abi).await?.as_str())?;
        let router_abi: Abi =
            serde_json::from_str(tokio::fs::read_to_string(raw.router_abi).await?.as_str())?;
        let factory =
            ethers::contract::Contract::new(raw.factory_addr, factory_abi, client.clone());
        let router =
            ethers::contract::Contract::new(raw.router_address, router_abi, client.clone());
        Ok(Self {
            factory,
            router,
            swap_fee: raw.swap_fee,
            name: raw.name,
            pairs: HashMap::new(),
            pool,
        })
    }

    async fn get_pair_addresses_from_db(&mut self) -> Result<Vec<PartialPair>> {
        let conn = self.pool.get().await?;
        let name = self.name.clone();
        conn.interact(move |conn| -> Result<Vec<PartialPair>> {
            let mut stmt = conn.prepare(
                "SELECT address, token0, token1 FROM pairs WHERE protocol = ?1 AND excluded = ?2",
            )?;
            let mut rows = stmt.query(params![name, false])?;

            let mut partials = Vec::new();

            while let Ok(Some(row)) = rows.next() {
                partials.push(PartialPair::new(row.get(0)?, row.get(1)?, row.get(2)?)?);
            }
            Ok(partials)
        })
        .await
        .map_err(|oops| anyhow!(oops.to_string()))?
    }

    async fn load_db_pairs(&mut self, client: WSClient) -> Result<()> {
        let partials = self.get_pair_addresses_from_db().await?;
        for partial in partials {
            let address = partial.address;
            let contract = address.generate_pool_contract(client.clone());
            self.pairs.insert(
                (partial.token0, partial.token1),
                Pair::new(
                    contract,
                    partial.token0,
                    partial.token1,
                    self.swap_fee,
                    self.factory.address(),
                ),
            );
        }
        Ok(())
    }

    async fn update_pairs(mut protocol: Self, client: WSClient) -> Result<Self> {
        protocol.load_db_pairs(client).await?;

        Ok(protocol)
    }

    pub(crate) async fn get_reserves(
        mut protocol: Self,
        address: Address,
    ) -> Result<(Self, Address)> {
        let mut multicall: Multicall<WSClient> =
            Multicall::new(protocol.factory.client().clone(), None)
                .await?
                .version(MulticallVersion::Multicall);

        for pair in protocol.pairs.values_mut() {
            multicall.add_call(
                pair.contract
                    .method::<_, (u128, u128, u32)>("getReserves", ())?,
                false,
            );
        }

        let tokens = multicall.call_raw().await?;
        multicall.clear_calls();

        ensure!(
            protocol.pairs.len() == tokens.len(),
            "Differing lengths of pairs and multicall returns"
        );
        for it in zip(protocol.pairs.values_mut(), tokens) {
            let pair = it.0;
            let token = it.1;
            let mut reserves = token
                .into_tuple()
                .ok_or_else(|| anyhow!("Token cannot convert into tuple"))?;

            pair.reserve0 = reserves
                .swap_remove(0)
                .into_uint()
                .ok_or_else(|| anyhow!("Token cannot convert into uint"))?
                .as_u128();
            pair.reserve1 = reserves
                .swap_remove(1)
                .into_uint()
                .ok_or_else(|| anyhow!("Token cannot convert into uint"))?
                .as_u128();
        }

        Ok((protocol, address))
    }
}

pub async fn generate_protocols(
    client: WSClient,
    file_path: &str,
    pool: Arc<Pool>,
) -> Result<HashMap<Address, Protocol>> {
    let raw_protocols: Vec<RawProtocol> =
        serde_json::from_str(tokio::fs::read_to_string(file_path).await?.as_str())?;
    let mut tasks = Vec::with_capacity(raw_protocols.len());
    for raw in raw_protocols {
        tasks.push(tokio::spawn(Protocol::new(
            raw,
            client.clone(),
            pool.clone(),
        )));
    }

    let mut protocols: HashMap<Address, Protocol> = HashMap::with_capacity(tasks.len());
    let outcomes = try_join_all(tasks)
        .await?
        .into_iter()
        .collect::<Result<Vec<Protocol>>>()?;
    for protocol in outcomes {
        protocols.insert(protocol.factory.address(), protocol);
    }

    Ok(protocols)
}

pub async fn update_all_pairs(
    mut protocols: HashMap<Address, Protocol>,
    client: WSClient,
) -> Result<HashMap<Address, Protocol>> {
    let mut handles = Vec::with_capacity(protocols.len());
    for (_address, protocol) in protocols.drain() {
        handles.push(tokio::spawn(Protocol::update_pairs(
            protocol,
            client.clone(),
        )));
    }

    let outcome = futures::future::try_join_all(handles).await?;

    for item in outcome {
        let protocol = item?;
        protocols.insert(protocol.factory.address(), protocol);
    }
    Ok(protocols)
}

fn repeat_vars(count: usize) -> String {
    assert_ne!(count, 0);
    let mut s = "?,".repeat(count);
    // Remove trailing comma
    s.pop();
    s
}

pub fn get_all_pairs<'a>(
    protocols: Values<'a, H160, Protocol>,
) -> FlatMap<
    Values<'a, Address, Protocol>,
    Values<'_, (Address, Address), Pair>,
    fn(&'a Protocol) -> Values<'_, (Address, Address), Pair>,
> {
    protocols.flat_map(|item| item.pairs.values())
}
