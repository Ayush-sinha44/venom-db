/// AST node types for the SQL subset venom-db supports.
///
/// Supported:
///   CREATE TABLE name (col type [PRIMARY KEY], ...)
///   INSERT INTO name VALUES (v1, v2, ...)
///   SELECT col,... FROM name [WHERE expr] [ORDER BY col ASC|DESC] [LIMIT n]
///   DELETE FROM name [WHERE expr]

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable {
        table: String,
        columns: Vec<ColumnDef>,
    },
    Insert {
        table: String,
        values: Vec<Value>,
    },
    Update{
        table:String,
        assignments:Vec<Assignment>,
        filter:Option<Expr>,
    },
    Select {
        table: String,
        columns: SelectColumns,
        filter: Option<Expr>,
        order_by: Option<(String, OrderDir)>,
        limit: Option<u64>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: DataType,
    pub primary_key: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OrderDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DataType {
    Int,
    Text,
    Float,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectColumns {
    Star,                    // SELECT *
    Named(Vec<String>),      // SELECT col1, col2
}



/// A literal value in SQL
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Text(String),
    Null,
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Value::Int(n)   => write!(f, "{}", n),
            Value::Float(n) => write!(f, "{}", n),
            Value::Text(s)  => write!(f, "{}", s),
            Value::Null     => write!(f, "NULL"),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Text(a), Value::Text(b)) => a.partial_cmp(b),
            _ => None,
        }
    }
}

/// A WHERE clause expression (we support simple binary comparisons)
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub left: String,   // column name
    pub op: Op,
    pub right: Value,   // literal
}
#[derive(Debug,Clone,PartialEq)]
pub struct Assignment{
    pub column:String,
    pub value:Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Eq,        // =
    Ne,        // !=
    Lt,        // <
    Gt,        // >
    Le,        // <=
    Ge,        // >=
    IsNull,    // IS NULL
    IsNotNull, // IS NOT NULL
}
