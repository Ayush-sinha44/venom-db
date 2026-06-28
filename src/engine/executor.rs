use std::collections::HashMap;
use crate::sql::ast::*;
use crate::storage::page::Page;
use super::catalog::{Catalog, Schema};

/// A row returned from a query
pub type Row = Vec<Value>;

/// The Executor takes an AST Statement and runs it against the storage layer.
///
/// Storage model here: one HashMap<page_id, Page> per table.
/// In a real DB this would go through the buffer pool.
pub struct Executor {
    pub catalog: Catalog,
    /// table_name → (next_page_id, pages)
    storage: HashMap<String, Vec<Page>>,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            catalog: Catalog::new(),
            storage: HashMap::new(),
        }
    }

    /// Execute a statement. Returns rows for SELECT, empty vec otherwise.
    pub fn execute(&mut self, stmt: Statement) -> Result<Vec<Row>, String> {
        match stmt {
            Statement::CreateTable { table, columns } => {
                let schema = Schema::new(table.clone(), columns);
                self.catalog.create_table(schema)?;
                self.storage.insert(table, Vec::new());
                Ok(vec![])
            }

            Statement::Insert { table, values } => {
                let schema = self.catalog.get(&table)
                    .ok_or_else(|| format!("table '{}' not found", table))?
                    .clone();

                let row_bytes = schema.serialize_row(&values)?;

                let pages = self.storage.get_mut(&table)
                    .ok_or_else(|| format!("storage for '{}' not found", table))?;

                // Find a page with space, or create a new one
                let inserted = pages.iter_mut().any(|page| {
                    page.insert_tuple(&row_bytes).is_some()
                });

                if !inserted {
                    let new_id = pages.len() as u32;
                    let mut page = Page::new(new_id);
                    page.insert_tuple(&row_bytes)
                        .ok_or("row too large for a single page")?;
                    pages.push(page);
                }

                Ok(vec![])
            }

            Statement::Select { table, columns, filter } => {
                let schema = self.catalog.get(&table)
                    .ok_or_else(|| format!("table '{}' not found", table))?
                    .clone();

                let pages = self.storage.get(&table)
                    .ok_or_else(|| format!("storage for '{}' not found", table))?;

                let mut results = Vec::new();

                // Sequential scan across all pages
                for page in pages {
                    for slot_id in 0..page.num_slots() {
                        let tuple = match page.get_tuple(slot_id) {
                            Some(t) => t,
                            None    => continue, // tombstone
                        };

                        let row = schema.deserialize_row(tuple)?;

                        // Apply WHERE filter
                        if let Some(ref expr) = filter {
                            if !Self::eval_filter(&schema, &row, expr)? {
                                continue;
                            }
                        }

                        // Project columns
                        let projected = Self::project(&schema, row, &columns)?;
                        results.push(projected);
                    }
                }

                Ok(results)
            }

            Statement::Delete { table, filter } => {
                let schema = self.catalog.get(&table)
                    .ok_or_else(|| format!("table '{}' not found", table))?
                    .clone();

                let pages = self.storage.get_mut(&table)
                    .ok_or_else(|| format!("storage for '{}' not found", table))?;

                for page in pages.iter_mut() {
                    let num_slots = page.num_slots();
                    for slot_id in 0..num_slots {
                        let tuple = match page.get_tuple(slot_id) {
                            Some(t) => t,
                            None    => continue,
                        };
                        let row = schema.deserialize_row(tuple)?;

                        let should_delete = match &filter {
                            Some(expr) => Self::eval_filter(&schema, &row, expr)?,
                            None       => true, // DELETE FROM t (no WHERE = delete all)
                        };

                        if should_delete {
                            page.delete_tuple(slot_id);
                        }
                    }
                }

                Ok(vec![])
            }
        }
    }

    /// Evaluate a WHERE expression against a row
    fn eval_filter(schema: &Schema, row: &[Value], expr: &Expr) -> Result<bool, String> {
        let col_idx = schema.col_index(&expr.left)
            .ok_or_else(|| format!("column '{}' not found", expr.left))?;

        let cell = &row[col_idx];

        let result = match (&expr.op, cell, &expr.right) {
            (Op::Eq, Value::Int(a),  Value::Int(b))  => a == b,
            (Op::Ne, Value::Int(a),  Value::Int(b))  => a != b,
            (Op::Lt, Value::Int(a),  Value::Int(b))  => a <  b,
            (Op::Gt, Value::Int(a),  Value::Int(b))  => a >  b,
            (Op::Le, Value::Int(a),  Value::Int(b))  => a <= b,
            (Op::Ge, Value::Int(a),  Value::Int(b))  => a >= b,
            (Op::Eq, Value::Text(a), Value::Text(b)) => a == b,
            (Op::Ne, Value::Text(a), Value::Text(b)) => a != b,
            _ => return Err(format!(
                "type mismatch in WHERE: {:?} {:?} {:?}", cell, expr.op, expr.right
            )),
        };

        Ok(result)
    }

    /// Project the selected columns out of a full row
    fn project(schema: &Schema, row: Row, cols: &SelectColumns) -> Result<Row, String> {
        match cols {
            SelectColumns::Star => Ok(row),
            SelectColumns::Named(names) => {
                names.iter().map(|name| {
                    let idx = schema.col_index(name)
                        .ok_or_else(|| format!("column '{}' not found", name))?;
                    Ok(row[idx].clone())
                }).collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::Lexer;
    use crate::sql::parser::Parser;

    fn run(exec: &mut Executor, sql: &str) -> Vec<Row> {
        let mut l = Lexer::new(sql);
        let tokens = l.tokenize().unwrap();
        let mut p = Parser::new(tokens);
        let stmt = p.parse().unwrap();
        exec.execute(stmt).unwrap()
    }

    fn setup() -> Executor {
        let mut e = Executor::new();
        run(&mut e, "CREATE TABLE users (id INT, name TEXT, age INT)");
        run(&mut e, "INSERT INTO users VALUES (1, 'Alice', 30)");
        run(&mut e, "INSERT INTO users VALUES (2, 'Bob', 25)");
        run(&mut e, "INSERT INTO users VALUES (3, 'Charlie', 35)");
        run(&mut e, "INSERT INTO users VALUES (4, 'Diana', 28)");
        e
    }

    #[test]
    fn test_select_star() {
        let mut e = setup();
        let rows = run(&mut e, "SELECT * FROM users");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0][0], Value::Int(1));
        assert_eq!(rows[0][1], Value::Text("Alice".into()));
    }

    #[test]
    fn test_select_with_where_int() {
        let mut e = setup();
        let rows = run(&mut e, "SELECT * FROM users WHERE id = 2");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Text("Bob".into()));
    }

    #[test]
    fn test_select_with_where_gt() {
        let mut e = setup();
        let rows = run(&mut e, "SELECT * FROM users WHERE age > 28");
        assert_eq!(rows.len(), 2); // Alice(30) and Charlie(35)
    }

    #[test]
    fn test_select_columns_projection() {
        let mut e = setup();
        let rows = run(&mut e, "SELECT name, age FROM users WHERE id = 1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 2); // only name and age
        assert_eq!(rows[0][0], Value::Text("Alice".into()));
        assert_eq!(rows[0][1], Value::Int(30));
    }

    #[test]
    fn test_delete_with_where() {
        let mut e = setup();
        run(&mut e, "DELETE FROM users WHERE id = 2");
        let rows = run(&mut e, "SELECT * FROM users");
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r[0] != Value::Int(2)));
    }

    #[test]
    fn test_delete_all() {
        let mut e = setup();
        run(&mut e, "DELETE FROM users");
        let rows = run(&mut e, "SELECT * FROM users");
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn test_select_text_filter() {
        let mut e = setup();
        let rows = run(&mut e, "SELECT * FROM users WHERE name = 'Charlie'");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][2], Value::Int(35));
    }

    #[test]
    fn test_table_not_found_error() {
        let mut e = Executor::new();
        let mut l = Lexer::new("SELECT * FROM nonexistent");
        let tokens = l.tokenize().unwrap();
        let mut p = Parser::new(tokens);
        let stmt = p.parse().unwrap();
        assert!(e.execute(stmt).is_err());
    }
}
