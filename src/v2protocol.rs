use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::iter::zip;
use std::sync::Arc;

use anyhow::{anyhow, ensure, Result};
use deadpool_sqlite::rusqlite::{params, params_from_iter};
use deadpool_sqlite::Pool;
use ethers::abi::{Abi, Token};
use ethers::prelude::*;

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
                token.into_address().ok_or_else(|| anyhow!("Token cannot convert into address"))?,
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
    factory: ethers::contract::Contract<WSClient>,
    pub router: ethers::contract::Contract<WSClient>,
    swap_fee: u32,
    name: String,
    pub pairs: HashMap<Address, Pair>,
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

    async fn update_excluded_pairs_for_protocol(&self, bad_tokens_file: &str) -> Result<()> {
        let name = self.name.clone();
        let bad_tokens: Vec<String> =
            serde_json::from_str(tokio::fs::read_to_string(bad_tokens_file).await?.as_str())?;
        let mut bad_tokens: Vec<String> = bad_tokens
            .iter()
            .map(|token| token.to_lowercase())
            .collect();
        let qmarks = repeat_vars(bad_tokens.len());
        bad_tokens.extend(bad_tokens.clone());
        bad_tokens.insert(0, self.name.clone());
        let bad_tokens = params_from_iter(bad_tokens);
        let sql = format!("UPDATE pairs SET excluded = TRUE WHERE protocol = ? AND (token0 IN ({}) OR token1 in ({}))",
                          qmarks, qmarks);

        let conn = self.pool.get().await?;

        let new: usize = conn
            .interact(move |conn| {
                conn.execute(
                    "UPDATE pairs SET excluded = FALSE WHERE protocol =?",
                    [name],
                )?;
                conn.execute(sql.as_str(), bad_tokens)
            })
            .await
            .map_err(|oops| anyhow!(oops.to_string()))??;

        println!("Updated excluded pairs for {}. Now: {}", self.name, new);

        Ok(())
    }

    async fn count_new_pairs(&self) -> Result<(u32, u32)> {
        let conn = self.pool.get().await?;
        let name = self.name.clone();
        let current: u32 = self.factory.method("allPairsLength", ())?.call().await?;
        let old: u32 = conn
            .interact(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM pairs WHERE protocol = ?1",
                    [name],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(|oops| anyhow!(oops.to_string()))??;

        Ok((old, current))
    }

    async fn get_new_pairs(&self, client: WSClient) -> Result<Option<Vec<DbAddition>>> {
        let mut multicall: Multicall<WSClient> =
            Multicall::new(self.factory.client().clone(), None)
                .await?
                .version(MulticallVersion::Multicall);
        let num_pairs = self.count_new_pairs().await?;
        let pair_call = match GetPairCall::new(self, num_pairs) {
            Some(paircall) => paircall,
            None => {
                return Ok(None);
            }
        };
        let addresses = pair_call.get_pair_addresses(&mut multicall).await?;
        let pool_contracts: Vec<SwapPool<WSClient>> = addresses
            .into_iter()
            .map(|address| address.generate_pool_contract(client.clone()))
            .collect();

        for pool in &pool_contracts {
            multicall.add_call(pool.method::<_, Address>("token0", ())?, false);
            multicall.add_call(pool.method::<_, Address>("token1", ())?, false);
        }

        let tokens = multicall.call_raw().await?;
        let tokens = tokens.chunks(2);
        let mut pairs: Vec<DbAddition> = Vec::with_capacity(pool_contracts.len());

        ensure!(
            tokens.len() == pool_contracts.len(),
            "Differing lengths of contracts and multicall returns"
        );
        for iter in zip(&pool_contracts, tokens) {
            let pool = iter.0;
            let chunk = iter.1;

            let new_pair = DbAddition::new(
                pool.address(),
                chunk[0]
                    .to_owned()
                    .into_address()
                    .ok_or_else(|| anyhow!("Token cannot convert into address"))?,
                chunk[1]
                    .to_owned()
                    .into_address()
                    .ok_or_else(|| anyhow!("Token cannot convert into address"))?,
            );

            pairs.push(new_pair);
        }
        Ok(Some(pairs))
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

    async fn insert_into_database(&self, additions: Vec<DbAddition>) -> Result<()> {
        let conn = self.pool.get().await?;
        let name = self.name.to_owned();
        conn.interact(move |conn| -> Result<()> {
            let mut stmt = conn.prepare("INSERT INTO pairs (protocol, address, token0, token1, excluded) VALUES (?, ?, ?, ?, ?)")?;
            for addition in additions {
                stmt.execute(params![name, format!("{:#x}", addition.address), format!("{:#x}", addition.token0), format!("{:#x}", addition.token1), false])?;
            }
            Ok(())
        }).await.map_err(|oops| anyhow!(oops.to_string()))?
    }

    async fn load_db_pairs(&mut self, client: WSClient) -> Result<()> {
        let partials = self.get_pair_addresses_from_db().await?;
        for partial in partials {
            let address = partial.address;
            let contract = address.generate_pool_contract(client.clone());
            self.pairs.insert(
                address.0,
                Pair::new(contract, partial.token0, partial.token1, self.swap_fee),
            );
        }
        Ok(())
    }
}

// Called by public functions
impl Protocol {
    async fn update_pairs(
        mut protocol: Self,
        client: WSClient,
        bad_tokens_file: &str,
    ) -> Result<Self> {
        {
            let additions = protocol.get_new_pairs(client.clone()).await?;
            if let Some(pairs) = additions {
                protocol.insert_into_database(pairs).await?
            };

            protocol
                .update_excluded_pairs_for_protocol(bad_tokens_file)
                .await?;
        };
        protocol.load_db_pairs(client).await?;

        Ok(protocol)
    }

    async fn get_reserves(mut protocol: Self) -> Result<Self> {
        let pairs: Vec<&mut Pair> = protocol.pairs.values_mut().collect();

        let mut multicall: Multicall<WSClient> =
            Multicall::new(protocol.factory.client().clone(), None)
                .await?
                .version(MulticallVersion::Multicall);

        for pair in &pairs {
            multicall.add_call(
                pair.contract
                    .method::<_, (u128, u128, u32)>("getReserves", ())?,
                false,
            );
        }

        let tokens = multicall.call_raw().await?;
        multicall.clear_calls();

        ensure!(
            pairs.len() == tokens.len(),
            "Differing lengths of pairs and multicall returns"
        );
        for it in zip(pairs, tokens) {
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

        Ok(protocol)
    }
}

pub async fn generate_protocols(
    client: WSClient,
    file_path: &str,
    pool: Arc<Pool>,
) -> Result<Vec<Protocol>> {
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
    let mut outcome = Vec::with_capacity(tasks.len());
    for task in tasks {
        outcome.push(task.await??)
    }

    Ok(outcome)
}

pub async fn update_all_pairs(protocols: Vec<Protocol>, client: WSClient) -> Result<Vec<Protocol>> {
    let mut handles = Vec::with_capacity(protocols.len());
    for protocol in protocols {
        handles.push(tokio::spawn(Protocol::update_pairs(
            protocol,
            client.clone(),
            BAD_TOKENS_PATH,
        )));
    }

    let mut updated = Vec::with_capacity(handles.len());
    let outcome = futures::future::join_all(handles).await;

    for item in outcome {
        updated.push(item??);
    }
    Ok(updated)
}

pub async fn get_all_reserves(protocols: Vec<Protocol>) -> Result<Vec<Protocol>> {
    let mut handles = Vec::with_capacity(protocols.len());
    for protocol in protocols {
        handles.push(tokio::spawn(Protocol::get_reserves(protocol)));
    }

    let mut updated = Vec::with_capacity(handles.len());
    let outcome = futures::future::join_all(handles).await;

    for item in outcome {
        updated.push(item??);
    }
    Ok(updated)
}

fn repeat_vars(count: usize) -> String {
    assert_ne!(count, 0);
    let mut s = "?,".repeat(count);
    // Remove trailing comma
    s.pop();
    s
}

pub fn get_all_pairs(protocols: Vec<&Protocol>) -> Vec<&Pair> {
    let mut allpairs = Vec::new();
    for protocol in protocols {
        allpairs.extend(protocol.pairs.values());
    }
    allpairs
}
