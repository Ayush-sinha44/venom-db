use std::collections::HashMap;
use crate::sql::ast::{ColumnDef, DataType, Value};

/// The Catalog stores table schemas: which columns exist and their types.
/// In a real DB this lives on disk. Here we keep it in memory.
#[derive(Debug, Clone)]
pub struct Schema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// Name of the primary key column, if any
    pub primary_key: Option<String>,
}

impl Schema {
    pub fn new(name: String, columns: Vec<ColumnDef>) -> Self {
        let primary_key = columns.iter()
            .find(|c| c.primary_key)
            .map(|c| c.name.clone());
        Self { name, columns, primary_key }
    }

    /// Column index by name
    pub fn col_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Primary key column index, if any
    pub fn pk_col_index(&self) -> Option<usize> {
        self.primary_key.as_ref().and_then(|pk| self.col_index(pk))
    }

    /// Serialize a row of Values into bytes for storage
    /// Format: each field prefixed by 1-byte type tag + length
    pub fn serialize_row(&self, values: &[Value]) -> Result<Vec<u8>, String> {
        if values.len() != self.columns.len() {
            return Err(format!(
                "expected {} values, got {}", self.columns.len(), values.len()
            ));
        }

        let mut buf = Vec::new();
        for (col, val) in self.columns.iter().zip(values.iter()) {
            match (&col.ty, val) {
                (_, Value::Null) => {
                    buf.push(2u8); // type tag: null (no payload)
                }
                (DataType::Int, Value::Int(n)) => {
                    buf.push(0u8); // type tag: int
                    buf.extend(&n.to_le_bytes());
                }
                (DataType::Text, Value::Text(s)) => {
                    buf.push(1u8); // type tag: text
                    let bytes = s.as_bytes();
                    buf.extend(&(bytes.len() as u32).to_le_bytes());
                    buf.extend(bytes);
                }
                _ => return Err(format!(
                    "type mismatch for column '{}': expected {:?}, got {:?}",
                    col.name, col.ty, val
                )),
            }
        }
        Ok(buf)
    }

    /// Deserialize bytes back into a row of Values
    pub fn deserialize_row(&self, buf: &[u8]) -> Result<Vec<Value>, String> {
        let mut values = Vec::new();
        let mut off = 0;

        for col in &self.columns {
            if off >= buf.len() {
                return Err("unexpected end of row data".into());
            }
            match buf[off] {
                2 => {
                    // NULL: tag only, no payload
                    off += 1;
                    values.push(Value::Null);
                }
                0 if matches!(col.ty, DataType::Int) => {
                    off += 1;
                    let n = i64::from_le_bytes(
                        buf[off..off+8].try_into().map_err(|_| "int read error")?
                    );
                    off += 8;
                    values.push(Value::Int(n));
                }
                1 if matches!(col.ty, DataType::Text) => {
                    off += 1;
                    let len = u32::from_le_bytes(
                        buf[off..off+4].try_into().map_err(|_| "text len error")?
                    ) as usize;
                    off += 4;
                    let s = String::from_utf8(buf[off..off+len].to_vec())
                        .map_err(|_| "utf8 error")?;
                    off += len;
                    values.push(Value::Text(s));
                }
                tag => return Err(format!("type tag {} doesn't match {:?}", tag, col.ty)),
            }
        }
        Ok(values)
    }
}

/// The catalog holds all table schemas
#[derive(Default, Debug, Clone)]
pub struct Catalog {
    pub tables: HashMap<String, Schema>,
    pub index_defs: Vec<(String, String)>,
}

impl Catalog {
    pub fn new() -> Self { Self::default() }

    pub fn create_table(&mut self, schema: Schema) -> Result<(), String> {
        if self.tables.contains_key(&schema.name) {
            return Err(format!("table '{}' already exists", schema.name));
        }
        self.tables.insert(schema.name.clone(), schema);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Schema> {
        self.tables.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_schema() -> Schema {
        Schema::new("users".into(), vec![
            ColumnDef { name: "id".into(),   ty: DataType::Int,  primary_key: false },
            ColumnDef { name: "name".into(), ty: DataType::Text, primary_key: false },
            ColumnDef { name: "age".into(),  ty: DataType::Int,  primary_key: false },
        ])
    }

    #[test]
    fn test_serialize_roundtrip() {
        let schema = make_schema();
        let row = vec![Value::Int(1), Value::Text("Ayush".into()), Value::Int(21)];
        let bytes = schema.serialize_row(&row).unwrap();
        let back = schema.deserialize_row(&bytes).unwrap();
        assert_eq!(row, back);
    }

    #[test]
    fn test_type_mismatch_error() {
        let schema = make_schema();
        // pass Text where Int expected
        let row = vec![Value::Text("oops".into()), Value::Text("x".into()), Value::Int(1)];
        assert!(schema.serialize_row(&row).is_err());
    }

    #[test]
    fn test_col_index() {
        let schema = make_schema();
        assert_eq!(schema.col_index("id"), Some(0));
        assert_eq!(schema.col_index("name"), Some(1));
        assert_eq!(schema.col_index("missing"), None);
    }
}
