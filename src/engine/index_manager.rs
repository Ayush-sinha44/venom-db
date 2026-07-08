use std::collections::HashMap;
use crate::index::btree::BTree;
use crate::index::node::Rid;

/// IndexManager manages per-column BTree indexes.
///
/// Keyed by (table_name, column_name).
/// Only INT columns are indexable (BTree keys are i64).
/// Indexes live in memory — crash recovery rebuilds them from heap pages.
pub struct IndexManager {
    indexes: HashMap<(String, String), BTree>,
}

impl IndexManager {
    pub fn new() -> Self {
        Self {
            indexes: HashMap::new(),
        }
    }

    /// Create a new BTree index for the given (table, column) pair.
    /// No-op if the index already exists.
    pub fn create_index(&mut self, table: &str, column: &str) {
        let key = (table.to_string(), column.to_string());
        if !self.indexes.contains_key(&key) {
            self.indexes.insert(key, BTree::new());
        }
    }

    /// Check if an index exists for (table, column).
    pub fn has_index(&self, table: &str, column: &str) -> bool {
        self.indexes.contains_key(&(table.to_string(), column.to_string()))
    }

    /// Insert a key→Rid mapping into the index for (table, column).
    /// No-op if no index exists on that column.
    pub fn insert(&mut self, table: &str, column: &str, key: i64, rid: Rid) {
        if let Some(tree) = self.indexes.get_mut(&(table.to_string(), column.to_string())) {
            tree.insert(key, rid);
        }
    }

    /// Delete a key from the index for (table, column).
    /// No-op if no index exists on that column.
    pub fn delete(&mut self, table: &str, column: &str, key: i64) {
        if let Some(tree) = self.indexes.get_mut(&(table.to_string(), column.to_string())) {
            tree.delete(key);
        }
    }

    /// Exact-match lookup: returns the Rid if found.
    pub fn search(&self, table: &str, column: &str, key: i64) -> Option<Rid> {
        self.indexes
            .get(&(table.to_string(), column.to_string()))
            .and_then(|tree| tree.search(key))
    }

    /// Range scan: returns all Rids where start <= key <= end.
    pub fn range_scan(&self, table: &str, column: &str, start: i64, end: i64) -> Vec<Rid> {
        self.indexes
            .get(&(table.to_string(), column.to_string()))
            .map(|tree| tree.range_scan(start, end))
            .unwrap_or_default()
    }

    /// Returns a list of all (table, column) pairs that have indexes.
    pub fn get_index_defs(&self) -> Vec<(String, String)> {
        self.indexes.keys().cloned().collect()
    }

    /// Drop all indexes (used during recovery before rebuild).
    pub fn clear(&mut self) {
        self.indexes.clear();
    }
}
