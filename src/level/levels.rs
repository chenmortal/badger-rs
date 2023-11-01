use std::{
    collections::{BTreeMap, HashSet},
    fs::{remove_file, OpenOptions},
    path::PathBuf,
    sync::{
        atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

use anyhow::anyhow;
use anyhow::bail;
use log::{debug, error, info};
use rand::Rng;
use tokio::{
    select,
    sync::{Mutex, Notify, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    compaction::{CompactStatus, KeyRange},
    level_handler::LevelHandler,
};
#[cfg(feature = "metrics")]
use crate::util::metrics::{add_num_compaction_tables, sub_num_compaction_tables};
use crate::{
    key_registry::KeyRegistry,
    level::compaction::LevelCompactStatus,
    manifest::Manifest,
    options::Options,
    pb::{ChecksumError, ERR_CHECKSUM_MISMATCH},
    table::{
        iter::{ConcatIter, TableIter},
        merge::MergeIter,
        opt::TableOption,
        Table,
    },
    txn::oracle::Oracle,
    util::closer::Closer,
    util::{
        cache::{BlockCache, IndexCache},
        mmap::MmapFile,
        DBFileId,
    },
    util::{compare_key, key_with_ts, parse_key, Throttle},
    util::{sys::sync_dir, SSTableId},
};
#[derive(Debug)]
pub(crate) struct LevelsController {
    next_file_id: AtomicU32,
    l0_stalls_ms: AtomicI64,
    levels: Vec<LevelHandler>,
    compact_status: CompactStatus,
    memtable_size: usize,
}
struct Targets {
    base_level: usize,
    target_size: Vec<usize>,
    file_size: Vec<usize>,
}
struct CompactionPriority {
    level: usize,
    score: f64,
    adjusted: f64,
    drop_prefixes: Vec<Vec<u8>>,
    targets: Targets,
}
pub(super) struct CompactDef {
    compactor_id: usize,
    // targets: Targets,
    priority: CompactionPriority,
    pub(super) this_level: LevelHandler,
    pub(super) next_level: LevelHandler,
    pub(super) top: Vec<Table>,
    pub(super) bottom: Vec<Table>,
    pub(super) this_range: KeyRange,
    pub(super) next_range: KeyRange,
    splits: Vec<KeyRange>,
    this_size: usize,
    // drop_prefixes: Vec<Vec<u8>>,
}
#[derive(Debug, Clone)]
pub struct LevelsControllerBuilder {
    dir: PathBuf,
    read_only: bool,
    memtable_size: usize,
    num_level_zero_tables_stall: usize,
    num_level_zero_tables: usize,
    max_levels: usize,
}
impl Default for LevelsControllerBuilder {
    fn default() -> Self {
        Self {
            dir: Default::default(),
            read_only: Default::default(),
            memtable_size: Default::default(),
            num_level_zero_tables_stall: Default::default(),
            num_level_zero_tables: Default::default(),
            max_levels: Default::default(),
        }
    }
}
impl LevelsControllerBuilder {
    pub(crate) async fn build(
        &self,
        manifest: &Arc<parking_lot::Mutex<Manifest>>,
        key_registry: KeyRegistry,
        block_cache: &Option<BlockCache>,
        index_cache: &Option<IndexCache>,
    ) -> anyhow::Result<LevelsController> {
        assert!(self.num_level_zero_tables_stall > self.num_level_zero_tables);

        let compact_status = CompactStatus::default();
        let mut compact_status_w = compact_status.write();
        compact_status_w
            .levels_mut()
            .resize_with(self.max_levels, LevelCompactStatus::default);
        drop(compact_status_w);

        let levels_control = LevelsController {
            next_file_id: Default::default(),
            l0_stalls_ms: Default::default(),
            levels: (0..self.max_levels)
                .map(|x| LevelHandler::new(x))
                .collect::<Vec<_>>(),
            compact_status,
            memtable_size: self.memtable_size,
        };

        let (max_file_id, mut level_tables) = self
            .open_tables_by_manifest(manifest, key_registry, block_cache, index_cache)
            .await?;
        let next_file_id = AtomicU32::new(max_file_id + 1);

        let mut levels = Vec::with_capacity(level_tables.len());
        let mut level = 0;
        for tables in level_tables {
            let handler = LevelHandler::new(level);
            level += 1;
            handler.init_tables(tables).await;
            levels.push(handler);
        }
        // for level in 0..level_tables.len() {
        //     let handler = LevelHandler::new(level);
        //     level_tables.drain(0..);
        //     handler.init_tables(level_tables[level]).await;
        //     levels.push(handler);
        // }

        // let manifest_lock = manifest.lock();
        // let manifest = &*manifest_lock;

        // revert_to_manifest(
        //     &self.dir,
        //     manifest,
        //     SSTableId::parse_set_from_dir(&self.dir),
        // )?;

        // let num_opened = Arc::new(AtomicUsize::new(0));
        // let mut throttle = Throttle::new(3);
        // let tables_len = manifest.tables.len();
        // let watch_cancel_token = Self::watch_num_opened(num_opened.clone(), tables_len);

        // let mut max_file_id: u32 = 0;
        // let mut open_table_tasks = BTreeMap::<u8, Vec<JoinHandle<Option<Table>>>>::new();
        // for (file_id, table_manifest) in manifest.tables.iter() {
        //     let num_opened_clone = num_opened.clone();
        //     let path = file_id.join_dir(&self.dir);
        //     let permit = throttle.acquire().await?;

        //     max_file_id = max_file_id.max((*file_id).into());

        //     let compression = table_manifest.compression;
        //     let key_registry_clone = key_registry.clone();
        //     let block_cache_clone = block_cache.clone();
        //     let index_cache_clone = index_cache.clone();
        //     let read_only = self.read_only;
        //     let future = async move {
        //         let mut table_opt =
        //             TableOption::new(&key_registry_clone, &block_cache_clone, &index_cache_clone)
        //                 .await;
        //         table_opt.set_compression(compression);
        //         let mut fp_open_opt = OpenOptions::new();
        //         fp_open_opt.read(true).write(!read_only);

        //         let (mmap_f, _is_new) = MmapFile::open(&path, fp_open_opt, 0)?;

        //         match Table::open(mmap_f, table_opt).await {
        //             Ok(table) => {
        //                 return Ok(table.into());
        //             }
        //             Err(e) => {
        //                 if e.downcast_ref::<ChecksumError>().is_some() {
        //                     error!("{}", e);
        //                     error!("Ignoring table {:?}", path);
        //                 } else {
        //                     bail!("Opening table:{:?} for {}", path, e)
        //                 };
        //             }
        //         };
        //         Ok(None)
        //     };
        //     debug_assert!((table_manifest.level as usize) < self.max_levels);
        //     let task = tokio::spawn(async move {
        //         let table = permit.done_with_future(future).await;
        //         num_opened_clone.fetch_add(1, Ordering::Relaxed);
        //         table.and_then(|x| x)
        //     });
        //     match open_table_tasks.get_mut(&table_manifest.level) {
        //         Some(tasks) => {
        //             tasks.push(task);
        //         }
        //         None => {
        //             open_table_tasks.insert(table_manifest.level, vec![task]);
        //         }
        //     }
        // }
        // drop(manifest_lock);
        // throttle.finish().await?;
        // watch_cancel_token.cancel();

        // levels_control
        //     .next_file_id
        //     .store(max_file_id + 1, Ordering::SeqCst);

        // // let tables_m = tables.lock().await;
        // for (level, tasks) in open_table_tasks {
        //     let mut tables = Vec::with_capacity(tasks.len());
        //     for handle in tasks {
        //         if let Some(t) = handle.await? {
        //             tables.push(t);
        //         };
        //     }
        //     levels_control.levels[level as usize]
        //         .init_tables(&tables)
        //         .await;
        // }
        // for
        // for ele in open_table_tasks. {

        // }
        // for (i, tables) in tables_m.iter() {
        //     levels_control.levels[*i as usize].init_tables(tables).await;
        // }
        // drop(tables_m);
        match levels_control.validate().await {
            Ok(_) => {}
            Err(e) => {
                let _ = levels_control.cleanup_levels().await;
                bail!("Level validation for {}", e);
            }
        }

        match sync_dir(Options::dir()) {
            Ok(_) => {}
            Err(e) => {
                let _ = levels_control.cleanup_levels().await;
                bail!(e);
            }
        };
        Ok(levels_control)
    }
    fn watch_num_opened(num_opened: Arc<AtomicUsize>, tables_len: usize) -> CancellationToken {
        let start = tokio::time::Instant::now();
        let cancell = CancellationToken::new();
        let cancell_clone = cancell.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            loop {
                select! {
                    i=tick.tick()=>{
                        info!("{} tables out of {} opened in {}",
                        num_opened.load(Ordering::SeqCst),
                        tables_len,
                        i.duration_since(start).as_millis());
                    },
                    _stop=cancell_clone.cancelled()=>{
                        info!(
                            "All {} tables opened in {}",
                            num_opened.load(Ordering::SeqCst),
                            tokio::time::Instant::now()
                                .duration_since(start)
                                .as_millis()
                        );
                        break;
                    }
                };
            }
        });
        cancell
    }
    async fn open_tables_by_manifest(
        &self,
        // max_level:usize,
        // manifest: &Manifest,
        manifest: &Arc<parking_lot::Mutex<Manifest>>,
        // num_opened: Arc<AtomicUsize>,
        key_registry: KeyRegistry,
        block_cache: &Option<BlockCache>,
        index_cache: &Option<IndexCache>,
    ) -> anyhow::Result<(u32, Vec<Vec<Table>>)> {
        let manifest_lock = manifest.lock();
        let manifest = &*manifest_lock;

        revert_to_manifest(
            &self.dir,
            manifest,
            SSTableId::parse_set_from_dir(&self.dir),
        )?;
        let num_opened = Arc::new(AtomicUsize::new(0));
        // let mut throttle = Throttle::new(3);
        let tables_len = manifest.tables.len();
        let watch_cancel_token = Self::watch_num_opened(num_opened.clone(), tables_len);
        let mut max_file_id: u32 = 0;
        let mut throttle = Throttle::new(3);
        let mut open_table_tasks = Vec::new();
        open_table_tasks.resize_with(self.max_levels, Vec::new);
        // let mut open_table_tasks = BTreeMap::<u8, Vec<JoinHandle<Option<Table>>>>::new();
        for (file_id, table_manifest) in manifest.tables.iter() {
            let num_opened_clone = num_opened.clone();
            let path = file_id.join_dir(&self.dir);
            let permit = throttle.acquire().await?;

            max_file_id = max_file_id.max((*file_id).into());

            let compression = table_manifest.compression;
            let key_registry_clone = key_registry.clone();
            let block_cache_clone = block_cache.clone();
            let index_cache_clone = index_cache.clone();
            let read_only = self.read_only;
            let future = async move {
                let mut table_opt =
                    TableOption::new(&key_registry_clone, &block_cache_clone, &index_cache_clone)
                        .await;
                table_opt.set_compression(compression);
                let mut fp_open_opt = OpenOptions::new();
                fp_open_opt.read(true).write(!read_only);

                let (mmap_f, _is_new) = MmapFile::open(&path, fp_open_opt, 0)?;

                match Table::open(mmap_f, table_opt).await {
                    Ok(table) => {
                        return Ok(table.into());
                    }
                    Err(e) => {
                        if e.downcast_ref::<ChecksumError>().is_some() {
                            error!("{}", e);
                            error!("Ignoring table {:?}", path);
                        } else {
                            bail!("Opening table:{:?} for {}", path, e)
                        };
                    }
                };
                Ok(None)
            };
            // debug_assert!((table_manifest.level as usize) < self.max_levels);
            let task = tokio::spawn(async move {
                let table = permit.done_with_future(future).await;
                num_opened_clone.fetch_add(1, Ordering::Relaxed);
                table.and_then(|x| x)
            });
            let task_level = table_manifest.level as usize;
            if task_level < self.max_levels {
                open_table_tasks[task_level].push(task);
            } else {
                open_table_tasks.last_mut().unwrap().push(task);
            }
            // match open_table_tasks.get_mut(&table_manifest.level) {
            //     Some(tasks) => {
            //         tasks.push(task);
            //     }
            //     None => {
            //         open_table_tasks.insert(table_manifest.level, vec![task]);
            //     }
            // }
        }
        drop(manifest_lock);
        throttle.finish().await?;
        watch_cancel_token.cancel();
        let mut level_tables = Vec::new();
        for tasks in open_table_tasks {
            let mut tables = Vec::with_capacity(tasks.len());
            for task in tasks {
                if let Some(table) = task.await? {
                    tables.push(table);
                }
            }
            level_tables.push(tables);
        }
        Ok((max_file_id, level_tables))
        // open_table_tasks.drain(..).map(|x|for ele in x {

        // });
        // for ele in open_table_tasks {

        // }
        // levels_control
        //     .next_file_id
        //     .store(max_file_id + 1, Ordering::SeqCst);

        // for ele in open_table_tasks.iter_mut() {

        // }

        // let tables_m = tables.lock().await;
        // for (level, tasks) in open_table_tasks {
        //     let mut tables = Vec::with_capacity(tasks.len());
        //     for handle in tasks {
        //         if let Some(t) = handle.await? {
        //             tables.push(t);
        //         };
        //     }
        //     levels_control.levels[level as usize]
        //         .init_tables(&tables)
        //         .await;
        // }
        // Ok(())
    }
}
impl LevelsController {
    async fn validate(&self) -> anyhow::Result<()> {
        for level_handler in self.levels.iter() {
            level_handler
                .validate()
                .await
                .map_err(|e| anyhow!("Levels Controller {}", e))?;
        }
        Ok(())
    }
    async fn cleanup_levels(&self) -> anyhow::Result<()> {
        let mut first_err = None;
        for level_handler in self.levels.iter() {
            match level_handler.sync_mmap().await {
                Ok(_) => {}
                Err(e) => {
                    if first_err.is_none() {
                        first_err = e.into();
                    }
                }
            }
        }
        match first_err {
            Some(e) => {
                bail!(e)
            }
            None => Ok(()),
        }
    }
    #[inline]
    fn last_level(&self) -> &LevelHandler {
        debug_assert!(self.levels.len() > 0);
        self.levels.last().unwrap()
    }
    pub(crate) async fn start_compact(
        level_controller: Arc<Self>,
        opt: &Arc<Options>,
        closer: &mut Closer,
        sem: Arc<Semaphore>,
        oracle: &Arc<Oracle>,
    ) {
        let num = Options::num_compactors();
        for task_id in 0..num {
            let closer_c = closer.clone();
            let opt_clone = opt.clone();
            let oracle_clone = oracle.clone();
            let level_controller_clone = level_controller.clone();
            tokio::spawn(async move {
                level_controller_clone
                    .run_compact(task_id, closer_c, opt_clone, &oracle_clone)
                    .await;
            });
        }
    }

    pub(crate) async fn run_compact(
        &self,
        task_id: usize,
        closer: Closer,
        // sem: Arc<Semaphore>,
        opt: Arc<Options>,
        oracle: &Arc<Oracle>,
    ) {
        let sleep =
            tokio::time::sleep(Duration::from_millis(rand::thread_rng().gen_range(0..1000)));
        select! {
            _=sleep=>{},
            _=closer.captured()=>{return ;}
        }
        let mut count = 0;
        let mut ticker = tokio::time::interval(Duration::from_millis(50));

        // let level = self.last_level().get_level().await;
        // let targets = self.level_targets(&opt).await;
        // ticker.tick()
        // fn run (priotirty:CompactionPriority){

        // }
        // let run= |priority:CompactionPriority|{

        // };
        let priority = CompactionPriority {
            level: self.last_level().get_level(),
            score: 0.0,
            adjusted: 0.0,
            drop_prefixes: Vec::new(),
            targets: self.level_targets().await,
        };
        self.do_compact(task_id, priority, &opt, oracle).await;
        loop {
            select! {
                _=ticker.tick()=>{
                    count+=1;
                    // if Options::lmax_compaction  && task_id==2 && count >=200{

                    // }
                }
                _=closer.captured()=>{return ;}
            }
        }
    }

    async fn run_compact_def(
        &self,
        task_id: usize,
        level: usize,
        compact_def: &mut CompactDef,
    ) -> anyhow::Result<()> {
        if compact_def.priority.targets.file_size.len() == 0 {
            bail!("Filesizes cannot be zero. Targets are not set");
        }
        let time_start = SystemTime::now();

        // let this_level = compact_def.this_level.clone();
        // let next_level = compact_def.next_level.clone();

        debug_assert!(compact_def.splits.len() == 0);

        if compact_def.this_level.get_level() != compact_def.next_level.get_level() {
            self.add_splits(compact_def).await;
        }

        if compact_def.splits.len() == 0 {
            compact_def.splits.push(KeyRange::default());
        }

        let num_tables = compact_def.top.len() + compact_def.bottom.len();
        #[cfg(feature = "metrics")]
        add_num_compaction_tables(num_tables);
        let result = self.compact_build_tables(level, compact_def).await;
        #[cfg(feature = "metrics")]
        sub_num_compaction_tables(num_tables);
        result?;
        Ok(())
    }

    async fn do_compact(
        &self,
        task_id: usize,
        mut priority: CompactionPriority,
        opt: &Arc<Options>,
        oracle: &Arc<Oracle>,
    ) -> anyhow::Result<()> {
        let priority_level = priority.level;
        debug_assert!(priority_level < Options::max_levels());
        if priority.targets.base_level == 0 {
            priority.targets = self.level_targets().await;
        }
        let this_level = self.levels[priority_level].clone();
        let next_level = if priority_level == 0 {
            self.levels[priority.targets.base_level].clone()
        } else {
            this_level.clone()
        };

        let mut compact_def = CompactDef {
            compactor_id: task_id,
            this_level,
            next_level,
            top: Vec::new(),
            bottom: Vec::new(),
            this_range: KeyRange::default(),
            next_range: KeyRange::default(),
            splits: Vec::new(),
            this_size: 0,
            priority,
        };
        if priority_level == 0 {
            if !self.fill_tables_level0(&mut compact_def).await {
                bail!("Unable to fill tables")
            };
        } else {
            if priority_level != Options::max_levels() - 1 {
                compact_def.next_level = self.levels[priority_level + 1].clone();
            }
            if !self.fill_tables(&mut compact_def, oracle).await {
                bail!("Unable to fill tables")
            };
        }
        Ok(())
    }
    async fn level_targets(&self) -> Targets {
        let levels_len = self.levels.len();
        let mut targets = Targets {
            base_level: 0,
            target_size: vec![0; levels_len],
            file_size: vec![0; levels_len],
        };
        let mut level_size = self.last_level().get_total_size().await;
        for i in (1..levels_len).rev() {
            targets.target_size[i] = level_size.max(Options::base_level_size());
            if targets.base_level == 0 && level_size <= Options::base_level_size() {
                targets.base_level = i;
            }
            level_size /= Options::level_size_multiplier();
        }

        let mut table_size = Options::base_table_size();
        for i in 0..levels_len {
            targets.file_size[i] = if i == 0 {
                self.memtable_size
            } else if i <= targets.base_level {
                table_size
            } else {
                table_size *= Options::table_size_multiplier();
                table_size
            }
        }

        for i in targets.base_level + 1..levels_len - 1 {
            if self.levels[i].get_total_size().await > 0 {
                break;
            }
            targets.base_level = i;
        }

        let base_level = targets.base_level;
        let levels = &self.levels;

        if base_level < levels.len() - 1
            && levels[base_level].get_total_size().await == 0
            && levels[base_level + 1].get_total_size().await < targets.target_size[base_level + 1]
        {
            targets.base_level += 1;
        }
        targets
    }
    async fn try_fill_max_level_tables(
        &self,
        compact_def: &mut CompactDef,
        oracle: &Arc<Oracle>,
    ) -> Option<bool> {
        let this_r = compact_def.this_level.handler_tables.read().await;
        let next_r = compact_def.next_level.handler_tables.read().await;
        let tables = this_r.tables.clone();
        if tables.len() == 0 {
            return false.into();
        }
        if compact_def.this_level.get_level() != Options::max_levels() - 1 {
            return None;
        }
        let mut sorted_tables = tables.clone();
        if sorted_tables.len() != 0 {
            sorted_tables.sort_unstable_by(|a, b| b.stale_data_size().cmp(&a.stale_data_size()));
        }

        if sorted_tables.len() > 0 && sorted_tables[0].stale_data_size() == 0 {
            return false.into();
        }
        compact_def.bottom.clear();

        let now = SystemTime::now();

        for table in sorted_tables {
            if table.max_version() > oracle.discard_at_or_below().await {
                continue;
            }

            if now.duration_since(table.created_at()).unwrap() < Duration::from_secs(60 * 60) {
                continue;
            }

            if table.stale_data_size() < 10 << 20 {
                continue;
            }

            compact_def.this_size = table.size();
            compact_def.this_range = KeyRange::from_table(&table).await;
            compact_def.next_range = compact_def.this_range.clone();
            let this_level = compact_def.this_level.get_level();
            if self
                .compact_status
                .is_overlaps_with(this_level, &compact_def.this_range)
            {
                continue;
            };
            let table_size = table.size();
            compact_def.top = vec![table.clone()];
            let need_file_size = compact_def.priority.targets.file_size[this_level];
            if table_size >= need_file_size {
                break;
            }

            // collect_bottom_tables
            let mut total_size = table_size;
            let mut j =
                match tables.binary_search_by(|a| compare_key(a.smallest(), table.smallest())) {
                    Ok(s) => s,
                    Err(s) => s,
                };
            debug_assert!(tables[j].id() == table.id());
            j += 1;
            while j < tables.len() {
                let new_t = &tables[j];
                total_size += new_t.size();
                if total_size >= need_file_size {
                    break;
                }
                compact_def.bottom.push(new_t.clone());
                compact_def
                    .next_range
                    .extend(KeyRange::from_table(new_t).await);
                j += 1;
            }
            //

            if !self.compact_status.compare_and_add(compact_def) {
                compact_def.bottom.clear();
                compact_def.next_range = KeyRange::default();
                continue;
            };
            return true.into();
        }
        if compact_def.top.len() == 0 {
            return false.into();
        }

        let r = self.compact_status.compare_and_add(compact_def);
        drop(this_r);
        drop(next_r);
        return r.into();
    }
    async fn fill_tables(&self, compact_def: &mut CompactDef, oracle: &Arc<Oracle>) -> bool {
        //if compact_def.this_level.level is not last return None;
        if let Some(s) = self.try_fill_max_level_tables(compact_def, oracle).await {
            return s;
        }

        let this_level_r = compact_def.this_level.handler_tables.read().await;
        let next_level_r = compact_def.next_level.handler_tables.read().await;
        let mut tables = this_level_r.tables.clone();
        tables.sort_unstable_by(|a, b| a.max_version().cmp(&b.max_version()));

        for table in tables {
            compact_def.this_size = table.size();
            compact_def.this_range = KeyRange::from_table(&table).await;

            if self
                .compact_status
                .is_overlaps_with(compact_def.this_level.get_level(), &compact_def.this_range)
            {
                continue;
            };
            compact_def.top = vec![table.clone()];

            let (left_index, right_index) = compact_def
                .next_level
                .overlapping_tables(&compact_def.this_range)
                .await;
            compact_def.bottom = next_level_r.tables[left_index..right_index].to_vec();

            if compact_def.bottom.len() == 0 {
                compact_def.next_range = compact_def.this_range.clone();
                if !self.compact_status.compare_and_add(&compact_def) {
                    continue;
                };
                return true;
            }

            compact_def.next_range = KeyRange::from_tables(&compact_def.bottom).await.unwrap(); //bottom.len !=0 so can unwrap()

            if self
                .compact_status
                .is_overlaps_with(compact_def.next_level.get_level(), &compact_def.next_range)
            {
                continue;
            };

            if !self.compact_status.compare_and_add(compact_def) {
                continue;
            };
            return true;
        }
        false
    }
    async fn fill_tables_level0(&self, compact_def: &mut CompactDef) -> bool {
        if self.fill_tables_level0_to_levelbase(compact_def).await {
            true
        } else {
            self.fill_tables_level0_to_level0(compact_def).await
        }
    }
    async fn fill_tables_level0_to_levelbase(&self, compact_def: &mut CompactDef) -> bool {
        if compact_def.next_level.get_level() == 0 {
            panic!("Base level can't be zero");
        }

        if compact_def.priority.adjusted > 0.0 && compact_def.priority.adjusted < 1.0 {
            return false;
        }

        let this_level_r = compact_def.this_level.handler_tables.read().await;
        let next_level_r = compact_def.next_level.handler_tables.read().await;

        if this_level_r.tables.len() == 0 {
            return false;
        };
        let mut top = Vec::new();
        if compact_def.priority.drop_prefixes.len() == 0 {
            let mut key_range = KeyRange::default();
            for table in this_level_r.tables.iter() {
                let k = KeyRange::from_table(table).await;
                if key_range.is_overlaps_with(&k) {
                    top.push(table.clone());
                    key_range.extend(k);
                } else {
                    break;
                };
            }
        } else {
            top = this_level_r.tables.clone();
        }

        compact_def.this_range = KeyRange::from_tables(&top).await.unwrap();
        compact_def.top = top;

        let (left_index, right_index) = compact_def
            .next_level
            .overlapping_tables(&compact_def.this_range)
            .await;

        compact_def.bottom = next_level_r.tables[left_index..right_index].to_vec();

        compact_def.next_range = if compact_def.bottom.len() == 0 {
            compact_def.this_range.clone()
        } else {
            KeyRange::from_tables(&compact_def.bottom).await.unwrap() //len!=0 so can unwrap()
        };

        let r = self.compact_status.compare_and_add(compact_def);
        drop(this_level_r);
        drop(next_level_r);
        return r;
    }

    async fn fill_tables_level0_to_level0(&self, compact_def: &mut CompactDef) -> bool {
        if compact_def.compactor_id != 0 {
            return false;
        }

        compact_def.next_level = self.levels[0].clone();
        compact_def.next_range = KeyRange::default();
        compact_def.bottom.clear();

        debug_assert!(compact_def.this_level.get_level() == 0);
        debug_assert!(compact_def.next_level.get_level() == 0);

        let targets = &mut compact_def.priority.targets;

        let this_level_handler_r = compact_def.this_level.handler_tables.read().await;
        let mut compact_status_w = self.compact_status.write();
        let mut out = Vec::new();
        let now = SystemTime::now();

        for table in this_level_handler_r.tables.iter() {
            if table.size() >= targets.file_size[0] {
                continue;
            }

            if now.duration_since(table.created_at()).unwrap() < Duration::from_secs(10) {
                continue;
            };

            if compact_status_w.tables().contains(&table.id()) {
                continue;
            }
            out.push(table.clone());
        }
        drop(this_level_handler_r);
        if out.len() < 4 {
            return false;
        }

        compact_def.this_range = KeyRange::default_with_inf();
        compact_def.top = out;

        let this_level_compact_status =
            &mut compact_status_w.levels_mut()[compact_def.this_level.get_level()];
        this_level_compact_status
            .0
            .ranges
            .push(KeyRange::default_with_inf());

        for table in compact_def.top.iter() {
            compact_status_w.tables_mut().insert(table.id());
        }
        targets.file_size[0] = u32::MAX as usize;
        drop(compact_status_w);
        true
    }
    async fn add_splits(&self, compact_def: &mut CompactDef) {
        compact_def.splits.clear();
        let mut width = (compact_def.bottom.len() as f64 / 5.0).ceil() as usize;
        width = width.max(3);
        let mut skr = compact_def.this_range.clone();
        skr.extend_borrow(&compact_def.next_range);

        for i in 0..compact_def.bottom.len() {
            if i == compact_def.bottom.len() - 1 {
                skr.right.clear();
                compact_def.splits.push(skr.clone());
                return;
            }
            if i % width == width - 1 {
                let biggest = compact_def.bottom[i].0.biggest.read().await;
                skr.right = key_with_ts(parse_key(&biggest), 0);
                compact_def.splits.push(skr.clone());
                skr.left = skr.right.clone();
            }
        }
    }
    async fn compact_build_tables(
        &self,
        level: usize,
        compact_def: &mut CompactDef,
    ) -> anyhow::Result<()> {
        let mut valid = Vec::new();
        't: for table in compact_def.bottom.iter() {
            for prefix in compact_def.priority.drop_prefixes.iter() {
                if table.smallest().starts_with(&prefix) {
                    let biggest = table.0.biggest.read().await;
                    if biggest.starts_with(&prefix) {
                        continue 't;
                    }
                };
            }
            valid.push(table.clone());
        }

        let mut out = Vec::new();
        if level == 0 {
            compact_def
                .top
                .iter()
                .rev()
                .for_each(|t| out.push(t.clone()));
        } else if compact_def.top.len() > 0 {
            out.push(compact_def.top[0].clone());
        };

        let mut throttle = Throttle::new(3);
        for key_range in compact_def.splits.iter() {
            match throttle.acquire().await {
                Ok(permit) => {
                    let out_concat = ConcatIter::new(out.clone(), false, false);
                    let valid_concat = ConcatIter::new(valid.clone(), false, false);
                    let merget_iter = MergeIter::new(vec![out_concat, valid_concat], false);
                    tokio::spawn(async move {
                        permit.done_with_error(None).await;
                    });
                }
                Err(e) => {
                    error!("cannot start subcompaction: {}", e);
                    bail!(e)
                }
            };
        }
        Ok(())
    }
    async fn sub_compact(
        &self,
        merget_iter: MergeIter<TableIter>,
        key_range: KeyRange,
        compact_def: &mut CompactDef,
        oracle: &Arc<Oracle>,
    ) {
        let mut all_tables = Vec::with_capacity(compact_def.top.len() + compact_def.bottom.len());
        all_tables.extend_from_slice(&compact_def.top);
        all_tables.extend_from_slice(&compact_def.bottom);

        let has_overlap = self
            .check_overlap(&all_tables, compact_def.next_level.get_level() + 1)
            .await;

        let discard_ts = oracle.discard_at_or_below().await;
    }
    async fn check_overlap(&self, tables: &Vec<Table>, level: usize) -> bool {
        let key_range = KeyRange::from_tables(&tables).await.unwrap();
        for i in level..self.levels.len() {
            let (left, right) = self.levels[i].overlapping_tables(&key_range).await;
            if right - left > 0 {
                return true;
            }
        }
        return false;
    }
    pub(crate) fn get_reserve_file_id(&self) -> SSTableId {
        self.next_file_id.fetch_add(1, Ordering::AcqRel).into()
    }
}

// #[inline]
// async fn close_all_tables(tables: &Arc<Mutex<BTreeMap<u8, Vec<Table>>>>) {
//     let tables_m = tables.lock().await;
//     for (_, table_slice) in tables_m.iter() {
//         for table in table_slice {
//             let _ = table.sync_mmap();
//         }
//     }
//     drop(tables_m);
// }
pub(crate) fn revert_to_manifest(
    dir: &PathBuf,
    manifest: &Manifest,
    sst_id_set: HashSet<SSTableId>,
) -> anyhow::Result<()> {
    //check all files in manifest exist;
    for (id, _) in manifest.tables.iter() {
        if !sst_id_set.contains(id) {
            bail!("file does not exist for table {:?}", id);
        };
    }
    //delete files that shouldn't exist;
    for id in sst_id_set {
        match manifest.tables.get(&id) {
            Some(_) => {}
            None => {
                debug!("Table file {:?} not referenced in Manifest", id);
                let sst_path = id.join_dir(dir);
                remove_file(sst_path)
                    .map_err(|e| anyhow!("While removing table {:?} for {}", id, e))?;
            }
        }
    }
    Ok(())
}
#[test]
fn test_a() {
    't: for i in 0..4 {
        for j in 0..3 {
            if j == 2 {
                continue 't;
            }
            println!("{}-{}", i, j);
        }
    }

    // let mut v = Vec::with_capacity(2);;
    // v.insert(1, 1);
    // v[1]=1;
    // dbg!(v);
}
#[test]
fn test_b() {
    let mut map = BTreeMap::<u8, Vec<String>>::new();
    for i in 0..20 {
        map.insert(i, vec![i.to_string()]);
    }
    for k in 0..1000_000 {
        for i in 0..20 {
            if let Some(s) = map.get(&i) {
                let mut count = 0;
                for ele in s {
                    count += 1;
                }
            };
        }
    }
}
#[test]
fn test_c() {
    let mut map = Vec::with_capacity(20);
    for i in 0..20 {
        map.insert(i, vec![i.to_string()]);
    }
    for k in 0..1000_000 {
        for i in 0..20 {
            if let Some(s) = map.get(i as usize) {
                let mut count = 0;
                for ele in s {
                    count += 1;
                }
            };
        }
    }
}
