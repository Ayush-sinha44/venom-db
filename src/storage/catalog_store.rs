/// CatalogStore — persists table schemas to `catalog.bin` in the data directory.
///
/// Format (binary, little-endian):
///   [num_tables: u32]
///   for each table:
///     [name_len: u32][name_bytes]
///     [num_cols: u32]
///     for each column:
///       [col_name_len: u32][col_name_bytes]
///       [type_tag: u8]   (0 = INT, 1 = TEXT)
///
/// This is intentionally simple — no versioning yet. When we add more
/// types we'll add a file header with a version byte.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use crate::sql::ast::{ColumnDef, DataType};
use crate::engine::catalog::{Catalog, Schema};

pub struct CatalogStore {
    path: String,
}

impl CatalogStore {
    pub fn new(data_dir: &str) -> Self {
        Self {
            path: format!("{}/catalog.bin", data_dir),
        }
    }

    /// Write all schemas in the catalog to disk. Called after every CREATE TABLE.
    pub fn save(&self, catalog: &Catalog) -> io::Result<()> {
        let mut buf = Vec::new();

        let tables: Vec<&Schema> = catalog.tables.values().collect();
        buf.extend(&(tables.len() as u32).to_le_bytes());

        for schema in &tables {
            // Table name
            let name_bytes = schema.name.as_bytes();
            buf.extend(&(name_bytes.len() as u32).to_le_bytes());
            buf.extend(name_bytes);

            // Columns
            buf.extend(&(schema.columns.len() as u32).to_le_bytes());
            for col in &schema.columns {
                let col_bytes = col.name.as_bytes();
                buf.extend(&(col_bytes.len() as u32).to_le_bytes());
                buf.extend(col_bytes);
                let tag: u8 = match col.ty {
                    DataType::Int  => 0,
                    DataType::Text => 1,
                };
                buf.push(tag);
            }
        }

        // Atomic write: write to tmp then rename so a crash mid-write
        // doesn't corrupt the catalog file.
        let tmp = format!("{}.tmp", self.path);
        {
            let mut f = OpenOptions::new()
                .write(true).create(true).truncate(true)
                .open(&tmp)?;
            f.write_all(&buf)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Read all schemas from disk and populate the catalog.
    /// Returns an empty catalog if the file doesn't exist yet (first run).
    pub fn load(&self) -> io::Result<Catalog> {
        let mut catalog = Catalog::new();

        let mut f = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(catalog),
            Err(e) => return Err(e),
        };

        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;

        let mut off = 0;

        macro_rules! read_u32 {
            () => {{
                if off + 4 > buf.len() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "catalog truncated"));
                }
                let v = u32::from_le_bytes(buf[off..off+4].try_into().unwrap());
                off += 4;
                v
            }};
        }

        macro_rules! read_str {
            () => {{
                let len = read_u32!() as usize;
                if off + len > buf.len() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "catalog string truncated"));
                }
                let s = String::from_utf8(buf[off..off+len].to_vec())
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf8 in catalog"))?;
                off += len;
                s
            }};
        }

        let num_tables = read_u32!();

        for _ in 0..num_tables {
            let table_name = read_str!();
            let num_cols = read_u32!();
            let mut columns = Vec::new();

            for _ in 0..num_cols {
                let col_name = read_str!();
                if off >= buf.len() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "catalog col type missing"));
                }
                let ty = match buf[off] {
                    0 => DataType::Int,
                    1 => DataType::Text,
                    t => return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown type tag {} in catalog", t),
                    )),
                };
                off += 1;
                columns.push(ColumnDef { name: col_name, ty, primary_key: false });
            }

            // Use insert directly — catalog.create_table() would error on duplicate
            // which we don't want during load.
            let schema = Schema::new(table_name.clone(), columns);
            catalog.tables.insert(table_name, schema);
        }

        Ok(catalog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::{ColumnDef, DataType};

    fn make_catalog() -> Catalog {
        let mut c = Catalog::new();
        c.create_table(Schema::new("users".into(), vec![
            ColumnDef { name: "id".into(),   ty: DataType::Int,  primary_key: false },
            ColumnDef { name: "name".into(), ty: DataType::Text, primary_key: false },
            ColumnDef { name: "age".into(),  ty: DataType::Int,  primary_key: false },
        ])).unwrap();
        c.create_table(Schema::new("posts".into(), vec![
            ColumnDef { name: "post_id".into(), ty: DataType::Int,  primary_key: false },
            ColumnDef { name: "body".into(),    ty: DataType::Text, primary_key: false },
        ])).unwrap();
        c
    }

    #[test]
    fn test_catalog_roundtrip() {
        let dir = "/tmp/venom_catalog_test";
        std::fs::create_dir_all(dir).unwrap();
        let store = CatalogStore::new(dir);

        let original = make_catalog();
        store.save(&original).unwrap();

        let loaded = store.load().unwrap();

        // Same tables present
        assert!(loaded.get("users").is_some());
        assert!(loaded.get("posts").is_some());

        // Same columns
        let users = loaded.get("users").unwrap();
        assert_eq!(users.columns.len(), 3);
        assert_eq!(users.columns[0].name, "id");
        assert_eq!(users.columns[0].ty, DataType::Int);
        assert_eq!(users.columns[1].name, "name");
        assert_eq!(users.columns[1].ty, DataType::Text);

        let posts = loaded.get("posts").unwrap();
        assert_eq!(posts.columns.len(), 2);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_empty_catalog_on_first_run() {
        let store = CatalogStore::new("/tmp/venom_no_such_dir_xyz");
        let catalog = store.load().unwrap();
        assert!(catalog.tables.is_empty());
    }
}
