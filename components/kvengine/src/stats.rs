// Copyright 2022 TiKV Project Authors. Licensed under Apache-2.0.

use crate::{load_bool, NUM_CFS};
use bytes::Buf;

#[derive(Default, Debug, Serialize, Deserialize)]
#[serde(default)]
#[serde(rename_all = "kebab-case")]
pub struct EngineStats {
    pub num_shards: usize,
    pub num_initial_flushed_shard: usize,
    pub num_active_shards: usize,
    pub num_compacting_shards: usize,
    pub mem_tables_count: usize,
    pub mem_tables_size: u64,
    pub l0_tables_count: usize,
    pub l0_tables_size: u64,
    pub partial_l0_count: usize,
    pub partial_ln_count: usize,
    pub cfs_num_files: Vec<usize>,
    pub cf_total_sizes: Vec<u64>,
    pub level_num_files: Vec<usize>,
    pub level_total_sizes: Vec<u64>,
    pub top_10_write: Vec<ShardStats>,
}

impl EngineStats {
    pub fn new() -> Self {
        let mut stats = EngineStats::default();
        stats.cfs_num_files = vec![0; 3];
        stats.cf_total_sizes = vec![0; 3];
        stats.level_num_files = vec![0; 3];
        stats.level_total_sizes = vec![0; 3];
        stats
    }
}

impl super::Engine {
    pub fn get_all_shard_stats(&self) -> Vec<ShardStats> {
        let mut shard_stats = Vec::with_capacity(self.shards.len());
        for shard in self.shards.iter() {
            let stats = shard.get_stats();
            shard_stats.push(stats)
        }
        shard_stats
    }

    pub fn get_shard_stat(&self, region_id: u64) -> ShardStats {
        self.shards
            .get(&region_id)
            .map_or(ShardStats::default(), |shard| shard.get_stats())
    }

    pub fn get_engine_stats(mut shard_stats: Vec<ShardStats>) -> EngineStats {
        let mut engine_stats = EngineStats::new();
        engine_stats.num_shards = shard_stats.len();
        for shard in &shard_stats {
            if shard.active {
                engine_stats.num_active_shards += 1;
            }
            if shard.compacting {
                engine_stats.num_compacting_shards += 1;
            }
            if shard.flushed {
                engine_stats.num_initial_flushed_shard += 1;
            }
            engine_stats.mem_tables_count += shard.mem_table_count;
            engine_stats.mem_tables_size += shard.mem_table_size;
            engine_stats.l0_tables_count += shard.l0_table_count;
            engine_stats.l0_tables_size += shard.l0_table_size;
            engine_stats.partial_l0_count += shard.partial_l0s;
            engine_stats.partial_ln_count += shard.partial_tbls;
            for cf in 0..NUM_CFS {
                let shard_cf_stat = &shard.cfs[cf];
                for (i, level_stat) in shard_cf_stat.levels.iter().enumerate() {
                    engine_stats.level_num_files[i] += level_stat.num_tables;
                    engine_stats.cfs_num_files[cf] += level_stat.num_tables;
                    engine_stats.level_total_sizes[i] += level_stat.data_size;
                    engine_stats.cf_total_sizes[cf] += level_stat.data_size;
                }
            }
        }
        shard_stats.sort_by(|a, b| {
            let a_size = a.mem_table_size + a.l0_table_size;
            let b_size = b.mem_table_size + b.l0_table_size;
            b_size.cmp(&a_size)
        });
        shard_stats.truncate(10);
        engine_stats.top_10_write = shard_stats;
        engine_stats
    }
}

#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(default)]
#[serde(rename_all = "kebab-case")]
pub struct ShardStats {
    pub id: u64,
    pub ver: u64,
    pub start: String,
    pub end: String,
    pub active: bool,
    pub compacting: bool,
    pub flushed: bool,
    pub mem_table_count: usize,
    pub mem_table_size: u64,
    pub l0_table_count: usize,
    pub l0_table_size: u64,
    pub cfs: Vec<CFStats>,
    pub base_version: u64,
    pub max_mem_table_size: u64,
    pub meta_sequence: u64,
    pub write_sequence: u64,
    pub total_size: u64,
    pub partial_l0s: usize,
    pub partial_tbls: usize,
    pub compaction_cf: isize,
    pub compaction_level: usize,
    pub compaction_score: f64,
}

#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(default)]
#[serde(rename_all = "kebab-case")]
pub struct CFStats {
    pub levels: Vec<LevelStats>,
}

#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(default)]
#[serde(rename_all = "kebab-case")]
pub struct LevelStats {
    pub level: usize,
    pub num_tables: usize,
    pub data_size: u64,
}

impl super::Shard {
    pub fn get_stats(&self) -> ShardStats {
        let mut total_size = 0;
        let shard_mem_tbls = self.get_mem_tbls();
        let mem_table_count = shard_mem_tbls.tbls.len();
        let mut mem_table_size = 0;
        for mem_tbl in shard_mem_tbls.tbls.as_ref() {
            mem_table_size += mem_tbl.size() as u64;
        }
        total_size += mem_table_size;
        let mut partial_l0s = 0;
        let shard_l0_tbls = self.get_l0_tbls();
        let l0_table_count = shard_l0_tbls.tbls.len();
        let mut l0_table_size = 0;
        for l0_tbl in shard_l0_tbls.tbls.as_ref() {
            if self.cover_full_table(l0_tbl.smallest(), l0_tbl.biggest()) {
                l0_table_size += l0_tbl.size();
            } else {
                l0_table_size += l0_tbl.size() / 2;
                partial_l0s += 1;
            }
        }
        total_size += l0_table_size;
        let mut partial_tbls = 0;
        let mut cfs = vec![];
        for cf in 0..NUM_CFS {
            let scf = self.get_cf(cf);
            let mut cf_stat = CFStats { levels: vec![] };
            for l in scf.levels.as_ref() {
                let mut level_stats = LevelStats::default();
                level_stats.level = l.level;
                level_stats.num_tables = l.tables.len();
                for t in &l.tables {
                    if self.cover_full_table(t.smallest(), t.biggest()) {
                        level_stats.data_size += t.size();
                    } else {
                        level_stats.data_size += t.size() / 2;
                        partial_tbls += 1;
                    }
                }
                total_size += level_stats.data_size;
                cf_stat.levels.push(level_stats);
            }
            cfs.push(cf_stat);
        }
        let priority = self.compaction_priority.read().unwrap().clone();
        let compaction_cf = priority.as_ref().map_or(0, |x| x.cf);
        let compaction_level = priority.as_ref().map_or(0, |x| x.level);
        let compaction_score = priority.as_ref().map_or(0f64, |x| x.score);
        ShardStats {
            id: self.id,
            ver: self.ver,
            start: format!("{:?}", self.start.chunk()),
            end: format!("{:?}", self.end.chunk()),
            active: self.is_active(),
            compacting: load_bool(&self.compacting),
            flushed: self.get_initial_flushed(),
            mem_table_count,
            mem_table_size,
            l0_table_count,
            l0_table_size,
            cfs,
            base_version: self.base_version,
            max_mem_table_size: self.get_max_mem_table_size(),
            meta_sequence: self.get_meta_sequence(),
            write_sequence: self.get_write_sequence(),
            total_size,
            partial_l0s,
            partial_tbls,
            compaction_cf,
            compaction_level,
            compaction_score,
        }
    }
}
