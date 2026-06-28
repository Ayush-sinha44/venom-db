/// Lexer: turns a raw SQL string into a flat list of tokens.
/// Handles: keywords, identifiers, literals, punctuation.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Keywords
    Create, Table, Insert, Into, Values,
    Select, From, Where, Delete,
    Int, Text,

    // Punctuation
    LParen, RParen, Comma, Semicolon, Star,

    // Operators
    Eq, Ne, Lt, Gt, Le, Ge,

    // Literals / identifiers
    Ident(String),
    IntLit(i64),
    TextLit(String),

    Eof,
}

pub struct Lexer {
    input: Vec<char>,
    pos: usize,
}

impl Lexer {
    pub fn new(input: &str) -> Self {
        Self { input: input.chars().collect(), pos: 0 }
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, String> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            let is_eof = tok == Token::Eof;
            tokens.push(tok);
            if is_eof { break; }
        }
        Ok(tokens)
    }

    fn peek(&self) -> Option<char> {
        self.input.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.input.get(self.pos).copied();
        self.pos += 1;
        c
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.advance();
        }
    }

    fn next_token(&mut self) -> Result<Token, String> {
        self.skip_whitespace();

        match self.peek() {
            None => Ok(Token::Eof),
            Some(c) => match c {
                '(' => { self.advance(); Ok(Token::LParen) }
                ')' => { self.advance(); Ok(Token::RParen) }
                ',' => { self.advance(); Ok(Token::Comma) }
                ';' => { self.advance(); Ok(Token::Semicolon) }
                '*' => { self.advance(); Ok(Token::Star) }
                '=' => { self.advance(); Ok(Token::Eq) }
                '<' => {
                    self.advance();
                    if self.peek() == Some('=') { self.advance(); Ok(Token::Le) }
                    else { Ok(Token::Lt) }
                }
                '>' => {
                    self.advance();
                    if self.peek() == Some('=') { self.advance(); Ok(Token::Ge) }
                    else { Ok(Token::Gt) }
                }
                '!' => {
                    self.advance();
                    if self.peek() == Some('=') { self.advance(); Ok(Token::Ne) }
                    else { Err(format!("unexpected char '!'")) }
                }
                '\'' => self.read_string(),
                c if c.is_ascii_digit() || c == '-' => self.read_number(),
                c if c.is_alphabetic() || c == '_' => self.read_ident_or_keyword(),
                c => Err(format!("unexpected character: '{}'", c)),
            }
        }
    }

    fn read_string(&mut self) -> Result<Token, String> {
        self.advance(); // consume opening '
        let mut s = String::new();
        loop {
            match self.advance() {
                Some('\'') => break,
                Some(c)    => s.push(c),
                None       => return Err("unterminated string literal".into()),
            }
        }
        Ok(Token::TextLit(s))
    }

    fn read_number(&mut self) -> Result<Token, String> {
        let mut s = String::new();
        if self.peek() == Some('-') { s.push('-'); self.advance(); }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            s.push(self.advance().unwrap());
        }
        s.parse::<i64>()
            .map(Token::IntLit)
            .map_err(|_| format!("invalid number: {}", s))
    }

    fn read_ident_or_keyword(&mut self) -> Result<Token, String> {
        let mut s = String::new();
        while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == '_') {
            s.push(self.advance().unwrap());
        }
        Ok(match s.to_uppercase().as_str() {
            "CREATE" => Token::Create,
            "TABLE"  => Token::Table,
            "INSERT" => Token::Insert,
            "INTO"   => Token::Into,
            "VALUES" => Token::Values,
            "SELECT" => Token::Select,
            "FROM"   => Token::From,
            "WHERE"  => Token::Where,
            "DELETE" => Token::Delete,
            "INT"    => Token::Int,
            "TEXT"   => Token::Text,
            _        => Token::Ident(s),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_select() {
        let mut l = Lexer::new("SELECT * FROM users WHERE id = 1");
        let tokens = l.tokenize().unwrap();
        assert!(matches!(tokens[0], Token::Select));
        assert!(matches!(tokens[1], Token::Star));
        assert!(matches!(tokens[2], Token::From));
        assert!(matches!(tokens[3], Token::Ident(_)));
        assert!(matches!(tokens[4], Token::Where));
        assert!(matches!(tokens[6], Token::Eq));
        assert!(matches!(tokens[7], Token::IntLit(1)));
    }

    #[test]
    fn test_tokenize_string_literal() {
        let mut l = Lexer::new("INSERT INTO t VALUES (1, 'hello')");
        let tokens = l.tokenize().unwrap();
        assert!(tokens.contains(&Token::TextLit("hello".into())));
    }

    #[test]
    fn test_operators() {
        let mut l = Lexer::new("a != b <= c >= d");
        let tokens = l.tokenize().unwrap();
        assert!(tokens.contains(&Token::Ne));
        assert!(tokens.contains(&Token::Le));
        assert!(tokens.contains(&Token::Ge));
    }
}
