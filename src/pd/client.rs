// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::prelude::*;
use futures::stream::BoxStream;
use log::info;
use tokio::sync::RwLock;

use crate::compat::stream_fn;
use crate::kv::codec;
use crate::pd::retry::RetryClientTrait;
use crate::pd::Cluster;
use crate::pd::RetryClient;
use crate::proto::keyspacepb;
use crate::proto::kvrpcpb;
use crate::proto::metapb;
use crate::region::RegionId;
use crate::region::RegionVerId;
use crate::region::RegionWithLeader;
use crate::region::StoreId;
use crate::region_cache::RegionCache;
use crate::store::KvConnect;
use crate::store::RegionStore;
use crate::store::TikvConnect;
use crate::store::{KvClient, Store};
use crate::BoundRange;
use crate::Config;
use crate::Key;
use crate::Result;
use crate::SecurityManager;
use crate::Timestamp;

/// The PdClient handles all the encoding stuff.
///
/// Raw APIs does not require encoding/decoding at all.
/// All keys in all places (client, PD, TiKV) are in the same encoding (here called "raw format").
///
/// Transactional APIs are a bit complicated.
/// We need encode and decode keys when we communicate with PD, but not with TiKV.
/// We encode keys before sending requests to PD, and decode keys in the response from PD.
/// That's all we need to do with encoding.
///
///  client -encoded-> PD, PD -encoded-> client
///  client -raw-> TiKV, TiKV -raw-> client
///
/// The reason for the behavior is that in transaction mode, TiKV encode keys for MVCC.
/// In raw mode, TiKV doesn't encode them.
/// TiKV tells PD using its internal representation, whatever the encoding is.
/// So if we use transactional APIs, keys in PD are encoded and PD does not know about the encoding stuff.
#[async_trait]
pub trait PdClient: Send + Sync + 'static {
    type KvClient: KvClient + Send + Sync + 'static;

    /// In transactional API, `region` is decoded (keys in raw format).
    async fn map_region_to_store(self: Arc<Self>, region: RegionWithLeader) -> Result<RegionStore>;

    /// In transactional API, the key and returned region are both decoded (keys in raw format).
    async fn region_for_key(&self, key: &Key) -> Result<RegionWithLeader>;

    /// In transactional API, the returned region is decoded (keys in raw format)
    async fn region_for_id(&self, id: RegionId) -> Result<RegionWithLeader>;

    async fn get_timestamp(self: Arc<Self>) -> Result<Timestamp>;

    async fn update_safepoint(self: Arc<Self>, safepoint: u64) -> Result<bool>;

    async fn load_keyspace(&self, keyspace: &str) -> Result<keyspacepb::KeyspaceMeta>;

    /// In transactional API, `key` is in raw format
    async fn store_for_key(self: Arc<Self>, key: &Key) -> Result<RegionStore> {
        let region = self.region_for_key(key).await?;
        self.map_region_to_store(region).await
    }

    async fn store_for_id(self: Arc<Self>, id: RegionId) -> Result<RegionStore> {
        let region = self.region_for_id(id).await?;
        self.map_region_to_store(region).await
    }

    async fn all_stores(&self) -> Result<Vec<Store>>;

    fn group_keys_by_region<K, K2>(
        self: Arc<Self>,
        keys: impl Iterator<Item = K> + Send + Sync + 'static,
    ) -> BoxStream<'static, Result<(Vec<K2>, RegionWithLeader)>>
    where
        K: AsRef<Key> + Into<K2> + Send + Sync + 'static,
        K2: Send + Sync + 'static,
    {
        let keys = keys.peekable();
        stream_fn(keys, move |mut keys| {
            let this = self.clone();
            async move {
                if let Some(key) = keys.next() {
                    let region = this.region_for_key(key.as_ref()).await?;
                    let mut grouped = vec![key.into()];
                    while let Some(key) = keys.peek() {
                        if !region.contains(key.as_ref()) {
                            break;
                        }
                        grouped.push(keys.next().unwrap().into());
                    }
                    Ok(Some((keys, (grouped, region))))
                } else {
                    Ok(None)
                }
            }
        })
        .boxed()
    }

    /// Returns a Stream which iterates over the contexts for each region covered by range.
    fn regions_for_range(
        self: Arc<Self>,
        range: BoundRange,
    ) -> BoxStream<'static, Result<RegionWithLeader>> {
        let (start_key, end_key) = range.into_keys();
        stream_fn(Some(start_key), move |start_key| {
            let end_key = end_key.clone();
            let this = self.clone();
            async move {
                let start_key = match start_key {
                    None => return Ok(None),
                    Some(sk) => sk,
                };

                let region = this.region_for_key(&start_key).await?;
                let region_end = region.end_key();
                if end_key
                    .map(|x| x <= region_end && !x.is_empty())
                    .unwrap_or(false)
                    || region_end.is_empty()
                {
                    return Ok(Some((None, region)));
                }
                Ok(Some((Some(region_end), region)))
            }
        })
        .boxed()
    }

    /// Returns a Stream which iterates over the contexts for ranges in the same region.
    fn group_ranges_by_region(
        self: Arc<Self>,
        mut ranges: Vec<kvrpcpb::KeyRange>,
    ) -> BoxStream<'static, Result<(Vec<kvrpcpb::KeyRange>, RegionWithLeader)>> {
        ranges.reverse();
        stream_fn(Some(ranges), move |ranges| {
            let this = self.clone();
            async move {
                let mut ranges = match ranges {
                    None => return Ok(None),
                    Some(r) => r,
                };

                if let Some(range) = ranges.pop() {
                    let start_key: Key = range.start_key.clone().into();
                    let end_key: Key = range.end_key.clone().into();
                    let region = this.region_for_key(&start_key).await?;
                    let region_start = region.start_key();
                    let region_end = region.end_key();
                    let mut grouped = vec![];
                    if !region_end.is_empty() && (end_key > region_end || end_key.is_empty()) {
                        grouped.push(make_key_range(start_key.into(), region_end.clone().into()));
                        ranges.push(make_key_range(region_end.into(), end_key.into()));
                        return Ok(Some((Some(ranges), (grouped, region))));
                    }
                    grouped.push(range);

                    while let Some(range) = ranges.pop() {
                        let start_key: Key = range.start_key.clone().into();
                        let end_key: Key = range.end_key.clone().into();
                        if start_key < region_start || start_key > region_end {
                            ranges.push(range);
                            break;
                        }
                        if !region_end.is_empty() && (end_key > region_end || end_key.is_empty()) {
                            grouped
                                .push(make_key_range(start_key.into(), region_end.clone().into()));
                            ranges.push(make_key_range(region_end.into(), end_key.into()));
                            return Ok(Some((Some(ranges), (grouped, region))));
                        }
                        grouped.push(range);
                    }
                    Ok(Some((Some(ranges), (grouped, region))))
                } else {
                    Ok(None)
                }
            }
        })
        .boxed()
    }

    fn decode_region(mut region: RegionWithLeader, enable_codec: bool) -> Result<RegionWithLeader> {
        if enable_codec {
            codec::decode_bytes_in_place(&mut region.region.start_key, false)?;
            codec::decode_bytes_in_place(&mut region.region.end_key, false)?;
        }
        Ok(region)
    }

    async fn update_leader(&self, ver_id: RegionVerId, leader: metapb::Peer) -> Result<()>;

    async fn invalidate_region_cache(&self, ver_id: RegionVerId);

    async fn invalidate_store_cache(&self, store_id: StoreId);
}

/// This client converts requests for the logical TiKV cluster into requests
/// for a single TiKV store using PD and internal logic.
pub struct PdRpcClient<KvC: KvConnect + Send + Sync + 'static = TikvConnect, Cl = Cluster> {
    pd: Arc<RetryClient<Cl>>,
    kv_connect: KvC,
    kv_client_cache: Arc<RwLock<HashMap<String, KvC::KvClient>>>,
    enable_codec: bool,
    region_cache: RegionCache<RetryClient<Cl>>,
}

#[async_trait]
impl<KvC: KvConnect + Send + Sync + 'static> PdClient for PdRpcClient<KvC> {
    type KvClient = KvC::KvClient;

    async fn map_region_to_store(self: Arc<Self>, region: RegionWithLeader) -> Result<RegionStore> {
        let store_id = region.get_store_id()?;
        let store = self.region_cache.get_store_by_id(store_id).await?;
        let kv_client = self.kv_client(&store.address).await?;
        Ok(RegionStore::new(region, Arc::new(kv_client)))
    }

    async fn region_for_key(&self, key: &Key) -> Result<RegionWithLeader> {
        let enable_codec = self.enable_codec;
        let key = if enable_codec {
            key.to_encoded()
        } else {
            key.clone()
        };

        let region = self.region_cache.get_region_by_key(&key).await?;
        Self::decode_region(region, enable_codec)
    }

    async fn region_for_id(&self, id: RegionId) -> Result<RegionWithLeader> {
        let region = self.region_cache.get_region_by_id(id).await?;
        Self::decode_region(region, self.enable_codec)
    }

    async fn all_stores(&self) -> Result<Vec<Store>> {
        let pb_stores = self.region_cache.read_through_all_stores().await?;
        let mut stores = Vec::with_capacity(pb_stores.len());
        for store in pb_stores {
            let client = self.kv_client(&store.address).await?;
            stores.push(Store::new(Arc::new(client)));
        }
        Ok(stores)
    }

    async fn get_timestamp(self: Arc<Self>) -> Result<Timestamp> {
        self.pd.clone().get_timestamp().await
    }

    async fn update_safepoint(self: Arc<Self>, safepoint: u64) -> Result<bool> {
        self.pd.clone().update_safepoint(safepoint).await
    }

    async fn update_leader(&self, ver_id: RegionVerId, leader: metapb::Peer) -> Result<()> {
        self.region_cache.update_leader(ver_id, leader).await
    }

    async fn invalidate_region_cache(&self, ver_id: RegionVerId) {
        self.region_cache.invalidate_region_cache(ver_id).await
    }

    async fn invalidate_store_cache(&self, store_id: StoreId) {
        self.region_cache.invalidate_store_cache(store_id).await
    }

    async fn load_keyspace(&self, keyspace: &str) -> Result<keyspacepb::KeyspaceMeta> {
        self.pd.load_keyspace(keyspace).await
    }
}

impl PdRpcClient<TikvConnect, Cluster> {
    pub async fn connect(
        pd_endpoints: &[String],
        config: Config,
        enable_codec: bool,
    ) -> Result<PdRpcClient> {
        PdRpcClient::new(
            config.clone(),
            |security_mgr| TikvConnect::new(security_mgr, config.timeout),
            |security_mgr| RetryClient::connect(pd_endpoints, security_mgr, config.timeout),
            enable_codec,
        )
        .await
    }
}

impl<KvC: KvConnect + Send + Sync + 'static, Cl> PdRpcClient<KvC, Cl> {
    pub async fn new<PdFut, MakeKvC, MakePd>(
        config: Config,
        kv_connect: MakeKvC,
        pd: MakePd,
        enable_codec: bool,
    ) -> Result<PdRpcClient<KvC, Cl>>
    where
        PdFut: Future<Output = Result<RetryClient<Cl>>>,
        MakeKvC: FnOnce(Arc<SecurityManager>) -> KvC,
        MakePd: FnOnce(Arc<SecurityManager>) -> PdFut,
    {
        let security_mgr = Arc::new(
            if let (Some(ca_path), Some(cert_path), Some(key_path)) =
                (&config.ca_path, &config.cert_path, &config.key_path)
            {
                SecurityManager::load(ca_path, cert_path, key_path)?
            } else {
                SecurityManager::default()
            },
        );

        let pd = Arc::new(pd(security_mgr.clone()).await?);
        let kv_client_cache = Default::default();
        Ok(PdRpcClient {
            pd: pd.clone(),
            kv_client_cache,
            kv_connect: kv_connect(security_mgr),
            enable_codec,
            region_cache: RegionCache::new(pd),
        })
    }

    async fn kv_client(&self, address: &str) -> Result<KvC::KvClient> {
        if let Some(client) = self.kv_client_cache.read().await.get(address) {
            return Ok(client.clone());
        };
        info!("connect to tikv endpoint: {:?}", address);
        match self.kv_connect.connect(address).await {
            Ok(client) => {
                self.kv_client_cache
                    .write()
                    .await
                    .insert(address.to_owned(), client.clone());
                Ok(client)
            }
            Err(e) => Err(e),
        }
    }
}

fn make_key_range(start_key: Vec<u8>, end_key: Vec<u8>) -> kvrpcpb::KeyRange {
    let mut key_range = kvrpcpb::KeyRange::default();
    key_range.start_key = start_key;
    key_range.end_key = end_key;
    key_range
}

#[cfg(test)]
pub mod test {
    use futures::executor;
    use futures::executor::block_on;

    use super::*;
    use crate::mock::*;

    #[tokio::test]
    async fn test_kv_client_caching() {
        let client = block_on(pd_rpc_client());

        let addr1 = "foo";
        let addr2 = "bar";

        let kv1 = client.kv_client(addr1).await.unwrap();
        let kv2 = client.kv_client(addr2).await.unwrap();
        let kv3 = client.kv_client(addr2).await.unwrap();
        assert!(kv1.addr != kv2.addr);
        assert_eq!(kv2.addr, kv3.addr);
    }

    #[test]
    fn test_group_keys_by_region() {
        let client = MockPdClient::default();

        // FIXME This only works if the keys are in order of regions. Not sure if
        // that is a reasonable constraint.
        let tasks: Vec<Key> = vec![
            vec![1].into(),
            vec![2].into(),
            vec![3].into(),
            vec![5, 2].into(),
            vec![12].into(),
            vec![11, 4].into(),
        ];

        let stream = Arc::new(client).group_keys_by_region(tasks.into_iter());
        let mut stream = executor::block_on_stream(stream);

        let result: Vec<Key> = stream.next().unwrap().unwrap().0;
        assert_eq!(
            result,
            vec![
                vec![1].into(),
                vec![2].into(),
                vec![3].into(),
                vec![5, 2].into()
            ]
        );
        assert_eq!(
            stream.next().unwrap().unwrap().0,
            vec![vec![12].into(), vec![11, 4].into()]
        );
        assert!(stream.next().is_none());
    }

    #[test]
    fn test_regions_for_range() {
        let client = Arc::new(MockPdClient::default());
        let k1: Key = vec![1].into();
        let k2: Key = vec![5, 2].into();
        let k3: Key = vec![11, 4].into();
        let range1 = (k1, k2.clone()).into();
        let mut stream = executor::block_on_stream(client.clone().regions_for_range(range1));
        assert_eq!(stream.next().unwrap().unwrap().id(), 1);
        assert!(stream.next().is_none());

        let range2 = (k2, k3).into();
        let mut stream = executor::block_on_stream(client.regions_for_range(range2));
        assert_eq!(stream.next().unwrap().unwrap().id(), 1);
        assert_eq!(stream.next().unwrap().unwrap().id(), 2);
        assert!(stream.next().is_none());
    }

    #[test]
    fn test_group_ranges_by_region() {
        let client = Arc::new(MockPdClient::default());
        let k1 = vec![1];
        let k2 = vec![5, 2];
        let k3 = vec![11, 4];
        let k4 = vec![16, 4];
        let k5 = vec![250, 251];
        let k6 = vec![255, 251];
        let k_split = vec![10];
        let range1 = make_key_range(k1.clone(), k2.clone());
        let range2 = make_key_range(k1.clone(), k3.clone());
        let range3 = make_key_range(k2.clone(), k4.clone());
        let ranges = vec![range1, range2, range3];

        let mut stream = executor::block_on_stream(client.clone().group_ranges_by_region(ranges));
        let ranges1 = stream.next().unwrap().unwrap();
        let ranges2 = stream.next().unwrap().unwrap();
        let ranges3 = stream.next().unwrap().unwrap();
        let ranges4 = stream.next().unwrap().unwrap();

        assert_eq!(ranges1.1.id(), 1);
        assert_eq!(
            ranges1.0,
            vec![
                make_key_range(k1.clone(), k2.clone()),
                make_key_range(k1.clone(), k_split.clone()),
            ]
        );
        assert_eq!(ranges2.1.id(), 2);
        assert_eq!(ranges2.0, vec![make_key_range(k_split.clone(), k3.clone())]);
        assert_eq!(ranges3.1.id(), 1);
        assert_eq!(ranges3.0, vec![make_key_range(k2.clone(), k_split.clone())]);
        assert_eq!(ranges4.1.id(), 2);
        assert_eq!(ranges4.0, vec![make_key_range(k_split, k4.clone())]);
        assert!(stream.next().is_none());

        let range1 = make_key_range(k1.clone(), k2.clone());
        let range2 = make_key_range(k3.clone(), k4.clone());
        let range3 = make_key_range(k5.clone(), k6.clone());
        let ranges = vec![range1, range2, range3];
        stream = executor::block_on_stream(client.group_ranges_by_region(ranges));
        let ranges1 = stream.next().unwrap().unwrap();
        let ranges2 = stream.next().unwrap().unwrap();
        let ranges3 = stream.next().unwrap().unwrap();
        assert_eq!(ranges1.1.id(), 1);
        assert_eq!(ranges1.0, vec![make_key_range(k1, k2)]);
        assert_eq!(ranges2.1.id(), 2);
        assert_eq!(ranges2.0, vec![make_key_range(k3, k4)]);
        assert_eq!(ranges3.1.id(), 3);
        assert_eq!(ranges3.0, vec![make_key_range(k5, k6)]);
    }
}
