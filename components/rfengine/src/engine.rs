// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    collections::{hash_map::Entry, BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    num::ParseIntError,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{mpsc::SyncSender, Arc, Mutex, RwLock},
    thread::{self, JoinHandle},
    time::Instant,
};

use byteorder::{ByteOrder, LittleEndian};
use bytes::{Buf, BufMut, Bytes};
use dashmap::{mapref::one::Ref, DashMap};
use raft_proto::eraftpb;
use slog_global::info;
use thiserror::Error as ThisError;

use crate::{
    log_batch::{RaftLogBlock, RaftLogOp, RaftLogs},
    metrics::*,
    *,
};

#[derive(Clone)]
pub struct RfEngine {
    pub dir: PathBuf,
    pub(crate) writer: Arc<Mutex<WALWriter>>,

    pub(crate) regions: Arc<dashmap::DashMap<u64, RwLock<RegionData>>>,

    pub(crate) task_sender: SyncSender<Task>,
    pub(crate) worker_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

#[derive(Default)]
pub struct WriteBatch {
    pub(crate) regions: HashMap<u64, RegionBatch>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self {
            regions: Default::default(),
        }
    }

    pub(crate) fn get_region(&mut self, region_id: u64) -> &mut RegionBatch {
        match self.regions.entry(region_id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(RegionBatch::new(region_id)),
        }
    }

    pub fn append_raft_log(&mut self, region_id: u64, entry: &eraftpb::Entry) {
        let op = RaftLogOp::new(entry);
        self.get_region(region_id).append_raft_log(op);
    }

    pub fn truncate_raft_log(&mut self, region_id: u64, index: u64) {
        self.get_region(region_id).truncate(index);
    }

    pub fn set_state(&mut self, region_id: u64, key: &[u8], val: &[u8]) {
        self.get_region(region_id).set_state(key, val);
    }

    pub fn reset(&mut self) {
        self.regions.clear()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub(crate) fn merge_region(&mut self, region_batch: RegionBatch) {
        self.get_region(region_batch.region_id).merge(region_batch);
    }
}

pub(crate) struct RegionBatch {
    pub(crate) region_id: u64,
    pub(crate) truncated_idx: u64,
    pub(crate) states: BTreeMap<Bytes, Bytes>,
    pub(crate) raft_logs: VecDeque<RaftLogOp>,
}

impl RegionBatch {
    pub(crate) fn new(region_id: u64) -> Self {
        Self {
            region_id,
            truncated_idx: 0,
            states: Default::default(),
            raft_logs: Default::default(),
        }
    }

    pub(crate) fn truncate(&mut self, idx: u64) {
        while let Some(front) = self.raft_logs.front() {
            if front.index >= idx {
                break;
            }
            self.raft_logs.pop_front();
        }
        self.truncated_idx = idx;
    }

    pub fn set_state(&mut self, key: &[u8], val: &[u8]) {
        self.states
            .insert(Bytes::copy_from_slice(key), Bytes::copy_from_slice(val));
    }

    pub fn append_raft_log(&mut self, op: RaftLogOp) {
        while let Some(back) = self.raft_logs.back() {
            if back.index + 1 == op.index {
                break;
            }
            self.raft_logs.pop_back();
        }
        self.raft_logs.push_back(op);
    }

    pub fn merge(&mut self, other: RegionBatch) {
        for (k, v) in other.states {
            self.states.insert(k, v);
        }
        for op in other.raft_logs {
            self.append_raft_log(op);
        }
        if self.truncated_idx < other.truncated_idx {
            self.truncated_idx = other.truncated_idx;
        }
    }

    pub(crate) fn encoded_len(&self) -> usize {
        // region_id, start_index, end_index, truncated_idx, states_len.
        let mut len = 8 + 8 + 8 + 8 + 4;
        for (key, val) in &self.states {
            len += 2 + key.len() + 4 + val.len();
        }
        len += self.raft_logs.len() * 4;
        for op in &self.raft_logs {
            len += op.encoded_len();
        }
        len
    }

    pub(crate) fn encode_to(&self, buf: &mut impl BufMut) {
        buf.put_u64_le(self.region_id);
        let first = self.raft_logs.front().map_or(0, |x| x.index);
        let end = self.raft_logs.back().map_or(0, |x| x.index + 1);
        buf.put_u64_le(first);
        buf.put_u64_le(end);
        buf.put_u64_le(self.truncated_idx);
        buf.put_u32_le(self.states.len() as u32);
        for (key, val) in &self.states {
            buf.put_u16_le(key.len() as u16);
            buf.put_slice(key.chunk());
            buf.put_u32_le(val.len() as u32);
            buf.put_slice(val.chunk());
        }
        let mut end_off = 0;
        for op in &self.raft_logs {
            end_off += op.encoded_len() as u32;
            buf.put_u32_le(end_off);
        }
        for op in &self.raft_logs {
            op.encode_to(buf);
        }
    }

    pub(crate) fn decode(mut buf: &[u8]) -> Self {
        let mut batch = RegionBatch::new(0);
        batch.region_id = LittleEndian::read_u64(buf);
        buf = &buf[8..];
        let first = LittleEndian::read_u64(buf);
        buf = &buf[8..];
        let end = LittleEndian::read_u64(buf);
        buf = &buf[8..];
        batch.truncated_idx = LittleEndian::read_u64(buf);
        buf = &buf[8..];
        let states_len = LittleEndian::read_u32(buf);
        buf = &buf[4..];
        for _ in 0..states_len {
            let key_len = LittleEndian::read_u16(buf) as usize;
            buf = &buf[2..];
            let key = Bytes::copy_from_slice(&buf[..key_len]);
            buf = &buf[key_len..];
            let val_len = LittleEndian::read_u32(buf) as usize;
            buf = &buf[4..];
            let val = Bytes::copy_from_slice(&buf[..val_len]);
            buf = &buf[val_len..];
            batch.states.insert(key, val);
        }
        let num_logs = (end - first) as usize;
        let log_index_len = num_logs * 4;
        let mut log_index_buf = &buf[..log_index_len];
        buf = &buf[log_index_len..];
        let mut start_index = 0;
        for _ in 0..num_logs {
            let end_index = LittleEndian::read_u32(log_index_buf) as usize;
            log_index_buf = &log_index_buf[4..];
            let log_op = RaftLogOp::decode(&buf[start_index..end_index]);
            start_index = end_index;
            batch.raft_logs.push_back(log_op);
        }
        batch
    }
}

impl RfEngine {
    pub fn open(dir: &Path, wal_size: usize) -> Result<Self> {
        maybe_create_dir(dir)?;
        let mut epoches = read_epoches(dir)?;
        let mut epoch_id = 1;
        if !epoches.is_empty() {
            epoch_id = epoches.last().unwrap().id;
        }

        let (tx, rx) = std::sync::mpsc::sync_channel(1024);
        let writer = WALWriter::new(dir, epoch_id, wal_size)?;
        let mut en = Self {
            dir: dir.to_owned(),
            regions: Arc::new(DashMap::default()),
            writer: Arc::new(Mutex::new(writer)),
            task_sender: tx,
            worker_handle: Arc::new(Mutex::new(None)),
        };
        let mut offset = 0;
        let mut worker_states = HashMap::new();
        for ep in &epoches {
            offset = en.load_epoch(ep)?;
            if ep.has_state_file {
                for e in en.regions.iter() {
                    let region = e.read().unwrap();
                    worker_states.insert(region.region_id, region.states.clone());
                }
            }
        }
        en.writer.lock().unwrap().seek(offset);
        if !epoches.is_empty() {
            epoches.pop();
        }
        {
            let mut worker = Worker::new(dir.to_owned(), epoches, rx, worker_states);
            let join_handle = thread::spawn(move || worker.run());
            let mut handle_ref = en.worker_handle.lock().unwrap();
            *handle_ref = Some(join_handle);
        }
        Ok(en)
    }

    pub fn write(&self, wb: WriteBatch) -> Result<usize> {
        self.apply(&wb);
        self.persist(wb)
    }

    pub fn persist(&self, wb: WriteBatch) -> Result<usize> {
        let timer = Instant::now();
        let mut writer = self.writer.lock().unwrap();
        let epoch_id = writer.epoch_id;
        for data in wb.regions.values() {
            writer.append_region_data(data);
        }
        let size = writer.buf_size();
        let rotated = writer.flush()?;
        drop(writer);
        if rotated {
            self.task_sender.send(Task::Rotate { epoch_id }).unwrap();
        }
        ENGINE_PERSIST_DURATION_HISTOGRAM.observe(elapsed_secs(timer));
        Ok(size)
    }

    // apply applies the write batch to the memory before persist, so it can be retrieved
    // before persist.
    // It is used for async I/O that persist the write batch in another thread.
    pub fn apply(&self, wb: &WriteBatch) {
        let timer = Instant::now();
        for (region_id, batch_data) in &wb.regions {
            let region_id = *region_id;
            let region_ref = self.get_or_init_region_data(region_id);
            let mut region_data = region_ref.write().unwrap();
            let truncated = region_data.apply(batch_data);
            let truncated_index = region_data.truncated_idx;
            if !truncated.is_empty() {
                self.task_sender
                    .send(Task::Truncate {
                        region_id,
                        truncated_index,
                        truncated,
                    })
                    .unwrap();
            }
        }
        ENGINE_APPLY_DURATION_HISTOGRAM.observe(elapsed_secs(timer));
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn get_term(&self, region_id: u64, index: u64) -> Option<u64> {
        let map_ref = self.regions.get(&region_id);
        map_ref.as_ref()?;
        let map_ref = map_ref.unwrap();
        let region_data = map_ref.read().unwrap();
        region_data.term(index)
    }

    pub fn get_last_index(&self, region_id: u64) -> Option<u64> {
        let map_ref = self.regions.get(&region_id);
        map_ref.as_ref()?;
        let map_ref = map_ref.unwrap();
        let region_data = map_ref.read().unwrap();
        let last = region_data.raft_logs.last_index();
        if last == 0 {
            return None;
        }
        Some(last)
    }

    pub fn get_state(&self, region_id: u64, key: &[u8]) -> Option<Bytes> {
        let map_ref = self.regions.get(&region_id);
        map_ref.as_ref()?;
        let map_ref = map_ref.unwrap();
        let region_data = map_ref.read().unwrap();
        match region_data.get_state(key) {
            Some(val) => {
                if !val.is_empty() {
                    Some(val.clone())
                } else {
                    None
                }
            }
            None => None,
        }
    }

    pub fn get_last_state_with_prefix(&self, region_id: u64, prefix: &[u8]) -> Option<Bytes> {
        let map_ref = self.regions.get(&region_id);
        map_ref.as_ref()?;
        let map_ref = map_ref.unwrap();
        let region_data = map_ref.read().unwrap();
        let mut end_prefix = Vec::from(prefix);
        end_prefix[prefix.len() - 1] += 1;
        let r = Bytes::copy_from_slice(prefix)..Bytes::copy_from_slice(&end_prefix);
        let states = &region_data.states;
        let range = states.range(r);
        for (k, v) in range.rev() {
            if k.chunk() == end_prefix.as_slice() {
                continue;
            }
            return Some(v.clone());
        }
        None
    }

    pub(crate) fn get_or_init_region_data(
        &self,
        region_id: u64,
    ) -> Ref<'_, u64, RwLock<RegionData>> {
        match self.regions.get(&region_id) {
            Some(region_data) => {
                return region_data;
            }
            None => {}
        }
        let region_data = RwLock::new(RegionData::new(region_id));
        self.regions.insert(region_id, region_data);
        self.regions.get(&region_id).unwrap()
    }

    pub fn iterate_region_states<F>(&self, region_id: u64, desc: bool, mut f: F) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> Result<()>,
    {
        let map_ref = self.regions.get(&region_id);
        if map_ref.is_none() {
            return Ok(());
        }
        let map_ref = map_ref.unwrap();
        let region_data = map_ref.read().unwrap();
        let states = &region_data.states;
        if desc {
            for (k, v) in states.iter().rev() {
                f(k.chunk(), v.chunk())?;
            }
        } else {
            for (k, v) in states.iter() {
                f(k.chunk(), v.chunk())?;
            }
        }
        Ok(())
    }

    /// f returns false to stop iterating.
    pub fn iterate_all_states<F>(&self, desc: bool, mut f: F)
    where
        F: FnMut(u64, &[u8], &[u8]) -> bool,
    {
        for region_ref in self.regions.iter() {
            let region_data = region_ref.read().unwrap();
            let states = &region_data.states;
            let iter = states.iter();
            if desc {
                for (k, v) in iter.rev() {
                    if !f(region_data.region_id, k.chunk(), v.chunk()) {
                        break;
                    }
                }
            } else {
                for (k, v) in iter {
                    if !f(region_data.region_id, k.chunk(), v.chunk()) {
                        break;
                    }
                }
            }
        }
    }

    pub fn stop_worker(&mut self) {
        self.task_sender.send(Task::Close).unwrap();
        let mut handle_ref = self.worker_handle.lock().unwrap();
        if let Some(h) = handle_ref.take() {
            h.join().unwrap();
        }
    }

    /// After split and before the new region is initially flushed, the old region's raft log
    /// can not be truncated, otherwise, it would not be able to recover the new region.
    /// So we can call add_dependent after split to protect the raft log.
    /// After the new region is initially flushed or re-ingested or destroyed, call
    /// remove_dependent to resume truncate the raft log.
    pub fn add_dependent(&self, region_id: u64, dependent_id: u64) {
        let map_ref = self.regions.get(&region_id);
        if map_ref.is_none() {
            return;
        }
        let map_ref = map_ref.unwrap();
        let mut region_data = map_ref.write().unwrap();
        region_data.dependents.insert(dependent_id);
        let dependents_len = region_data.dependents.len();
        drop(region_data);
        info!(
            "region {} add dependent {}, dependents_len {}",
            region_id, dependent_id, dependents_len
        );
    }

    pub fn remove_dependent(&self, region_id: u64, dependent_id: u64) {
        let map_ref = self.regions.get(&region_id);
        if map_ref.is_none() {
            return;
        }
        let map_ref = map_ref.unwrap();
        let mut region_data = map_ref.write().unwrap();
        region_data.dependents.remove(&dependent_id);
        let dependents_len = region_data.dependents.len();
        info!(
            "region {} remove dependent {}, dependents_len {}",
            region_id, dependent_id, dependents_len
        );
    }

    pub fn get_engine_stats(&self) -> EngineStats {
        let mut total_mem_size = 0;
        let mut total_mem_entries = 0;
        let mut regions_stats = vec![];
        for region in self.regions.iter() {
            let region_stats = region.read().unwrap().get_stats();
            total_mem_size += region_stats.size;
            total_mem_entries += region_stats.num_logs;
            regions_stats.push(region_stats);
        }
        regions_stats.sort_by(|a, b| (b.size).cmp(&a.size));
        regions_stats.truncate(10);
        let mut disk_size = 0;
        let mut num_files = 0;
        if let Ok(read_dir) = self.dir.read_dir() {
            for e in read_dir.flatten() {
                if let Ok(m) = e.metadata() {
                    num_files += 1;
                    disk_size += m.size();
                }
            }
        }
        EngineStats {
            total_mem_size,
            total_mem_entries,
            disk_size,
            num_files,
            top_10_size_regions: regions_stats,
        }
    }

    pub fn get_region_stats(&self, region_id: u64) -> RegionStats {
        if let Some(mref) = self.regions.get(&region_id) {
            mref.read().unwrap().get_stats()
        } else {
            RegionStats::default()
        }
    }
}

pub(crate) fn maybe_create_dir(dir: &Path) -> Result<()> {
    let recycle_path = dir.join(RECYCLE_DIR);
    if !recycle_path.exists() {
        fs::create_dir_all(recycle_path)?;
    } else if !recycle_path.is_dir() {
        return Err(Error::Open(String::from("recycle path is not dir")));
    }
    Ok(())
}

/// `RegionData` contains region data and state in memory.
#[derive(Clone, Default)]
pub(crate) struct RegionData {
    pub(crate) region_id: u64,
    /// `truncated_idx` is the max index that doesn't exists.
    /// After truncating, the first index would be truncated_idx + 1.
    pub(crate) truncated_idx: u64,
    pub(crate) raft_logs: RaftLogs,
    pub(crate) states: BTreeMap<Bytes, Bytes>,
    pub(crate) dependents: HashSet<u64>,
}

impl RegionData {
    pub(crate) fn new(region_id: u64) -> Self {
        Self {
            region_id,
            ..Default::default()
        }
    }

    pub(crate) fn get(&self, index: u64) -> Option<eraftpb::Entry> {
        self.raft_logs.get(index)
    }

    pub(crate) fn term(&self, index: u64) -> Option<u64> {
        self.raft_logs.get(index).map(|e| e.term)
    }

    pub(crate) fn get_state(&self, key: &[u8]) -> Option<&Bytes> {
        self.states.get(key)
    }

    pub(crate) fn apply(&mut self, batch: &RegionBatch) -> Vec<RaftLogBlock> {
        debug_assert_eq!(self.region_id, batch.region_id);
        let mut truncated_blocks = vec![];
        if self.truncated_idx < batch.truncated_idx {
            self.truncated_idx = batch.truncated_idx;
        }
        if self.need_truncate() {
            truncated_blocks = self.raft_logs.truncate(self.truncated_idx);
        }
        for (key, val) in &batch.states {
            if val.is_empty() {
                self.states.remove(key.chunk());
            } else {
                self.states.insert(key.clone(), val.clone());
            }
        }
        for op in &batch.raft_logs {
            let truncated = self.raft_logs.append(op.clone());
            if !truncated.is_empty() {
                truncated_blocks.extend(truncated);
            }
        }
        truncated_blocks
    }

    pub(crate) fn need_truncate(&self) -> bool {
        let first = self.raft_logs.first_index();
        self.dependents.is_empty() && first > 0 && self.truncated_idx >= first
    }

    pub(crate) fn get_stats(&self) -> RegionStats {
        let size = self.raft_logs.size();
        let first_idx = self.raft_logs.first_index();
        let last_idx = self.raft_logs.last_index();
        let num_logs = if last_idx != 0 {
            (last_idx - first_idx + 1) as usize
        } else {
            0
        };
        RegionStats {
            id: self.region_id,
            size,
            num_logs,
            num_states: self.states.len(),
            first_idx,
            last_idx,
            truncated_idx: self.truncated_idx,
            dep_count: self.dependents.len(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("IO error: {0:?}")]
    Io(std::io::Error),
    #[error("EOF")]
    EOF,
    #[error("parse error")]
    ParseError,
    #[error("checksum not match")]
    Checksum,
    #[error("Open error: {0}")]
    Open(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            return Error::EOF;
        }
        Error::Io(e)
    }
}

impl From<ParseIntError> for Error {
    fn from(_: ParseIntError) -> Self {
        Error::ParseError
    }
}

#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(default)]
#[serde(rename_all = "kebab-case")]
pub struct EngineStats {
    pub total_mem_size: usize,
    pub total_mem_entries: usize,
    pub num_files: usize,
    pub disk_size: u64,
    pub top_10_size_regions: Vec<RegionStats>,
}

#[derive(Default, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
#[serde(rename_all = "kebab-case")]
pub struct RegionStats {
    pub id: u64,
    pub size: usize,
    pub num_logs: usize,
    pub num_states: usize,
    pub first_idx: u64,
    pub last_idx: u64,
    pub truncated_idx: u64,
    pub dep_count: usize,
}

#[cfg(test)]
mod tests {
    use bytes::{BufMut, BytesMut};
    use eraftpb::{Entry, EntryType};
    use slog::o;

    use super::*;

    #[test]
    fn test_rfengine() {
        init_logger();
        let tmp_dir = tempfile::tempdir().unwrap();
        let wal_size = 128 * 1024_usize;
        let mut engine = RfEngine::open(tmp_dir.path(), wal_size).unwrap();
        let mut wb = WriteBatch::new();
        for region_id in 1..=10_u64 {
            let (key, val) = make_state_kv(2, 1);
            wb.set_state(region_id, key.chunk(), val.chunk());
        }
        engine.write(wb).unwrap();

        for idx in 1..=1050_u64 {
            let mut wb = WriteBatch::new();
            for region_id in 1..=10_u64 {
                if region_id == 1 && (idx > 100 && idx < 900) {
                    continue;
                }
                wb.append_raft_log(region_id, &make_log_data(idx, 128));
                let (key, val) = make_state_kv(1, idx);
                wb.set_state(region_id, key.chunk(), val.chunk());
                if idx % 100 == 0 && region_id != 1 {
                    wb.truncate_raft_log(region_id, idx - 100);
                }
            }
            engine.write(wb).unwrap();
        }
        assert_eq!(engine.regions.len(), 10);
        let mut old_entries_map = HashMap::new();
        for region_ref in engine.regions.iter() {
            let region_data = region_ref.read().unwrap();
            assert_eq!(*region_ref.key(), region_data.region_id);
            old_entries_map.insert(region_data.region_id, region_data.clone());
        }
        assert_eq!(old_entries_map.len(), 10);
        engine.stop_worker();
        for _ in 0..2 {
            let engine = RfEngine::open(tmp_dir.path(), wal_size).unwrap();
            engine.iterate_all_states(false, |region_id, key, _| {
                let old_region_data = old_entries_map.get(&region_id).unwrap();
                assert!(old_region_data.get_state(key).is_some());
                true
            });
            assert_eq!(engine.regions.len(), 10);
            for new_data_ref in engine.regions.iter() {
                let new_data = new_data_ref.read().unwrap();
                let old_data = old_entries_map.get(new_data_ref.key()).unwrap();
                assert_eq!(old_data.truncated_idx, new_data.truncated_idx);
                assert_eq!(
                    old_data.raft_logs.first_index(),
                    new_data.raft_logs.first_index()
                );
                assert_eq!(
                    old_data.raft_logs.last_index(),
                    new_data.raft_logs.last_index()
                );
                for i in new_data.raft_logs.first_index()..=new_data.raft_logs.last_index() {
                    let entry = new_data.raft_logs.get(i).unwrap();
                    assert_eq!(
                        old_data.get(entry.index).unwrap().data.chunk(),
                        entry.data.chunk()
                    );
                }
            }
        }
    }

    fn make_log_data(index: u64, size: usize) -> eraftpb::Entry {
        let mut entry = eraftpb::Entry::new();
        entry.set_entry_type(eraftpb::EntryType::EntryConfChange);
        entry.set_index(index);
        entry.set_term(1);

        let mut data = BytesMut::with_capacity(size);
        data.resize(size, 0);
        entry.set_data(data.freeze());
        entry
    }

    fn make_state_kv(key_byte: u8, idx: u64) -> (BytesMut, BytesMut) {
        let mut key = BytesMut::new();
        key.put_u8(key_byte);
        let mut val = BytesMut::new();
        val.put_u64_le(idx);
        (key, val)
    }

    fn init_logger() {
        use slog::Drain;
        let decorator = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::CompactFormat::new(decorator).build();
        let drain = std::sync::Mutex::new(drain).fuse();
        let logger = slog::Logger::root(drain, o!());
        slog_global::set_global(logger);
    }

    fn new_raft_entry(tp: EntryType, term: u64, index: u64, data: &[u8], context: u8) -> Entry {
        let mut entry = Entry::new();
        entry.set_entry_type(tp);
        entry.set_term(term);
        entry.set_index(index);
        entry.set_data(data.to_vec().into());
        entry.set_context(vec![context].into());
        entry
    }

    #[test]
    fn test_region_data() {
        let mut region_data = RegionData::new(1);

        let mut region_batch = RegionBatch::new(1);
        for i in 1..=5 {
            region_batch.append_raft_log(RaftLogOp::new(&new_raft_entry(
                EntryType::EntryNormal,
                i,
                i,
                b"data",
                0,
            )));
        }
        assert!(region_data.apply(&region_batch).is_empty());
        for i in 1..=5 {
            assert_eq!(
                region_data.get(i).unwrap(),
                region_batch.raft_logs[(i - 1) as usize].to_entry()
            );
            assert_eq!(region_data.term(i).unwrap(), i);
        }
        assert!(region_data.get(6).is_none());
        let region_stats = region_data.get_stats();
        assert_eq!(
            region_stats,
            RegionStats {
                id: 1,
                size: 20,
                num_logs: 5,
                num_states: 0,
                first_idx: 1,
                last_idx: 5,
                truncated_idx: 0,
                dep_count: 0
            }
        );

        region_batch = RegionBatch::new(1);
        region_batch.truncate(5);
        region_batch.set_state(b"k1", b"v1");
        region_batch.set_state(b"k2", b"v2");
        let truncated = region_data.apply(&region_batch);
        assert_eq!(truncated.len(), 1);
        assert_eq!(truncated[0].first_index(), 1);
        assert_eq!(truncated[0].last_index(), 5);
        for i in 1..=5 {
            assert!(region_data.get(i).is_none());
        }
        assert_eq!(region_data.get_state(b"k1"), Some(&b"v1".to_vec().into()));
        assert_eq!(region_data.get_state(b"k2"), Some(&b"v2".to_vec().into()));
        let region_stats = region_data.get_stats();
        assert_eq!(
            region_stats,
            RegionStats {
                id: 1,
                size: 0,
                num_logs: 0,
                num_states: 2,
                first_idx: 0,
                last_idx: 0,
                truncated_idx: 5,
                dep_count: 0
            }
        );

        region_batch = RegionBatch::new(1);
        region_batch.truncate(5);
        region_batch.set_state(b"k1", b"");
        assert!(region_data.apply(&region_batch).is_empty());
        assert!(region_data.get_state(b"k1").is_none());
        assert_eq!(region_data.get_state(b"k2"), Some(&b"v2".to_vec().into()));
    }
}
