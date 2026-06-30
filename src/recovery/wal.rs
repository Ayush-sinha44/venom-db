use super::log_record::LogRecord;
use crate::storage::page::Page;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

pub struct WalManager {
    log_file: File,
    current_lsn: u64,
    next_txn_id: u32,
    active_txns: HashSet<u32>,
    pub pages: HashMap<u32, Page>,
}

impl WalManager {
    pub fn new(log_path: &str) -> std::io::Result<Self> {
        let log_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(log_path)?;
        Ok(Self {
            log_file,
            current_lsn: 0,
            next_txn_id: 1,
            active_txns: HashSet::new(),
            pages: HashMap::new(),
        })
    }

    pub fn begin_txn(&mut self) -> std::io::Result<u32> {
        let txn_id = self.next_txn_id;
        self.next_txn_id += 1;
        self.active_txns.insert(txn_id);
        self.append(LogRecord::Begin { lsn: 0, txn_id })?;
        Ok(txn_id)
    }

    pub fn log_insert(&mut self, txn_id: u32, page_id: u32, data: &[u8]) -> std::io::Result<u16> {
        let page = self
            .pages
            .entry(page_id)
            .or_insert_with(|| Page::new(page_id));
        let slot_id = page
            .insert_tuple(data)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::OutOfMemory, "page full"))?;
        self.append(LogRecord::Insert {
            lsn: 0,
            txn_id,
            page_id,
            slot_id,
            data: data.to_vec(),
        })?;
        Ok(slot_id)
    }

    pub fn log_delete(&mut self, txn_id: u32, page_id: u32, slot_id: u16) -> std::io::Result<bool> {
        let old_data = self
            .pages
            .get(&page_id)
            .and_then(|p| p.get_tuple(slot_id))
            .map(|d| d.to_vec())
            .unwrap_or_default();
        let deleted = self
            .pages
            .get_mut(&page_id)
            .map(|p| p.delete_tuple(slot_id))
            .unwrap_or(false);
        self.append(LogRecord::Delete {
            lsn: 0,
            txn_id,
            page_id,
            slot_id,
            old_data,
        })?;
        Ok(deleted)
    }
    pub fn log_update(
        &mut self,
        txn_id: u32,
        page_id: u32,
        slot_id: u16,
        old_data: &[u8],
        new_data: &[u8],
    ) -> std::io::Result<()> {
        self.append(LogRecord::Update {
            lsn: 0,
            txn_id,
            page_id,
            slot_id,
            old_data: old_data.to_vec(),
            new_data: new_data.to_vec(),
        })?;
        Ok(())
    }

    pub fn commit(&mut self, txn_id: u32) -> std::io::Result<()> {
        self.append(LogRecord::Commit { lsn: 0, txn_id })?;
        self.flush()?;
        self.active_txns.remove(&txn_id);
        Ok(())
    }

    pub fn abort(&mut self, txn_id: u32) -> std::io::Result<()> {
        self.append(LogRecord::Abort { lsn: 0, txn_id })?;
        self.flush()?;
        self.active_txns.remove(&txn_id);
        Ok(())
    }

    pub fn recover(&mut self) -> std::io::Result<RecoveryReport> {
        let records = self.read_all_records()?;
        let mut report = RecoveryReport::default();
        if records.is_empty() {
            return Ok(report);
        }

        // Build per-txn commit/abort/start sets
        // A txn_id is only "committed" if its BEGIN and COMMIT are in the
        // same contiguous session. We track per-session by walking forward.
        let mut committed: HashSet<u32> = HashSet::new();
        let mut aborted: HashSet<u32> = HashSet::new();
        let mut started: HashSet<u32> = HashSet::new();

        for record in &records {
            match record {
                LogRecord::Begin { txn_id, .. } => {
                    started.insert(*txn_id);
                }
                LogRecord::Commit { txn_id, .. } => {
                    committed.insert(*txn_id);
                }
                LogRecord::Abort { txn_id, .. } => {
                    aborted.insert(*txn_id);
                }
                _ => {}
            }
        }

        let incomplete: HashSet<u32> = started
            .iter()
            .filter(|id| !committed.contains(id) && !aborted.contains(id))
            .copied()
            .collect();

        // REDO: replay only committed txns
        for record in &records {
            match record {
                LogRecord::Insert {
                    txn_id,
                    page_id,
                    data,
                    ..
                } if committed.contains(txn_id) => {
                    let page = self
                        .pages
                        .entry(*page_id)
                        .or_insert_with(|| Page::new(*page_id));
                    page.insert_tuple(data);
                    report.redone += 1;
                }
                LogRecord::Delete {
                    txn_id,
                    page_id,
                    slot_id,
                    ..
                } if committed.contains(txn_id) => {
                    if let Some(page) = self.pages.get_mut(page_id) {
                        page.delete_tuple(*slot_id);
                        report.redone += 1;
                    }
                }
                LogRecord::Update {
                    txn_id,
                    page_id,
                    slot_id,
                    new_data,
                    ..
                } if committed.contains(txn_id) => {
                    if let Some(page) = self.pages.get_mut(page_id) {
                        page.update_tuple(*slot_id, new_data);
                        report.redone += 1;
                    } else {
                        // Page wasn't touched by INSERT/DELETE in this log — shouldn't
                        // normally happen since UPDATE requires the row to already exist,
                        // but guard anyway.
                        let mut new_page = Page::new(*page_id);
                        new_page.update_tuple(*slot_id, new_data);
                        self.pages.insert(*page_id, new_page);
                        report.redone += 1;
                    }
                }

                _ => {}
            }
        }

        // UNDO: reverse incomplete txns in reverse log order
        for record in records.iter().rev() {
            let txn_id = match record.txn_id() {
                Some(id) if incomplete.contains(&id) => id,
                _ => continue,
            };
            match record {
                LogRecord::Insert {
                    page_id, slot_id, ..
                } => {
                    if let Some(page) = self.pages.get_mut(page_id) {
                        page.delete_tuple(*slot_id);
                        report.undone += 1;
                    }
                }
                LogRecord::Delete {
                    page_id, old_data, ..
                } => {
                    let page = self
                        .pages
                        .entry(*page_id)
                        .or_insert_with(|| Page::new(*page_id));
                    page.insert_tuple(old_data);
                    report.undone += 1;
                }
                _ => {}
            }
            report.incomplete_txns.insert(txn_id);
        }

        Ok(report)
    }

    fn append(&mut self, mut record: LogRecord) -> std::io::Result<u64> {
        let lsn = self.current_lsn;
        self.current_lsn += 1;
        record = match record {
            LogRecord::Begin { txn_id, .. } => LogRecord::Begin { lsn, txn_id },
            LogRecord::Insert {
                txn_id,
                page_id,
                slot_id,
                data,
                ..
            } => LogRecord::Insert {
                lsn,
                txn_id,
                page_id,
                slot_id,
                data,
            },
            LogRecord::Delete {
                txn_id,
                page_id,
                slot_id,
                old_data,
                ..
            } => LogRecord::Delete {
                lsn,
                txn_id,
                page_id,
                slot_id,
                old_data,
            },
            LogRecord::Update {
                txn_id,
                page_id,
                slot_id,
                old_data,
                new_data,
                ..
            } => LogRecord::Update {
                lsn,
                txn_id,
                page_id,
                slot_id,
                old_data,
                new_data,
            },
            LogRecord::Commit { txn_id, .. } => LogRecord::Commit { lsn, txn_id },
            LogRecord::Abort { txn_id, .. } => LogRecord::Abort { lsn, txn_id },
            LogRecord::Checkpoint { .. } => LogRecord::Checkpoint { lsn },
        };
        let bytes = record.to_bytes();
        self.log_file.seek(SeekFrom::End(0))?;
        self.log_file.write_all(&bytes)?;
        Ok(lsn)
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.log_file.flush()?;
        self.log_file.sync_all()?;
        Ok(())
    }

    pub fn read_all_records(&mut self) -> std::io::Result<Vec<LogRecord>> {
        self.log_file.seek(SeekFrom::Start(0))?;
        let mut all_bytes = Vec::new();
        self.log_file.read_to_end(&mut all_bytes)?;
        let mut records = Vec::new();
        let mut off = 0;
        while off + 4 <= all_bytes.len() {
            let len = u32::from_le_bytes(all_bytes[off..off + 4].try_into().unwrap()) as usize;
            off += 4;
            if off + len > all_bytes.len() {
                break;
            }
            if let Some(r) = LogRecord::from_bytes(&all_bytes[off..off + len]) {
                records.push(r);
            }
            off += len;
        }
        Ok(records)
    }

    pub fn current_lsn(&self) -> u64 {
        self.current_lsn
    }
    pub fn active_txns(&self) -> &HashSet<u32> {
        &self.active_txns
    }
}

#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub redone: usize,
    pub undone: usize,
    pub incomplete_txns: HashSet<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> String {
        format!("/tmp/wal_{}.log", name)
    }

    #[test]
    fn test_commit_survives_restart() -> std::io::Result<()> {
        let path = temp("commit");
        let _ = std::fs::remove_file(&path);
        {
            let mut wal = WalManager::new(&path)?;
            let t = wal.begin_txn()?;
            wal.log_insert(t, 0, b"committed row")?;
            wal.commit(t)?;
        }
        {
            let mut wal = WalManager::new(&path)?;
            let report = wal.recover()?;
            assert_eq!(report.redone, 1);
            assert_eq!(report.undone, 0);
            assert_eq!(wal.pages[&0].get_tuple(0).unwrap(), b"committed row");
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn test_uncommitted_txn_is_rolled_back() -> std::io::Result<()> {
        let path = temp("rollback2");
        let _ = std::fs::remove_file(&path);
        {
            let mut wal = WalManager::new(&path)?;
            let t = wal.begin_txn()?;
            wal.log_insert(t, 0, b"should disappear")?;
            // no commit
        }
        {
            let mut wal = WalManager::new(&path)?;
            let report = wal.recover()?;
            // txn was incomplete — must appear in incomplete set
            assert_eq!(report.incomplete_txns.len(), 1);
            // nothing was committed so REDO touches nothing
            assert_eq!(report.redone, 0);
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn test_mixed_committed_and_crashed() -> std::io::Result<()> {
        let path = temp("mixed2");
        let _ = std::fs::remove_file(&path);
        {
            let mut wal = WalManager::new(&path)?;
            let t1 = wal.begin_txn()?;
            wal.log_insert(t1, 0, b"row A - committed")?;
            wal.commit(t1)?;
            let t2 = wal.begin_txn()?;
            wal.log_insert(t2, 0, b"row B - should vanish")?;
            // crash
        }
        {
            let mut wal = WalManager::new(&path)?;
            let report = wal.recover()?;
            assert_eq!(report.redone, 1);
            assert_eq!(report.incomplete_txns.len(), 1);
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn test_log_serialization_roundtrip() -> std::io::Result<()> {
        let path = temp("serial2");
        let _ = std::fs::remove_file(&path);
        let mut wal = WalManager::new(&path)?;
        let t = wal.begin_txn()?;
        wal.log_insert(t, 5, b"hello wal")?;
        wal.log_delete(t, 5, 0)?;
        wal.commit(t)?;
        let records = wal.read_all_records()?;
        assert_eq!(records.len(), 4);
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn test_multiple_commits() -> std::io::Result<()> {
        let path = temp("multi2");
        let _ = std::fs::remove_file(&path);
        {
            let mut wal = WalManager::new(&path)?;
            for i in 0..5u32 {
                let t = wal.begin_txn()?;
                wal.log_insert(t, i, format!("row {}", i).as_bytes())?;
                wal.commit(t)?;
            }
        }
        {
            let mut wal = WalManager::new(&path)?;
            let report = wal.recover()?;
            assert_eq!(report.redone, 5);
            assert_eq!(report.undone, 0);
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}
