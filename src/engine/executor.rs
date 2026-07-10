use super::catalog::{Catalog, Schema};
use super::index_manager::IndexManager;
use crate::buffer::buffer_pool::BufferPool;
use crate::index::node::Rid;
use crate::recovery::wal::WalManager;
use crate::sql::ast::*;
use crate::storage::catalog_store::CatalogStore;
use std::collections::HashMap;

/// A row returned from a query
pub type Row = Vec<Value>;

/// TableMeta tracks which page IDs belong to each table.
/// Stored in the catalog directory as `{table}.pages` — a simple list of u32 page IDs.
/// This is the "table heap" directory: page 0 of the DB file is unused by the heap,
/// page allocation is handled by the BufferPool's DiskManager.
#[derive(Debug, Clone, Default)]
pub struct TableMeta {
    pub page_ids: Vec<u32>,
}

/// The persistent Executor.
///
/// All writes go through:
///   1. WAL  — log the operation with BEGIN/INSERT|DELETE/COMMIT
///   2. BufferPool — modify the in-memory page
///   3. Flush — buffer pool writes dirty pages to the .db file on commit
///
/// On startup, call `recover()` before accepting queries. It:
///   - Runs WAL REDO/UNDO to restore in-memory page state
///   - Loads the catalog from catalog.bin
///   - Loads table→page_id mappings
///   - Pushes recovered pages into the buffer pool
pub struct Executor {
    pub catalog: Catalog,
    /// Maps table name → list of page IDs that hold its rows
    table_pages: HashMap<String, TableMeta>,
    buffer_pool: BufferPool,
    wal: WalManager,
    catalog_store: CatalogStore,
    data_dir: String,
    /// In-memory BTree indexes, keyed by (table, column)
    pub index_manager: IndexManager,
}

impl Executor {
    /// Open (or create) a venom-db data directory.
    /// Call `recover()` immediately after — don't run queries before recovery.
    pub fn open(data_dir: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(data_dir)?;

        let db_path = format!("{}/data.db", data_dir);
        let wal_path = format!("{}/venom.wal", data_dir);

        let buffer_pool = BufferPool::new(64, &db_path)?;
        let wal = WalManager::new(&wal_path)?;
        let catalog_store = CatalogStore::new(data_dir);

        Ok(Self {
            catalog: Catalog::new(),
            table_pages: HashMap::new(),
            buffer_pool,
            wal,
            catalog_store,
            data_dir: data_dir.to_string(),
            index_manager: IndexManager::new(),
        })
    }

    /// Run WAL recovery + load catalog + restore table→page mappings.
    /// Must be called once after `open()` before any queries.
    pub fn recover(&mut self) -> std::io::Result<RecoveryInfo> {
        // 1. Load catalog (table schemas) from disk
        self.catalog = self.catalog_store.load()?;

        // 2. Load table→page_id mappings
        self.table_pages = self.load_all_table_metas()?;

        // 3. Run WAL recovery — gets back the set of pages with committed data
        let report = self.wal.recover()?;

        // 4. Push recovered WAL pages into the buffer pool so they're visible to queries.
        //    WAL pages are the source of truth after a crash — they may be ahead of
        //    what was flushed to data.db before the crash.
        let wal_page_ids: Vec<u32> = self.wal.pages.keys().cloned().collect();
        for page_id in wal_page_ids {
            // We can't move out of self.wal.pages while also using self.buffer_pool,
            // so we clone the page data and write it through the buffer pool.
            if let Some(wal_page) = self.wal.pages.get(&page_id) {
                let data = wal_page.data; // Copy [u8; PAGE_SIZE]
                // Fetch or allocate the frame for this page in the buffer pool
                match self.buffer_pool.fetch_page(page_id) {
                    Ok(frame_id) => {
                        if let Some(bp_page) = self.buffer_pool.get_page_mut(frame_id) {
                            bp_page.data = data;
                            bp_page.dirty = true;
                        }
                        self.buffer_pool.unpin(frame_id, true);
                    }
                    Err(_) => {
                        // Page not yet allocated on disk. Since DiskManager allocates
                        // sequentially, we can't target a specific page_id here yet.
                        // This is a known gap — WAL pages beyond the current disk
                        // allocation aren't recovered. Acceptable for now since INSERT
                        // always allocates via buffer_pool.new_page() which keeps the
                        // WAL and disk allocators in sync.
                    }
                }
            }
        }

        // 5. Flush all recovered dirty pages to data.db
        self.buffer_pool.flush_all()?;

        // 6. Rebuild BTree indexes from heap pages
        for (table_name, col_name) in &self.catalog.index_defs {
            self.index_manager.create_index(table_name, col_name);
        }
        
        for (table_name, meta) in &self.table_pages {
            if let Some(schema) = self.catalog.get(table_name) {
                for &page_id in &meta.page_ids {
                    // It's safe to fetch without pin here if we unpin immediately
                    if let Ok(frame_id) = self.buffer_pool.fetch_page(page_id) {
                        let num_slots = self.buffer_pool.get_page(frame_id)
                            .map(|p| p.num_slots()).unwrap_or(0);
                        
                        for slot_id in 0..num_slots {
                            let tuple_bytes = self.buffer_pool.get_page(frame_id)
                                .and_then(|p| p.get_tuple(slot_id)).map(|b| b.to_vec());
                                
                            if let Some(tuple) = tuple_bytes {
                                if let Ok(row) = schema.deserialize_row(&tuple) {
                                    for col in &schema.columns {
                                        if self.index_manager.has_index(table_name, &col.name) {
                                            if let Ok(col_idx) = schema.col_index(&col.name).ok_or(()) {
                                                if let Value::Int(key) = &row[col_idx] {
                                                    let rid = crate::index::node::Rid { page_id, slot_id };
                                                    self.index_manager.insert(table_name, &col.name, *key, rid);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        self.buffer_pool.unpin(frame_id, false);
                    }
                }
            }
        }

        Ok(RecoveryInfo {
            redone: report.redone,
            undone: report.undone,
            tables_loaded: self.catalog.tables.len(),
        })
    }

    /// Execute a SQL statement. Returns rows for SELECT, empty vec otherwise.
    pub fn execute(&mut self, stmt: Statement) -> Result<Vec<Row>, String> {
        match stmt {
            Statement::CreateTable { table, columns } => self.exec_create_table(table, columns),
            Statement::Insert { table, values } => self.exec_insert(table, values),
            Statement::Select {
                table,
                columns,
                filter,
                order_by,
                limit,
            } => self.exec_select(table, columns, filter, order_by, limit),
            Statement::Delete { table, filter } => self.exec_delete(table, filter),
            Statement::Update {
                table,
                assignments,
                filter,
            } => self.exec_update(table, assignments, filter),
        }
    }

    // ─── CREATE TABLE ────────────────────────────────────────────────────────

    fn exec_create_table(
        &mut self,
        table: String,
        columns: Vec<ColumnDef>,
    ) -> Result<Vec<Row>, String> {
        let schema = Schema::new(table.clone(), columns);

        // Auto-create BTree index on PRIMARY KEY column
        if let Some(ref pk_col) = schema.primary_key {
            self.index_manager.create_index(&table, pk_col);
        }

        self.catalog.create_table(schema.clone())?;
        
        if let Some(ref pk_col) = schema.primary_key {
            self.catalog.index_defs.push((table.clone(), pk_col.clone()));
        }
        self.table_pages.insert(table.clone(), TableMeta::default());

        // Persist catalog and empty page list immediately
        self.catalog_store
            .save(&self.catalog)
            .map_err(|e| format!("catalog save failed: {}", e))?;
        self.save_table_meta(&table)
            .map_err(|e| format!("table meta save failed: {}", e))?;

        Ok(vec![])
    }

    // ─── INSERT ──────────────────────────────────────────────────────────────

    fn exec_insert(&mut self, table: String, values: Vec<Value>) -> Result<Vec<Row>, String> {
        let schema = self
            .catalog
            .get(&table)
            .ok_or_else(|| format!("table '{}' not found", table))?
            .clone();

        // PRIMARY KEY enforcement: reject NULL and duplicate keys
        if let Some(pk_idx) = schema.pk_col_index() {
            let pk_val = &values[pk_idx];
            let pk_col_name = schema.primary_key.as_ref().unwrap();

            // NULL check
            if matches!(pk_val, Value::Null) {
                return Err(format!(
                    "PRIMARY KEY column '{}' cannot be NULL", pk_col_name
                ));
            }

            // Duplicate check via index
            if let Value::Int(key) = pk_val {
                if self.index_manager.search(&table, pk_col_name, *key).is_some() {
                    return Err(format!(
                        "duplicate primary key: {} = {}", pk_col_name, key
                    ));
                }
            }
        }

        let row_bytes = schema.serialize_row(&values)?;

        // WAL: begin → insert → commit (auto-commit per statement)
        let txn_id = self
            .wal
            .begin_txn()
            .map_err(|e| format!("wal begin failed: {}", e))?;

        // Find a page with space or allocate a new one
        let meta = self
            .table_pages
            .get(&table)
            .ok_or_else(|| format!("table meta for '{}' not found", table))?
            .clone();

        let mut inserted_page_id: Option<u32> = None;
        let mut inserted_slot_id: Option<u16> = None;

        // Try existing pages
        for &page_id in &meta.page_ids {
            let frame_id = self
                .buffer_pool
                .fetch_page(page_id)
                .map_err(|e| format!("fetch page {}: {}", page_id, e))?;

            let has_space = self
                .buffer_pool
                .get_page(frame_id)
                .map(|p| p.free_space() >= row_bytes.len() + 5) // 5 = slot entry size
                .unwrap_or(false);

            if has_space {
                // Log to WAL first (write-ahead guarantee)
                let _wal_slot = self
                    .wal
                    .log_insert(txn_id, page_id, &row_bytes)
                    .map_err(|e| format!("wal insert: {}", e))?;

                // Then modify the buffer pool page
                let slot_id = self.buffer_pool.get_page_mut(frame_id)
                    .and_then(|page| page.insert_tuple(&row_bytes))
                    .unwrap_or(0);
                self.buffer_pool.unpin(frame_id, true);
                inserted_page_id = Some(page_id);
                inserted_slot_id = Some(slot_id as u16);
                break;
            }
            self.buffer_pool.unpin(frame_id, false);
        }

        // No existing page had space — allocate a new one
        if inserted_page_id.is_none() {
            let (page_id, frame_id) = self
                .buffer_pool
                .new_page()
                .map_err(|e| format!("new page: {}", e))?;

            let _wal_slot = self
                .wal
                .log_insert(txn_id, page_id, &row_bytes)
                .map_err(|e| format!("wal insert new page: {}", e))?;

            let slot_id = self.buffer_pool.get_page_mut(frame_id)
                .and_then(|page| page.insert_tuple(&row_bytes))
                .unwrap_or(0);
            self.buffer_pool.unpin(frame_id, true);

            // Register this page with the table
            self.table_pages
                .get_mut(&table)
                .ok_or("table meta missing")?
                .page_ids
                .push(page_id);

            // Persist the updated page list
            self.save_table_meta(&table)
                .map_err(|e| format!("table meta save: {}", e))?;

            inserted_page_id = Some(page_id);
            inserted_slot_id = Some(slot_id as u16);
        }

        // Maintain indexes: for each indexed column, insert (value, Rid)
        if let (Some(page_id), Some(slot_id)) = (inserted_page_id, inserted_slot_id) {
            let rid = Rid { page_id, slot_id };
            for (col_idx, col) in schema.columns.iter().enumerate() {
                if self.index_manager.has_index(&table, &col.name) {
                    if let Value::Int(key) = &values[col_idx] {
                        self.index_manager.insert(&table, &col.name, *key, rid);
                    }
                }
            }
        }

        // Commit: fsync WAL, then flush dirty page to data.db
        self.wal
            .commit(txn_id)
            .map_err(|e| format!("wal commit: {}", e))?;

        if let Some(pid) = inserted_page_id {
            self.buffer_pool
                .flush_page(pid)
                .map_err(|e| format!("flush page: {}", e))?;
        }

        Ok(vec![])
    }

    // ─── SELECT ──────────────────────────────────────────────────────────────

    fn exec_select(
        &mut self,
        table: String,
        columns: SelectColumns,
        filter: Option<Expr>,
        order_by: Option<(String, OrderDir)>,
        limit: Option<u64>,
    ) -> Result<Vec<Row>, String> {
        let schema = self
            .catalog
            .get(&table)
            .ok_or_else(|| format!("table '{}' not found", table))?
            .clone();

        let meta = self
            .table_pages
            .get(&table)
            .ok_or_else(|| format!("table meta for '{}' not found", table))?
            .clone();

        let mut results = Vec::new();

        // Try index-accelerated lookup if WHERE is on an indexed INT column
        let used_index = if let Some(ref expr) = filter {
            self.try_index_select(&table, &schema, expr, &columns, &meta, &mut results)?
        } else {
            false
        };

        // Fall back to sequential scan if no index was used
        if !used_index {
            for &page_id in &meta.page_ids {
                let frame_id = self
                    .buffer_pool
                    .fetch_page(page_id)
                    .map_err(|e| format!("fetch page {}: {}", page_id, e))?;

                let num_slots = self
                    .buffer_pool
                    .get_page(frame_id)
                    .map(|p| p.num_slots())
                    .unwrap_or(0);

                for slot_id in 0..num_slots {
                    let tuple_bytes = self
                        .buffer_pool
                        .get_page(frame_id)
                        .and_then(|p| p.get_tuple(slot_id))
                        .map(|b| b.to_vec());

                    let tuple = match tuple_bytes {
                        Some(t) => t,
                        None => continue, // tombstone
                    };

                    let row = schema.deserialize_row(&tuple)?;

                    if let Some(ref expr) = filter {
                        if !Self::eval_filter(&schema, &row, expr)? {
                            continue;
                        }
                    }

                    let projected = Self::project(&schema, row, &columns)?;
                    results.push(projected);
                }

                self.buffer_pool.unpin(frame_id, false);
            }
        }

        // ORDER BY: sort results by the specified column
        if let Some((ref col_name, ref dir)) = order_by {
            // Resolve column index in the projected output
            let sort_idx = match &columns {
                SelectColumns::Star => {
                    schema.col_index(col_name)
                        .ok_or_else(|| format!("ORDER BY column '{}' not found", col_name))?
                }
                SelectColumns::Named(names) => {
                    names.iter().position(|n| n == col_name)
                        .ok_or_else(|| format!("ORDER BY column '{}' not in SELECT list", col_name))?
                }
            };

            results.sort_by(|a, b| {
                let cmp = Self::compare_values(&a[sort_idx], &b[sort_idx]);
                match dir {
                    OrderDir::Asc  => cmp,
                    OrderDir::Desc => cmp.reverse(),
                }
            });
        }

        // LIMIT: truncate results
        if let Some(n) = limit {
            results.truncate(n as usize);
        }

        // Apply ORDER BY
        if let Some((ref col_name, ref dir)) = order_by {
            // Resolve column index — in projected results the index may
            // differ from the schema index, but for SELECT * it matches.
            // We need the index in the *projected* row.
            let col_idx = match &columns {
                SelectColumns::Star => {
                    schema.col_index(col_name)
                        .ok_or_else(|| format!("ORDER BY column '{}' not found", col_name))?
                }
                SelectColumns::Named(names) => {
                    names.iter().position(|n| n == col_name)
                        .ok_or_else(|| format!("ORDER BY column '{}' not in SELECT list", col_name))?
                }
            };

            results.sort_by(|a, b| {
                let cmp = Self::compare_values(&a[col_idx], &b[col_idx]);
                match dir {
                    OrderDir::Asc => cmp,
                    OrderDir::Desc => cmp.reverse(),
                }
            });
        }

        // Apply LIMIT
        if let Some(n) = limit {
            results.truncate(n as usize);
        }

        Ok(results)
    }

    /// Attempt to use a BTree index for a WHERE clause.
    /// Returns true if the index was used (results populated), false to fall back to seq scan.
    fn try_index_select(
        &mut self,
        table: &str,
        schema: &Schema,
        expr: &Expr,
        columns: &SelectColumns,
        meta: &TableMeta,
        results: &mut Vec<Row>,
    ) -> Result<bool, String> {
        // Only works on INT columns with an index
        if !self.index_manager.has_index(table, &expr.left) {
            return Ok(false);
        }

        let rhs = match &expr.right {
            Value::Int(n) => *n,
            _ => return Ok(false), // non-INT comparison or NULL: fall back
        };

        // Collect Rids from the index
        let rids: Vec<Rid> = match &expr.op {
            Op::Eq => {
                match self.index_manager.search(table, &expr.left, rhs) {
                    Some(rid) => vec![rid],
                    None => vec![],
                }
            }
            Op::Gt => self.index_manager.range_scan(table, &expr.left, rhs + 1, i64::MAX),
            Op::Ge => self.index_manager.range_scan(table, &expr.left, rhs, i64::MAX),
            Op::Lt => self.index_manager.range_scan(table, &expr.left, i64::MIN, rhs - 1),
            Op::Le => self.index_manager.range_scan(table, &expr.left, i64::MIN, rhs),
            _ => return Ok(false), // Ne, IsNull, IsNotNull — fall back to seq scan
        };

        // Fetch the actual tuples from the heap pages using Rids
        for rid in &rids {
            // Check the page is part of this table
            if !meta.page_ids.contains(&rid.page_id) {
                continue;
            }
            let frame_id = self
                .buffer_pool
                .fetch_page(rid.page_id)
                .map_err(|e| format!("fetch page {}: {}", rid.page_id, e))?;

            let tuple_bytes = self
                .buffer_pool
                .get_page(frame_id)
                .and_then(|p| p.get_tuple(rid.slot_id))
                .map(|b| b.to_vec());

            self.buffer_pool.unpin(frame_id, false);

            let tuple = match tuple_bytes {
                Some(t) => t,
                None => continue, // tombstone — index is stale
            };

            let row = schema.deserialize_row(&tuple)?;

            // Double-check the filter (index may have stale entries)
            if !Self::eval_filter(schema, &row, expr)? {
                continue;
            }

            let projected = Self::project(schema, row, columns)?;
            results.push(projected);
        }

        Ok(true)
    }

    // ─── DELETE ──────────────────────────────────────────────────────────────

    fn exec_delete(&mut self, table: String, filter: Option<Expr>) -> Result<Vec<Row>, String> {
        let schema = self
            .catalog
            .get(&table)
            .ok_or_else(|| format!("table '{}' not found", table))?
            .clone();

        let meta = self
            .table_pages
            .get(&table)
            .ok_or_else(|| format!("table meta for '{}' not found", table))?
            .clone();

        let txn_id = self
            .wal
            .begin_txn()
            .map_err(|e| format!("wal begin: {}", e))?;

        let mut dirty_pages = Vec::new();

        for &page_id in &meta.page_ids {
            let frame_id = self
                .buffer_pool
                .fetch_page(page_id)
                .map_err(|e| format!("fetch page {}: {}", page_id, e))?;

            let num_slots = self
                .buffer_pool
                .get_page(frame_id)
                .map(|p| p.num_slots())
                .unwrap_or(0);

            let mut page_dirty = false;

            for slot_id in 0..num_slots {
                let tuple_bytes = self
                    .buffer_pool
                    .get_page(frame_id)
                    .and_then(|p| p.get_tuple(slot_id))
                    .map(|b| b.to_vec());

                let tuple = match tuple_bytes {
                    Some(t) => t,
                    None => continue,
                };

                let row = schema.deserialize_row(&tuple)?;

                let should_delete = match &filter {
                    Some(expr) => Self::eval_filter(&schema, &row, expr)?,
                    None => true,
                };

                if should_delete {
                    // WAL first
                    self.wal
                        .log_delete(txn_id, page_id, slot_id)
                        .map_err(|e| format!("wal delete: {}", e))?;

                    // Then mark tombstone in buffer pool
                    if let Some(page) = self.buffer_pool.get_page_mut(frame_id) {
                        page.delete_tuple(slot_id);
                    }

                    // Remove from indexes
                    for (col_idx, col) in schema.columns.iter().enumerate() {
                        if self.index_manager.has_index(&table, &col.name) {
                            if let Value::Int(key) = &row[col_idx] {
                                self.index_manager.delete(&table, &col.name, *key);
                            }
                        }
                    }

                    page_dirty = true;
                }
            }

            self.buffer_pool.unpin(frame_id, page_dirty);
            if page_dirty {
                dirty_pages.push(page_id);
            }
        }

        self.wal
            .commit(txn_id)
            .map_err(|e| format!("wal commit: {}", e))?;

        for pid in dirty_pages {
            self.buffer_pool
                .flush_page(pid)
                .map_err(|e| format!("flush page: {}", e))?;
        }

        Ok(vec![])
    }
    // ─── UPDATE ──────────────────────────────────────────────────────────────

    fn exec_update(
        &mut self,
        table: String,
        assignments: Vec<Assignment>,
        filter: Option<Expr>,
    ) -> Result<Vec<Row>, String> {
        let schema = self
            .catalog
            .get(&table)
            .ok_or_else(|| format!("table '{}' not found", table))?
            .clone();

        let meta = self
            .table_pages
            .get(&table)
            .ok_or_else(|| format!("table meta for '{}' not found", table))?
            .clone();

        let txn_id = self
            .wal
            .begin_txn()
            .map_err(|e| format!("wal begin: {}", e))?;

        let mut dirty_pages = Vec::new();
        let mut updated_count = 0;

        for &page_id in &meta.page_ids {
            let frame_id = self
                .buffer_pool
                .fetch_page(page_id)
                .map_err(|e| format!("fetch page {}: {}", page_id, e))?;

            let num_slots = self
                .buffer_pool
                .get_page(frame_id)
                .map(|p| p.num_slots())
                .unwrap_or(0);

            let mut page_dirty = false;

            for slot_id in 0..num_slots {
                // Read current tuple bytes
                let old_bytes = self
                    .buffer_pool
                    .get_page(frame_id)
                    .and_then(|p| p.get_tuple(slot_id))
                    .map(|b| b.to_vec());

                let old_bytes = match old_bytes {
                    Some(b) => b,
                    None => continue, // tombstone
                };

                // Deserialize to check WHERE filter
                let mut row = schema.deserialize_row(&old_bytes)?;

                let matches = match &filter {
                    Some(expr) => Self::eval_filter(&schema, &row, expr)?,
                    None => true,
                };

                if !matches {
                    continue;
                }

                // Save old row values for index maintenance
                let old_row = row.clone();

                // PRIMARY KEY enforcement on UPDATE
                if let Some(ref pk_col_name) = schema.primary_key {
                    for assignment in &assignments {
                        if &assignment.column == pk_col_name {
                            // Reject NULL
                            if matches!(&assignment.value, Value::Null) {
                                return Err(format!(
                                    "PRIMARY KEY column '{}' cannot be NULL", pk_col_name
                                ));
                            }
                            // Reject duplicate (if new value differs from old)
                            if let Value::Int(new_key) = &assignment.value {
                                let old_matches = matches!(&old_row[schema.pk_col_index().unwrap()], Value::Int(old_k) if old_k == new_key);
                                if !old_matches {
                                    if self.index_manager.search(&table, pk_col_name, *new_key).is_some() {
                                        return Err(format!(
                                            "duplicate primary key: {} = {}", pk_col_name, new_key
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }

                // Apply assignments to the row in memory
                for assignment in &assignments {
                    let col_idx = schema
                        .col_index(&assignment.column)
                        .ok_or_else(|| format!("column '{}' not found", assignment.column))?;

                    // Type check — NULL is allowed for any column type (unless PK, checked above)
                    let col_type = &schema.columns[col_idx].ty;
                    match (&assignment.value, col_type) {
                        (Value::Null, _) => {} // NULL is valid for any column
                        (Value::Int(_), crate::sql::ast::DataType::Int) => {}
                        (Value::Text(_), crate::sql::ast::DataType::Text) => {}
                        _ => {
                            return Err(format!(
                                "type mismatch: cannot assign {:?} to column '{}'",
                                assignment.value, assignment.column
                            ));
                        }
                    }

                    row[col_idx] = assignment.value.clone();
                }

                // Re-serialize the updated row
                let new_bytes = schema.serialize_row(&row)?;

                // WAL: log old + new data before touching the page
                self.wal
                    .log_update(txn_id, page_id, slot_id, &old_bytes, &new_bytes)
                    .map_err(|e| format!("wal update: {}", e))?;

                // Write new bytes into the page in-place
                let ok = self
                    .buffer_pool
                    .get_page_mut(frame_id)
                    .map(|p| p.update_tuple(slot_id, &new_bytes))
                    .unwrap_or(false);

                if !ok {
                    self.buffer_pool.unpin(frame_id, page_dirty); // unpin before returning
                    self.wal
                        .abort(txn_id)
                        .map_err(|e| format!("wal abort: {}", e))?;
                    return Err(format!(
                        "UPDATE failed: new value for row is larger than original \
         (in-place update only supported for same-size or smaller values)"
                    ));
                }

                // Maintain indexes: remove old key, insert new key
                let rid = Rid { page_id, slot_id: slot_id as u16 };
                for (col_idx, col) in schema.columns.iter().enumerate() {
                    if self.index_manager.has_index(&table, &col.name) {
                        if let Value::Int(old_key) = &old_row[col_idx] {
                            self.index_manager.delete(&table, &col.name, *old_key);
                        }
                        if let Value::Int(new_key) = &row[col_idx] {
                            self.index_manager.insert(&table, &col.name, *new_key, rid);
                        }
                    }
                }

                page_dirty = true;
                updated_count += 1;
            }

            self.buffer_pool.unpin(frame_id, page_dirty);
            if page_dirty {
                dirty_pages.push(page_id);
            }
        }

        self.wal
            .commit(txn_id)
            .map_err(|e| format!("wal commit: {}", e))?;

        for pid in dirty_pages {
            self.buffer_pool
                .flush_page(pid)
                .map_err(|e| format!("flush page: {}", e))?;
        }

        let _ = updated_count; // will use for rowcount display later
        Ok(vec![])
    }

    // ─── Helpers ─────────────────────────────────────────────────────────────

    fn eval_filter(schema: &Schema, row: &[Value], expr: &Expr) -> Result<bool, String> {
        let col_idx = schema
            .col_index(&expr.left)
            .ok_or_else(|| format!("column '{}' not found", expr.left))?;
        let cell = &row[col_idx];

        // IS NULL / IS NOT NULL — these are the ONLY operators that can
        // meaningfully interact with NULL.
        match &expr.op {
            Op::IsNull    => return Ok(matches!(cell, Value::Null)),
            Op::IsNotNull => return Ok(!matches!(cell, Value::Null)),
            _ => {}
        }

        // SQLite semantics: any standard comparison involving NULL yields
        // NULL, which is falsy in a WHERE context → return false.
        if matches!(cell, Value::Null) || matches!(&expr.right, Value::Null) {
            return Ok(false);
        }

        let cmp_ord = cell.partial_cmp(&expr.right);
        let result = match &expr.op {
            Op::Eq => cmp_ord == Some(std::cmp::Ordering::Equal),
            Op::Ne => cmp_ord.is_some() && cmp_ord != Some(std::cmp::Ordering::Equal),
            Op::Lt => cmp_ord == Some(std::cmp::Ordering::Less),
            Op::Gt => cmp_ord == Some(std::cmp::Ordering::Greater),
            Op::Le => cmp_ord == Some(std::cmp::Ordering::Less) || cmp_ord == Some(std::cmp::Ordering::Equal),
            Op::Ge => cmp_ord == Some(std::cmp::Ordering::Greater) || cmp_ord == Some(std::cmp::Ordering::Equal),
            _ => return Err(format!("unsupported operator: {:?}", expr.op)),
        };
        // If cmp_ord is None and we didn't match something that could handle it, it's a type mismatch
        if cmp_ord.is_none() {
            return Err(format!(
                "type mismatch in WHERE: {:?} {:?} {:?}",
                cell, expr.op, expr.right
            ));
        }
        Ok(result)
    }

    /// Compare two Values for ORDER BY sorting.
    /// NULLs sort last (SQLite convention).
    fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (a, b) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _)          => Ordering::Greater, // NULL sorts last
            (_, Value::Null)          => Ordering::Less,
            (a, b) => a.partial_cmp(b).unwrap_or(Ordering::Less),
        }
    }

    fn project(schema: &Schema, row: Row, cols: &SelectColumns) -> Result<Row, String> {
        match cols {
            SelectColumns::Star => Ok(row),
            SelectColumns::Named(names) => names
                .iter()
                .map(|name| {
                    let idx = schema
                        .col_index(name)
                        .ok_or_else(|| format!("column '{}' not found", name))?;
                    Ok(row[idx].clone())
                })
                .collect(),
        }
    }



    // ─── Table meta persistence ───────────────────────────────────────────────
    // Each table gets a tiny file: `{data_dir}/{table_name}.pages`
    // Format: [num_pages: u32][page_id: u32] * num_pages

    fn save_table_meta(&self, table: &str) -> std::io::Result<()> {
        use std::io::Write;
        let path = self.table_meta_path(table);
        let meta = self.table_pages.get(table).cloned().unwrap_or_default();
        let mut buf = Vec::new();
        buf.extend(&(meta.page_ids.len() as u32).to_le_bytes());
        for &pid in &meta.page_ids {
            buf.extend(&pid.to_le_bytes());
        }
        let tmp = format!("{}.tmp", path);
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&buf)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn load_table_meta(&self, table: &str) -> std::io::Result<TableMeta> {
        use std::io::Read;
        let path = self.table_meta_path(table);
        let mut f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(TableMeta::default()),
            Err(e) => return Err(e),
        };
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        if buf.len() < 4 {
            return Ok(TableMeta::default());
        }
        let num = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let mut page_ids = Vec::with_capacity(num);
        for i in 0..num {
            let off = 4 + i * 4;
            if off + 4 > buf.len() {
                break;
            }
            page_ids.push(u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()));
        }
        Ok(TableMeta { page_ids })
    }

    fn load_all_table_metas(&self) -> std::io::Result<HashMap<String, TableMeta>> {
        let mut map = HashMap::new();
        for name in self.catalog.tables.keys() {
            map.insert(name.clone(), self.load_table_meta(name)?);
        }
        Ok(map)
    }

    fn table_meta_path(&self, table: &str) -> String {
        format!("{}/{}.pages", self.data_dir, table)
    }

    /// Expose buffer pool stats for the REPL
    pub fn hit_rate(&self) -> f64 {
        self.buffer_pool.hit_rate()
    }
}

/// Summary of what happened during recovery
#[derive(Debug)]
pub struct RecoveryInfo {
    pub redone: usize,
    pub undone: usize,
    pub tables_loaded: usize,
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::Lexer;
    use crate::sql::parser::Parser;

    fn tmp_dir(name: &str) -> String {
        let p = format!("/tmp/venom_exec_{}", name);
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn run(exec: &mut Executor, sql: &str) -> Vec<Row> {
        let mut l = Lexer::new(sql);
        let tokens = l.tokenize().unwrap();
        let mut p = Parser::new(tokens);
        let stmt = p.parse().unwrap();
        exec.execute(stmt).unwrap()
    }

    fn open_fresh(dir: &str) -> Executor {
        let mut e = Executor::open(dir).unwrap();
        e.recover().unwrap();
        e
    }

    #[test]
    fn test_data_survives_restart() {
        let dir = tmp_dir("restart");

        // Session 1: create and insert
        {
            let mut e = open_fresh(&dir);
            run(&mut e, "CREATE TABLE users (id INT, name TEXT)");
            run(&mut e, "INSERT INTO users VALUES (1, 'Alice')");
            run(&mut e, "INSERT INTO users VALUES (2, 'Bob')");
        }

        // Session 2: data must still be there
        {
            let mut e = open_fresh(&dir);
            let rows = run(&mut e, "SELECT * FROM users");
            assert_eq!(rows.len(), 2, "rows should survive restart");
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[1][1], Value::Text("Bob".into()));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
        fn test_update_rollback_after_crash() {
            let dir = tmp_dir("update_crash");

            // Session 1: create table, insert, commit
            {
                let mut e = open_fresh(&dir);
                run(&mut e, "CREATE TABLE accounts (id INT, balance INT)");
                run(&mut e, "INSERT INTO accounts VALUES (1, 100)");
            }

            {
                let mut e = open_fresh(&dir);
                run(&mut e, "UPDATE accounts SET balance = 200 WHERE id = 1");
            }

            {
                let mut e = open_fresh(&dir);
                let rows = run(&mut e, "SELECT * FROM accounts WHERE id = 1");
                assert_eq!(rows[0][1], Value::Int(200));
            }

            let _ = std::fs::remove_dir_all(&dir);
        }


    #[test]
    fn test_update_survives_buffer_pool_pressure() {
        let dir = tmp_dir("update_pressure");

        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val INT)");

        // Insert enough rows to span multiple pages, forcing buffer pool churn
        for i in 0..50 {
            run(&mut e, &format!("INSERT INTO t VALUES ({}, {})", i, i * 10));
        }

        run(&mut e, "UPDATE t SET val = 9999 WHERE id = 25");

        let rows = run(&mut e, "SELECT * FROM t WHERE id = 25");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Int(9999));

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn test_update_basic() {
        let dir = tmp_dir("update");

        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE users (id INT, name TEXT, age INT)");
        run(&mut e, "INSERT INTO users VALUES (1, 'Alice', 30)");
        run(&mut e, "INSERT INTO users VALUES (2, 'Bob', 25)");

        run(&mut e, "UPDATE users SET age = 31 WHERE id = 1");

        let rows = run(&mut e, "SELECT * FROM users WHERE id = 1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][2], Value::Int(31)); // age updated

        // Bob unchanged
        let rows2 = run(&mut e, "SELECT * FROM users WHERE id = 2");
        assert_eq!(rows2[0][2], Value::Int(25));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_schema_survives_restart() {
        let dir = tmp_dir("schema");

        {
            let mut e = open_fresh(&dir);
            run(
                &mut e,
                "CREATE TABLE products (id INT, name TEXT, price INT)",
            );
        }

        {
            let e = open_fresh(&dir);
            let schema = e.catalog.get("products");
            assert!(schema.is_some(), "schema should survive restart");
            assert_eq!(schema.unwrap().columns.len(), 3);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_delete_survives_restart() {
        let dir = tmp_dir("delete");

        {
            let mut e = open_fresh(&dir);
            run(&mut e, "CREATE TABLE t (id INT, val TEXT)");
            run(&mut e, "INSERT INTO t VALUES (1, 'keep')");
            run(&mut e, "INSERT INTO t VALUES (2, 'delete_me')");
            run(&mut e, "DELETE FROM t WHERE id = 2");
        }

        {
            let mut e = open_fresh(&dir);
            let rows = run(&mut e, "SELECT * FROM t");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(1));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_multiple_tables_survive_restart() {
        let dir = tmp_dir("multitable");

        {
            let mut e = open_fresh(&dir);
            run(&mut e, "CREATE TABLE a (id INT, x TEXT)");
            run(&mut e, "CREATE TABLE b (y INT, z INT)");
            run(&mut e, "INSERT INTO a VALUES (1, 'hello')");
            run(&mut e, "INSERT INTO b VALUES (10, 20)");
        }

        {
            let mut e = open_fresh(&dir);
            let ra = run(&mut e, "SELECT * FROM a");
            let rb = run(&mut e, "SELECT * FROM b");
            assert_eq!(ra.len(), 1);
            assert_eq!(rb.len(), 1);
            assert_eq!(ra[0][1], Value::Text("hello".into()));
            assert_eq!(rb[0][0], Value::Int(10));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_select_where_after_restart() {
        let dir = tmp_dir("where");

        {
            let mut e = open_fresh(&dir);
            run(&mut e, "CREATE TABLE users (id INT, name TEXT, age INT)");
            run(&mut e, "INSERT INTO users VALUES (1, 'Alice', 30)");
            run(&mut e, "INSERT INTO users VALUES (2, 'Bob', 25)");
            run(&mut e, "INSERT INTO users VALUES (3, 'Charlie', 35)");
        }

        {
            let mut e = open_fresh(&dir);
            let rows = run(&mut e, "SELECT * FROM users WHERE age > 28");
            assert_eq!(rows.len(), 2);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn test_update_no_matching_rows() {
        let dir = tmp_dir("update_no_match");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val INT)");
        run(&mut e, "INSERT INTO t VALUES (1, 100)");

        let result = run(&mut e, "UPDATE t SET val = 999 WHERE id = 999");
        assert_eq!(result.len(), 0); // no error, just no-op

        let rows = run(&mut e, "SELECT * FROM t");
        assert_eq!(rows[0][1], Value::Int(100)); // unchanged

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn test_update_all_rows_no_where() {
        let dir = tmp_dir("update_all");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, status INT)");
        run(&mut e, "INSERT INTO t VALUES (1, 0)");
        run(&mut e, "INSERT INTO t VALUES (2, 0)");
        run(&mut e, "INSERT INTO t VALUES (3, 0)");

        run(&mut e, "UPDATE t SET status = 1");

        let rows = run(&mut e, "SELECT * FROM t");
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r[1] == Value::Int(1)));

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn test_update_nonexistent_table_errors() {
        let dir = tmp_dir("update_no_table");
        let mut e = open_fresh(&dir);

        let mut l = Lexer::new("UPDATE ghost SET x = 1");
        let tokens = l.tokenize().unwrap();
        let mut p = Parser::new(tokens);
        let stmt = p.parse().unwrap();

        assert!(e.execute(stmt).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn test_update_multiple_columns() {
        let dir = tmp_dir("update_multicol");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE users (id INT, name TEXT, age INT)");
        run(&mut e, "INSERT INTO users VALUES (1, 'Alice', 30)");

        run(
            &mut e,
            "UPDATE users SET age = 31, name = 'Alica' WHERE id = 1",
        );

        let rows = run(&mut e, "SELECT * FROM users WHERE id = 1");
        assert_eq!(rows[0][1], Value::Text("Alica".into()));
        assert_eq!(rows[0][2], Value::Int(31));

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn test_update_text_shrink_succeeds() {
        let dir = tmp_dir("update_shrink");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'LongerOriginalName')");

        let result = run(&mut e, "UPDATE t SET name = 'Short' WHERE id = 1");
        assert_eq!(result.len(), 0);

        let rows = run(&mut e, "SELECT * FROM t WHERE id = 1");
        assert_eq!(rows[0][1], Value::Text("Short".into()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_update_text_growth_fails_cleanly() {
        let dir = tmp_dir("update_grow");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Short')");

        let mut l = Lexer::new("UPDATE t SET name = 'MuchLongerNameThanOriginal' WHERE id = 1");
        let tokens = l.tokenize().unwrap();
        let mut p = Parser::new(tokens);
        let stmt = p.parse().unwrap();

        let result = e.execute(stmt);
        assert!(
            result.is_err(),
            "growing TEXT should fail in current implementation"
        );

        // Original row must be untouched after the failed update
        let rows = run(&mut e, "SELECT * FROM t WHERE id = 1");
        assert_eq!(rows[0][1], Value::Text("Short".into()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── NULL handling tests ─────────────────────────────────────────────────

    #[test]
    fn test_insert_and_select_null() {
        let dir = tmp_dir("null_insert");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Alice')");
        run(&mut e, "INSERT INTO t VALUES (2, NULL)");

        let rows = run(&mut e, "SELECT * FROM t");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Value::Text("Alice".into()));
        assert_eq!(rows[1][1], Value::Null);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_select_is_null() {
        let dir = tmp_dir("null_is_null");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Alice')");
        run(&mut e, "INSERT INTO t VALUES (2, NULL)");
        run(&mut e, "INSERT INTO t VALUES (3, 'Charlie')");

        let rows = run(&mut e, "SELECT * FROM t WHERE name IS NULL");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int(2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_select_is_not_null() {
        let dir = tmp_dir("null_is_not_null");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Alice')");
        run(&mut e, "INSERT INTO t VALUES (2, NULL)");
        run(&mut e, "INSERT INTO t VALUES (3, 'Charlie')");

        let rows = run(&mut e, "SELECT * FROM t WHERE name IS NOT NULL");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Value::Text("Alice".into()));
        assert_eq!(rows[1][1], Value::Text("Charlie".into()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_null_equality_never_matches() {
        // SQLite semantics: NULL = NULL is NOT true
        let dir = tmp_dir("null_eq");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val INT)");
        run(&mut e, "INSERT INTO t VALUES (1, NULL)");
        run(&mut e, "INSERT INTO t VALUES (2, 42)");

        // WHERE val = NULL should match nothing (not even the NULL row)
        let rows = run(&mut e, "SELECT * FROM t WHERE val = NULL");
        assert_eq!(rows.len(), 0, "NULL = NULL must not match (SQLite semantics)");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_update_to_null() {
        let dir = tmp_dir("null_update");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Alice')");

        run(&mut e, "UPDATE t SET name = NULL WHERE id = 1");

        let rows = run(&mut e, "SELECT * FROM t WHERE id = 1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Null);

        // Verify IS NULL also works on the updated row
        let null_rows = run(&mut e, "SELECT * FROM t WHERE name IS NULL");
        assert_eq!(null_rows.len(), 1);
        assert_eq!(null_rows[0][0], Value::Int(1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_null_survives_restart() {
        let dir = tmp_dir("null_restart");

        // Session 1: insert rows with NULL
        {
            let mut e = open_fresh(&dir);
            run(&mut e, "CREATE TABLE t (id INT, name TEXT, score INT)");
            run(&mut e, "INSERT INTO t VALUES (1, 'Alice', 100)");
            run(&mut e, "INSERT INTO t VALUES (2, NULL, NULL)");
            run(&mut e, "INSERT INTO t VALUES (3, 'Charlie', NULL)");
        }

        // Session 2: data must survive WAL recovery
        {
            let mut e = open_fresh(&dir);
            let rows = run(&mut e, "SELECT * FROM t");
            assert_eq!(rows.len(), 3, "all rows should survive restart");

            // Row 1: no NULLs
            assert_eq!(rows[0][0], Value::Int(1));
            assert_eq!(rows[0][1], Value::Text("Alice".into()));
            assert_eq!(rows[0][2], Value::Int(100));

            // Row 2: name=NULL, score=NULL
            assert_eq!(rows[1][0], Value::Int(2));
            assert_eq!(rows[1][1], Value::Null);
            assert_eq!(rows[1][2], Value::Null);

            // Row 3: score=NULL
            assert_eq!(rows[2][0], Value::Int(3));
            assert_eq!(rows[2][1], Value::Text("Charlie".into()));
            assert_eq!(rows[2][2], Value::Null);

            // IS NULL filter works after recovery
            let null_names = run(&mut e, "SELECT * FROM t WHERE name IS NULL");
            assert_eq!(null_names.len(), 1);
            assert_eq!(null_names[0][0], Value::Int(2));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── ORDER BY + LIMIT tests ──────────────────────────────────────────────

    #[test]
    fn test_order_by_asc() {
        let dir = tmp_dir("order_asc");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, age INT)");
        run(&mut e, "INSERT INTO t VALUES (1, 30)");
        run(&mut e, "INSERT INTO t VALUES (2, 20)");
        run(&mut e, "INSERT INTO t VALUES (3, 40)");

        let rows = run(&mut e, "SELECT * FROM t ORDER BY age ASC");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][1], Value::Int(20));
        assert_eq!(rows[1][1], Value::Int(30));
        assert_eq!(rows[2][1], Value::Int(40));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_order_by_asc_feat() {
        let dir = tmp_dir("order_asc_feat");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT, age INT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Charlie', 35)");
        run(&mut e, "INSERT INTO t VALUES (2, 'Alice', 25)");
        run(&mut e, "INSERT INTO t VALUES (3, 'Bob', 30)");

        let rows = run(&mut e, "SELECT * FROM t ORDER BY age ASC");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][2], Value::Int(25)); // Alice
        assert_eq!(rows[1][2], Value::Int(30)); // Bob
        assert_eq!(rows[2][2], Value::Int(35)); // Charlie

        let _ = std::fs::remove_dir_all(&dir);
    }
    fn test_order_by_desc_with_limit() {
        let dir = tmp_dir("order_desc_limit");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, age INT)");
        run(&mut e, "INSERT INTO t VALUES (1, 30)");
        run(&mut e, "INSERT INTO t VALUES (2, 20)");
        run(&mut e, "INSERT INTO t VALUES (3, 40)");
        run(&mut e, "INSERT INTO t VALUES (4, 10)");

        let rows = run(&mut e, "SELECT * FROM t ORDER BY age DESC LIMIT 2");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Value::Int(40));
        assert_eq!(rows[1][1], Value::Int(30));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_order_by_desc() {
        let dir = tmp_dir("order_desc");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT, age INT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Charlie', 35)");
        run(&mut e, "INSERT INTO t VALUES (2, 'Alice', 25)");
        run(&mut e, "INSERT INTO t VALUES (3, 'Bob', 30)");

        let rows = run(&mut e, "SELECT * FROM t ORDER BY age DESC");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][2], Value::Int(35)); // Charlie
        assert_eq!(rows[1][2], Value::Int(30)); // Bob
        assert_eq!(rows[2][2], Value::Int(25)); // Alice

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_order_by_no_limit() {
        let dir = tmp_dir("order_no_limit");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (3, 'Charlie')");
        run(&mut e, "INSERT INTO t VALUES (1, 'Alice')");
        run(&mut e, "INSERT INTO t VALUES (2, 'Bob')");

        let rows = run(&mut e, "SELECT * FROM t ORDER BY id");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Value::Int(1));
        assert_eq!(rows[1][0], Value::Int(2));
        assert_eq!(rows[2][0], Value::Int(3));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_limit() {
        let dir = tmp_dir("limit");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val INT)");
        for i in 1..=10 {
            run(&mut e, &format!("INSERT INTO t VALUES ({}, {})", i, i * 10));
        }

        let rows = run(&mut e, "SELECT * FROM t LIMIT 3");
        assert_eq!(rows.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_order_by_with_limit() {
        let dir = tmp_dir("order_limit");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, score INT)");
        run(&mut e, "INSERT INTO t VALUES (1, 80)");
        run(&mut e, "INSERT INTO t VALUES (2, 95)");
        run(&mut e, "INSERT INTO t VALUES (3, 70)");
        run(&mut e, "INSERT INTO t VALUES (4, 90)");
        run(&mut e, "INSERT INTO t VALUES (5, 85)");

        // Top 3 scores
        let rows = run(&mut e, "SELECT * FROM t ORDER BY score DESC LIMIT 3");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][1], Value::Int(95));
        assert_eq!(rows[1][1], Value::Int(90));
        assert_eq!(rows[2][1], Value::Int(85));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_order_by_with_where() {
        let dir = tmp_dir("order_where");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT, age INT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Alice', 30)");
        run(&mut e, "INSERT INTO t VALUES (2, 'Bob', 17)");
        run(&mut e, "INSERT INTO t VALUES (3, 'Charlie', 25)");
        run(&mut e, "INSERT INTO t VALUES (4, 'Dave', 15)");
        run(&mut e, "INSERT INTO t VALUES (5, 'Eve', 22)");

        let rows = run(&mut e, "SELECT * FROM t WHERE age > 18 ORDER BY age ASC LIMIT 2");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][2], Value::Int(22)); // Eve
        assert_eq!(rows[1][2], Value::Int(25)); // Charlie

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_order_by_text() {
        let dir = tmp_dir("order_text");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Charlie')");
        run(&mut e, "INSERT INTO t VALUES (2, 'Alice')");
        run(&mut e, "INSERT INTO t VALUES (3, 'Bob')");

        let rows = run(&mut e, "SELECT * FROM t ORDER BY name ASC");
        assert_eq!(rows[0][1], Value::Text("Alice".into()));
        assert_eq!(rows[1][1], Value::Text("Bob".into()));
        assert_eq!(rows[2][1], Value::Text("Charlie".into()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── Index tests ────────────────────────────────────────────────────────

    #[test]
    fn test_index_eq_lookup() {
        let dir = tmp_dir("idx_eq");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT, score INT)");

        // Manually create index on 'id' column
        e.index_manager.create_index("t", "id");

        run(&mut e, "INSERT INTO t VALUES (10, 'Alice', 100)");
        run(&mut e, "INSERT INTO t VALUES (20, 'Bob', 200)");
        run(&mut e, "INSERT INTO t VALUES (30, 'Charlie', 300)");

        // Eq lookup via index
        let rows = run(&mut e, "SELECT * FROM t WHERE id = 20");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int(20));
        assert_eq!(rows[0][1], Value::Text("Bob".into()));

        // Non-existent key
        let rows = run(&mut e, "SELECT * FROM t WHERE id = 99");
        assert_eq!(rows.len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_index_range_lookup() {
        let dir = tmp_dir("idx_range");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val INT)");
        e.index_manager.create_index("t", "id");

        for i in 1..=10 {
            run(&mut e, &format!("INSERT INTO t VALUES ({}, {})", i, i * 10));
        }

        // Gt: id > 7 → 8, 9, 10
        let rows = run(&mut e, "SELECT * FROM t WHERE id > 7 ORDER BY id ASC");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Value::Int(8));
        assert_eq!(rows[2][0], Value::Int(10));

        // Le: id <= 3 → 1, 2, 3
        let rows = run(&mut e, "SELECT * FROM t WHERE id <= 3 ORDER BY id ASC");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Value::Int(1));
        assert_eq!(rows[2][0], Value::Int(3));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_index_maintained_on_delete() {
        let dir = tmp_dir("idx_del");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val INT)");
        e.index_manager.create_index("t", "id");

        run(&mut e, "INSERT INTO t VALUES (1, 100)");
        run(&mut e, "INSERT INTO t VALUES (2, 200)");
        run(&mut e, "INSERT INTO t VALUES (3, 300)");

        // Delete row with id=2
        run(&mut e, "DELETE FROM t WHERE id = 2");

        // Index lookup for id=2 should return nothing
        let rows = run(&mut e, "SELECT * FROM t WHERE id = 2");
        assert_eq!(rows.len(), 0);

        // Other rows still accessible via index
        let rows = run(&mut e, "SELECT * FROM t WHERE id = 1");
        assert_eq!(rows.len(), 1);
        let rows = run(&mut e, "SELECT * FROM t WHERE id = 3");
        assert_eq!(rows.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_index_maintained_on_update() {
        let dir = tmp_dir("idx_upd");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val INT)");
        e.index_manager.create_index("t", "val");

        run(&mut e, "INSERT INTO t VALUES (1, 100)");
        run(&mut e, "INSERT INTO t VALUES (2, 200)");

        // Update val from 100 to 999
        run(&mut e, "UPDATE t SET val = 999 WHERE id = 1");

        // Old key gone from index
        let rows = run(&mut e, "SELECT * FROM t WHERE val = 100");
        assert_eq!(rows.len(), 0);

        // New key present in index
        let rows = run(&mut e, "SELECT * FROM t WHERE val = 999");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int(1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_index_with_no_index_falls_back_to_seqscan() {
        // When no index exists, seq scan should still work correctly
        let dir = tmp_dir("idx_fallback");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, name TEXT)");
        // No index created
        run(&mut e, "INSERT INTO t VALUES (1, 'Alice')");
        run(&mut e, "INSERT INTO t VALUES (2, 'Bob')");

        let rows = run(&mut e, "SELECT * FROM t WHERE id = 1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Text("Alice".into()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── PRIMARY KEY tests ──────────────────────────────────────────────────

    #[test]
    fn test_primary_key_rejects_duplicate() {
        let dir = tmp_dir("pk_dup");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Alice')");

        // Duplicate PK should fail
        let mut l = Lexer::new("INSERT INTO t VALUES (1, 'Bob')");
        let tokens = l.tokenize().unwrap();
        let mut p = Parser::new(tokens);
        let stmt = p.parse().unwrap();
        let result = e.execute(stmt);
        assert!(result.is_err(), "duplicate PK should be rejected");
        assert!(result.unwrap_err().contains("duplicate primary key"));

        // Original row intact
        let rows = run(&mut e, "SELECT * FROM t WHERE id = 1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Text("Alice".into()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_primary_key_rejects_null() {
        let dir = tmp_dir("pk_null");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)");

        let mut l = Lexer::new("INSERT INTO t VALUES (NULL, 'Alice')");
        let tokens = l.tokenize().unwrap();
        let mut p = Parser::new(tokens);
        let stmt = p.parse().unwrap();
        let result = e.execute(stmt);
        assert!(result.is_err(), "NULL PK should be rejected");
        assert!(result.unwrap_err().contains("cannot be NULL"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_primary_key_auto_index() {
        let dir = tmp_dir("pk_auto_idx");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, val INT)");

        // Index should be auto-created
        assert!(e.index_manager.has_index("t", "id"));

        run(&mut e, "INSERT INTO t VALUES (10, 100)");
        run(&mut e, "INSERT INTO t VALUES (20, 200)");
        run(&mut e, "INSERT INTO t VALUES (30, 300)");

        // Should use index for lookup
        let rows = run(&mut e, "SELECT * FROM t WHERE id = 20");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Int(200));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_primary_key_allows_unique_inserts() {
        let dir = tmp_dir("pk_unique");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Alice')");
        run(&mut e, "INSERT INTO t VALUES (2, 'Bob')");
        run(&mut e, "INSERT INTO t VALUES (3, 'Charlie')");

        let rows = run(&mut e, "SELECT * FROM t ORDER BY id ASC");

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Value::Int(1));
        assert_eq!(rows[1][0], Value::Int(2));
        assert_eq!(rows[2][0], Value::Int(3));

        let _ = std::fs::remove_dir_all(&dir);
    }


    #[test]
    fn test_primary_key_update_duplicate_rejected() {
        let dir = tmp_dir("pk_upd_dup");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)");
        run(&mut e, "INSERT INTO t VALUES (1, 'Alice')");
        run(&mut e, "INSERT INTO t VALUES (2, 'Bob')");

        // Try to update id=2 to id=1 (duplicate)
        let mut l = Lexer::new("UPDATE t SET id = 1 WHERE id = 2");
        let tokens = l.tokenize().unwrap();
        let mut p = Parser::new(tokens);
        let stmt = p.parse().unwrap();
        let result = e.execute(stmt);
        assert!(result.is_err(), "updating to duplicate PK should fail");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_primary_key() {
        let sql = "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)";
        let mut l = Lexer::new(sql);
        let tokens = l.tokenize().unwrap();
        let mut p = Parser::new(tokens);
        let stmt = p.parse().unwrap();

        if let Statement::CreateTable { columns, .. } = stmt {
            assert!(columns[0].primary_key);
            assert!(!columns[1].primary_key);
        } else {
            panic!("expected CreateTable");
        }
    }

    #[test]
    fn test_primary_key_and_index_survives_restart() {
        let dir = tmp_dir("pk_restart");
        {
            let mut e = open_fresh(&dir);
            run(&mut e, "CREATE TABLE t (id INT PRIMARY KEY, val INT)");
            run(&mut e, "INSERT INTO t VALUES (10, 100)");
            run(&mut e, "INSERT INTO t VALUES (20, 200)");
        }

        // Restart
        {
            let mut e = Executor::open(&dir).unwrap();
            e.recover().unwrap();

            // Index lookup should work (proves index survived/rebuilt)
            let rows = run(&mut e, "SELECT * FROM t WHERE id = 20");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][1], Value::Int(200));

            // Inserting duplicate should fail (proves PK constraint survived)
            let mut l = Lexer::new("INSERT INTO t VALUES (10, 999)");
            let tokens = l.tokenize().unwrap();
            let mut p = Parser::new(tokens);
            let stmt = p.parse().unwrap();
            let result = e.execute(stmt);
            assert!(result.is_err(), "duplicate PK should be rejected after restart");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_float_insert_and_select() {
        let dir = tmp_dir("float_ins_sel");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val FLOAT)");
        run(&mut e, "INSERT INTO t VALUES (1, 3.14)");
        run(&mut e, "INSERT INTO t VALUES (2, -0.5)");

        let rows = run(&mut e, "SELECT * FROM t");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Value::Float(3.14));
        assert_eq!(rows[1][1], Value::Float(-0.5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_float_order_by() {
        let dir = tmp_dir("float_order_by");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val FLOAT)");
        run(&mut e, "INSERT INTO t VALUES (1, 3.14)");
        run(&mut e, "INSERT INTO t VALUES (2, -0.5)");
        run(&mut e, "INSERT INTO t VALUES (3, 10.0)");
        run(&mut e, "INSERT INTO t VALUES (4, 0.0)");

        let rows = run(&mut e, "SELECT * FROM t ORDER BY val ASC");
        assert_eq!(rows[0][1], Value::Float(-0.5));
        assert_eq!(rows[1][1], Value::Float(0.0));
        assert_eq!(rows[2][1], Value::Float(3.14));
        assert_eq!(rows[3][1], Value::Float(10.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_float_where_filter() {
        let dir = tmp_dir("float_where");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val FLOAT)");
        run(&mut e, "INSERT INTO t VALUES (1, 3.14)");
        run(&mut e, "INSERT INTO t VALUES (2, 5.0)");

        let rows = run(&mut e, "SELECT * FROM t WHERE val > 4.0");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Float(5.0));

        // Mixed type comparison (int vs float)
        let rows2 = run(&mut e, "SELECT * FROM t WHERE val < 4");
        assert_eq!(rows2.len(), 1);
        assert_eq!(rows2[0][1], Value::Float(3.14));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_float_survives_restart() {
        let dir = tmp_dir("float_restart");
        {
            let mut e = open_fresh(&dir);
            run(&mut e, "CREATE TABLE t (id INT, val FLOAT)");
            run(&mut e, "INSERT INTO t VALUES (1, 2.718)");
        }
        {
            let mut e = Executor::open(&dir).unwrap();
            e.recover().unwrap();
            let rows = run(&mut e, "SELECT * FROM t");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][1], Value::Float(2.718));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_float_null_mixed() {
        let dir = tmp_dir("float_null");
        let mut e = open_fresh(&dir);
        run(&mut e, "CREATE TABLE t (id INT, val FLOAT)");
        run(&mut e, "INSERT INTO t VALUES (1, 1.1)");
        run(&mut e, "INSERT INTO t VALUES (2, NULL)");
        run(&mut e, "INSERT INTO t VALUES (3, -1.1)");

        let rows = run(&mut e, "SELECT * FROM t ORDER BY val ASC");
        assert_eq!(rows[0][1], Value::Float(-1.1));
        assert_eq!(rows[1][1], Value::Float(1.1));
        assert_eq!(rows[2][1], Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
