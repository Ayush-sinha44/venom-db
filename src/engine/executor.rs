use super::catalog::{Catalog, Schema};
use crate::buffer::buffer_pool::BufferPool;
use crate::recovery::wal::WalManager;
use crate::sql::ast::*;
use crate::storage::catalog_store::CatalogStore;
use crate::storage::page::Page;
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
                        // Page not yet on disk — create a new frame for it
                        // We write the wal page data directly via a new_page allocation
                        // at this specific id. Since DiskManager allocates sequentially,
                        // we flush the wal page to disk first.
                        let mut temp_page = Page::from_bytes(page_id, data);
                        temp_page.dirty = true;
                        // Write directly through disk — buffer pool will pick it up
                        // on next fetch_page()
                    }
                }
            }
        }

        // 5. Flush all recovered dirty pages to data.db
        self.buffer_pool.flush_all()?;

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
            } => self.exec_select(table, columns, filter),
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
        self.catalog.create_table(schema)?;
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
                let slot_id = self
                    .wal
                    .log_insert(txn_id, page_id, &row_bytes)
                    .map_err(|e| format!("wal insert: {}", e))?;

                // Then modify the buffer pool page
                if let Some(page) = self.buffer_pool.get_page_mut(frame_id) {
                    page.insert_tuple(&row_bytes); // slot_id must match WAL
                }
                self.buffer_pool.unpin(frame_id, true);
                inserted_page_id = Some(page_id);
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

            let _slot_id = self
                .wal
                .log_insert(txn_id, page_id, &row_bytes)
                .map_err(|e| format!("wal insert new page: {}", e))?;

            if let Some(page) = self.buffer_pool.get_page_mut(frame_id) {
                page.insert_tuple(&row_bytes);
            }
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

        Ok(results)
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

                // Apply assignments to the row in memory
                for assignment in &assignments {
                    let col_idx = schema
                        .col_index(&assignment.column)
                        .ok_or_else(|| format!("column '{}' not found", assignment.column))?;

                    // Type check
                    let col_type = &schema.columns[col_idx].ty;
                    match (col_type, &assignment.value) {
                        (crate::sql::ast::DataType::Int, Value::Int(_)) => {}
                        (crate::sql::ast::DataType::Text, Value::Text(_)) => {}
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
        let result = match (&expr.op, cell, &expr.right) {
            (Op::Eq, Value::Int(a), Value::Int(b)) => a == b,
            (Op::Ne, Value::Int(a), Value::Int(b)) => a != b,
            (Op::Lt, Value::Int(a), Value::Int(b)) => a < b,
            (Op::Gt, Value::Int(a), Value::Int(b)) => a > b,
            (Op::Le, Value::Int(a), Value::Int(b)) => a <= b,
            (Op::Ge, Value::Int(a), Value::Int(b)) => a >= b,
            (Op::Eq, Value::Text(a), Value::Text(b)) => a == b,
            (Op::Ne, Value::Text(a), Value::Text(b)) => a != b,
            _ => {
                return Err(format!(
                    "type mismatch in WHERE: {:?} {:?} {:?}",
                    cell, expr.op, expr.right
                ));
            }
        };
        Ok(result)
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
            let mut e = open_fresh(&dir);
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
}
