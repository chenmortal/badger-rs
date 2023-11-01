use std::{
    collections::{HashSet, VecDeque},
    ops::Deref,
    sync::{
        atomic::{AtomicBool, AtomicU32},
        Arc,
    },
};

use crate::{
    default::{KV_WRITES_ENTRIES_CHANNEL_CAPACITY, LOCK_FILE},
    errors::DBError,
    key_registry::{self, KeyRegistry},
    kv::{KeyTs, ValueStruct},
    level::levels::LevelsController,
    // manifest::open_create_manifestfile,
    memtable::MemTable,
    options::Options,
    table::block::{self, Block},
    txn::oracle::Oracle,
    util::closer::Closer,
    util::metrics::calculate_size,
    util::{
        cache::{BlockCache, IndexCache},
        lock::DBLockGuard,
        publisher::Publisher,
        rayon::init_global_rayon_pool,
    },
    vlog::{threshold::VlogThreshold, ValueLog},
    write::WriteReq,
};
use anyhow::anyhow;
use bytes::Buf;
use stretto::AsyncCache;
use tokio::sync::{mpsc::Sender, RwLock};
use tokio::sync::{
    mpsc::{self, Receiver},
    Mutex,
};

// pub(crate) type BlockCache = AsyncCache<Vec<u8>, Block>;
// pub(crate) type IndexCache = AsyncCache<u64, Vec<u8>>;
#[derive(Debug)]
pub(crate) struct NextId(AtomicU32);
impl NextId {
    #[inline]
    pub(crate) fn new() -> Self {
        Self(AtomicU32::new(0))
    }
    #[inline]
    pub(crate) fn get_next_id(&self) -> u32 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
    #[inline]
    pub(crate) fn add_next_id(&self) -> u32 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
    #[inline]
    pub(crate) fn store(&self, val: u32) {
        self.0.store(val, std::sync::atomic::Ordering::SeqCst);
    }
}
#[derive(Debug, Clone)]
pub struct DB(Arc<DBInner>);
impl Deref for DB {
    type Target = DBInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
#[derive(Debug)]
pub struct DBInner {
    // pub(crate) next_mem_fid: NextId,
    pub(crate) key_registry: KeyRegistry,
    pub(crate) memtable: Option<Arc<RwLock<MemTable>>>,
    pub(crate) immut_memtable: RwLock<VecDeque<Arc<MemTable>>>,
    pub(crate) block_cache: Option<BlockCache>,
    pub(crate) index_cache: Option<IndexCache>,
    pub(crate) level_controller: LevelsController,
    pub(crate) oracle: Arc<Oracle>,
    pub(crate) send_write_req: Sender<WriteReq>,
    pub(crate) flush_memtable: Sender<Arc<MemTable>>,
    pub(crate) recv_memtable: Mutex<Receiver<Arc<MemTable>>>,
    pub(crate) vlog: ValueLog,
    banned_namespaces: RwLock<HashSet<u64>>,
    pub(crate) publisher: Publisher,
    is_closed: AtomicBool,
    pub(crate) block_writes: AtomicBool,
    pub(crate) opt: Options,
    pub(crate) lock_guard: Option<DBLockGuard>,
}
impl DBInner {
    pub async fn open(mut opt: Options) -> anyhow::Result<DB> {
        Options::init(opt.clone())?;

        let lock_guard = opt.lock_guard.try_build()?;

        init_global_rayon_pool()?;
        let manifest_file = opt.manifest.build()?;
        let block_cache = opt.block_cache.try_build()?;
        let index_cache = opt.index_cache.try_build()?;

        let key_registry = opt.key_registry.build().await?;

        calculate_size().await;
        // let mut update_size_closer = Closer::new();
        // let update_size_handle = tokio::spawn(update_size(update_size_closer.sem_clone()));

        // let next_mem_fid = NextId::new();
        let immut_memtable = opt.memtable.open_many(&key_registry).await?.into();
        let mut memtable = None;
        if !Options::read_only() {
            memtable = Arc::new(RwLock::new(opt.memtable.new(&key_registry).await?)).into();
        }

        // let level_controller = LevelsController::new(
        //     &manifest_file.manifest,
        //     key_registry.clone(),
        //     &block_cache,
        //     &index_cache,
        //     opt.memtable.memtable_size(),
        // )
        // .await?;
        let level_controller = opt
            .level_controller
            .build(
                &manifest_file.manifest,
                key_registry.clone(),
                &block_cache,
                &index_cache,
            )
            .await?;
        let threshold = VlogThreshold::new();

        let vlog = ValueLog::new(threshold, key_registry.clone())?;
        let closer = Closer::new(1);
        let publisher = Publisher::new(closer.clone());
        let (send_write_req, receiver) = mpsc::channel(KV_WRITES_ENTRIES_CHANNEL_CAPACITY);
        let (flush_memtable, recv_memtable) = mpsc::channel(Options::num_memtables());
        let db: DB = DB(Arc::new(Self {
            key_registry,
            memtable,
            immut_memtable,
            block_cache,
            index_cache,
            level_controller,
            oracle: Default::default(),
            send_write_req,
            flush_memtable,
            vlog,
            banned_namespaces: Default::default(),
            publisher,
            is_closed: AtomicBool::new(false),
            block_writes: AtomicBool::new(false),
            recv_memtable: recv_memtable.into(),
            opt,
            lock_guard,
        }));
        let flush_memtable = Closer::new(1);
        let _p = tokio::spawn(db.clone().flush_memtable(flush_memtable.clone()));
        // drop(value_dir_lock_guard);
        // drop(dir_lock_guard);

        Ok(db)
    }

    pub(crate) fn update_size() {}
    pub(crate) fn is_closed(&self) -> bool {
        self.is_closed.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub(crate) async fn is_banned(&self, key: &[u8]) -> Result<(), DBError> {
        match Options::name_space_offset() {
            Some(offset) => {
                if key.len() <= offset + 8 {
                    return Ok(());
                }
                let mut p = &key[offset..offset + 8];
                let name_space = p.get_u64();
                let banned_r = self.banned_namespaces.read().await;
                let r = banned_r.contains(&name_space);
                drop(banned_r);
                if r {
                    Err(DBError::BannedKey)
                } else {
                    Ok(())
                }
            }
            None => Ok(()),
        }
    }
    pub(crate) async fn get_value(&self, key_ts: &KeyTs) -> anyhow::Result<ValueStruct> {
        // todo!();
        let v = ValueStruct::default();
        Ok(v)
    }
}
