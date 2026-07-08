use super::lexer::Token;
use super::ast::*;

/// Parser: consumes a token stream and produces an AST Statement.
pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    pub fn parse(&mut self) -> Result<Statement, String> {
        match self.peek() {
            Token::Create => self.parse_create(),
            Token::Insert => self.parse_insert(),
            Token::Select => self.parse_select(),
            Token::Delete => self.parse_delete(),
            Token::Update => self.parse_update(),
            t => Err(format!("unexpected token: {:?}", t)),

        }
    }

    // --- Peek / consume helpers ---

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> &Token {
        let tok = self.tokens.get(self.pos).unwrap_or(&Token::Eof);
        self.pos += 1;
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<(), String> {
        let tok = self.advance().clone();
        if std::mem::discriminant(&tok) == std::mem::discriminant(expected) {
            Ok(())
        } else {
            Err(format!("expected {:?}, got {:?}", expected, tok))
        }
    }

    fn expect_ident(&mut self) -> Result<String, String> {
        match self.advance().clone() {
            Token::Ident(s) => Ok(s),
            t => Err(format!("expected identifier, got {:?}", t)),
        }
    }

    // --- Statement parsers ---

    /// CREATE TABLE name (col type, ...)
    fn parse_create(&mut self) -> Result<Statement, String> {
        self.expect(&Token::Create)?;
        self.expect(&Token::Table)?;
        let table = self.expect_ident()?;
        self.expect(&Token::LParen)?;

        let mut columns = Vec::new();
        loop {
            let name = self.expect_ident()?;
            let ty = match self.advance().clone() {
                Token::Int  => DataType::Int,
                Token::Text => DataType::Text,
                t => return Err(format!("expected type, got {:?}", t)),
            };
            let primary_key = if self.peek() == &Token::Primary {
                self.advance(); // consume PRIMARY
                self.expect(&Token::Key)?; // consume KEY
                true
            } else {
                false
            };
            columns.push(ColumnDef { name, ty, primary_key });

            match self.peek() {
                Token::Comma  => { self.advance(); }
                Token::RParen => { self.advance(); break; }
                t => return Err(format!("expected ',' or ')', got {:?}", t)),
            }
        }

        Ok(Statement::CreateTable { table, columns })
    }

    /// INSERT INTO name VALUES (v1, v2, ...)
    fn parse_insert(&mut self) -> Result<Statement, String> {
        self.expect(&Token::Insert)?;
        self.expect(&Token::Into)?;
        let table = self.expect_ident()?;
        self.expect(&Token::Values)?;
        self.expect(&Token::LParen)?;

        let mut values = Vec::new();
        loop {
            let val = match self.advance().clone() {
                Token::IntLit(n)  => Value::Int(n),
                Token::TextLit(s) => Value::Text(s),
                Token::Null       => Value::Null,
                t => return Err(format!("expected value, got {:?}", t)),
            };
            values.push(val);

            match self.peek() {
                Token::Comma  => { self.advance(); }
                Token::RParen => { self.advance(); break; }
                t => return Err(format!("expected ',' or ')', got {:?}", t)),
            }
        }

        Ok(Statement::Insert { table, values })
    }

    /// SELECT col,... FROM name [WHERE expr]
    fn parse_select(&mut self) -> Result<Statement, String> {
        self.expect(&Token::Select)?;

        let columns = if matches!(self.peek(), Token::Star) {
            self.advance();
            SelectColumns::Star
        } else {
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                if matches!(self.peek(), Token::Comma) { self.advance(); } else { break; }
            }
            SelectColumns::Named(cols)
        };

        self.expect(&Token::From)?;
        let table = self.expect_ident()?;

        let filter = if matches!(self.peek(), Token::Where) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        // Optional ORDER BY col ASC|DESC
        let order_by = if self.peek() == &Token::Order {
            self.advance(); // consume ORDER
            self.expect(&Token::By)?; // consume BY
            let col = self.expect_ident()?;
            let dir = match self.peek() {
                Token::Asc  => { self.advance(); OrderDir::Asc }
                Token::Desc => { self.advance(); OrderDir::Desc }
                _           => OrderDir::Asc, // default to ASC
            };
            Some((col, dir))
        } else {
            None
        };

        // Optional LIMIT n
        let limit = if self.peek() == &Token::Limit {
            self.advance(); // consume LIMIT
            match self.advance().clone() {
                Token::IntLit(n) if n >= 0 => Some(n as u64),
                t => return Err(format!("expected positive integer after LIMIT, got {:?}", t)),
            }
        } else {
            None
        };

        Ok(Statement::Select { table, columns, filter, order_by, limit })
    }

    /// DELETE FROM name [WHERE expr]
    fn parse_delete(&mut self) -> Result<Statement, String> {
        self.expect(&Token::Delete)?;
        self.expect(&Token::From)?;
        let table = self.expect_ident()?;

        let filter = if matches!(self.peek(), Token::Where) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        Ok(Statement::Delete { table, filter })
    }

    /// col op value  (e.g. age > 18)
    fn parse_expr(&mut self) -> Result<Expr, String> {
        let left = self.expect_ident()?;

        // Handle IS NULL / IS NOT NULL
        if self.peek() == &Token::Is {
            self.advance(); // consume IS
            if self.peek() == &Token::Not {
                self.advance(); // consume NOT
                self.expect(&Token::Null)?;
                return Ok(Expr { left, op: Op::IsNotNull, right: Value::Null });
            }
            self.expect(&Token::Null)?;
            return Ok(Expr { left, op: Op::IsNull, right: Value::Null });
        }

        let op = match self.advance().clone() {
            Token::Eq => Op::Eq,
            Token::Ne => Op::Ne,
            Token::Lt => Op::Lt,
            Token::Gt => Op::Gt,
            Token::Le => Op::Le,
            Token::Ge => Op::Ge,
            t => return Err(format!("expected operator, got {:?}", t)),
        };
        let right = match self.advance().clone() {
            Token::IntLit(n)  => Value::Int(n),
            Token::TextLit(s) => Value::Text(s),
            Token::Null       => Value::Null,
            t => return Err(format!("expected value, got {:?}", t)),
        };
        Ok(Expr { left, op, right })
    }
    fn parse_update(&mut self) -> Result<Statement, String> {
       self.expect(&Token::Update)?;
      let table = self.expect_ident()?;
      self.expect(&Token::Set)?;

      let mut assignments = Vec::new();
      loop {
        let column = self.expect_ident()?;
        self.expect(&Token::Eq)?;
        let value = match self.advance().clone() {
            Token::IntLit(n)  => Value::Int(n),
            Token::TextLit(s) => Value::Text(s),
            Token::Null       => Value::Null,
            t => return Err(format!("expected value, got {:?}", t)),
        };
        assignments.push(Assignment { column, value });

        match self.peek() {
            Token::Comma => { self.advance(); }
            _            => break,
        }
      }

      let filter = if self.peek() == &Token::Where {
        self.advance();
        Some(self.parse_expr()?)
      } else {
        None
       };

       Ok(Statement::Update { table, assignments, filter })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::Lexer;

    fn parse(sql: &str) -> Statement {
        let mut l = Lexer::new(sql);
        let tokens = l.tokenize().unwrap();
        let mut p = Parser::new(tokens);
        p.parse().unwrap()
    }

    #[test]
    fn test_parse_create() {
        let s = parse("CREATE TABLE users (id INT, name TEXT, age INT)");
        assert!(matches!(s, Statement::CreateTable { .. }));
        if let Statement::CreateTable { table, columns } = s {
            assert_eq!(table, "users");
            assert_eq!(columns.len(), 3);
            assert_eq!(columns[0].name, "id");
            assert_eq!(columns[1].ty, DataType::Text);
        }
    }
    #[test]
    fn test_parse_update() {
        let sql = "UPDATE users SET age = 31 WHERE id = 1";
         let mut l = Lexer::new(sql);
         let tokens = l.tokenize().unwrap();
         let mut p = Parser::new(tokens);
        let stmt = p.parse().unwrap();

        match stmt {
             Statement::Update { table, assignments, filter } => {
                 assert_eq!(table, "users");
                assert_eq!(assignments.len(), 1);
                assert_eq!(assignments[0].column, "age");
                assert_eq!(assignments[0].value, Value::Int(31));
                 assert!(filter.is_some());
        }
        _ => panic!("expected Update"),
    }
}

    #[test]
    fn test_parse_insert() {
        let s = parse("INSERT INTO users VALUES (1, 'Ayush', 21)");
        assert!(matches!(s, Statement::Insert { .. }));
        if let Statement::Insert { table, values } = s {
            assert_eq!(table, "users");
            assert_eq!(values[0], Value::Int(1));
            assert_eq!(values[1], Value::Text("Ayush".into()));
        }
    }

    #[test]
    fn test_parse_select_star() {
        let s = parse("SELECT * FROM users");
        assert!(matches!(s, Statement::Select { columns: SelectColumns::Star, .. }));
    }

    #[test]
    fn test_parse_select_with_where() {
        let s = parse("SELECT name, age FROM users WHERE id = 1");
        if let Statement::Select { columns, filter, .. } = s {
            assert!(matches!(columns, SelectColumns::Named(_)));
            let f = filter.unwrap();
            assert_eq!(f.left, "id");
            assert_eq!(f.op, Op::Eq);
            assert_eq!(f.right, Value::Int(1));
        }
    }

    #[test]
    fn test_parse_delete_with_where() {
        let s = parse("DELETE FROM users WHERE id = 1");
        if let Statement::Delete { table, filter } = s {
            assert_eq!(table, "users");
            assert!(filter.is_some());
        }
    }

    #[test]
    fn test_parse_order_by_desc() {
        let s = parse("SELECT * FROM users ORDER BY age DESC");
        if let Statement::Select { order_by, limit, .. } = s {
            let (col, dir) = order_by.unwrap();
            assert_eq!(col, "age");
            assert_eq!(dir, OrderDir::Desc);
            assert!(limit.is_none());
        } else {
            panic!("expected Select");
        }
    }

    #[test]
    fn test_parse_order_by_asc_default() {
        let s = parse("SELECT * FROM users ORDER BY name");
        if let Statement::Select { order_by, .. } = s {
            let (col, dir) = order_by.unwrap();
            assert_eq!(col, "name");
            assert_eq!(dir, OrderDir::Asc); // default
        } else {
            panic!("expected Select");
        }
    }

    #[test]
    fn test_parse_limit() {
        let s = parse("SELECT * FROM users LIMIT 5");
        if let Statement::Select { order_by, limit, .. } = s {
            assert!(order_by.is_none());
            assert_eq!(limit, Some(5));
        } else {
            panic!("expected Select");
        }
    }

    #[test]
    fn test_parse_order_by_with_limit() {
        let s = parse("SELECT * FROM users WHERE age > 18 ORDER BY age DESC LIMIT 10");
        if let Statement::Select { filter, order_by, limit, .. } = s {
            assert!(filter.is_some());
            let (col, dir) = order_by.unwrap();
            assert_eq!(col, "age");
            assert_eq!(dir, OrderDir::Desc);
            assert_eq!(limit, Some(10));
        } else {
            panic!("expected Select");
        }
    }
}
