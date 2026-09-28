//! PartiQL for DynamoDB: the lexer, the statement grammar and its AST.
//!
//! One parser serves the executor and the IAM condition-key reader, so a
//! statement cannot be read one way for authorization and another way when it
//! runs. Positional `?` parameters are bound while parsing, in textual order,
//! so every consumer sees the same values in the same places.
//!
//! Everything here is a property of the statement alone: a statement that
//! fails to parse, binds the wrong number of parameters, or compares an
//! operand type that has no ordering is rejected before any table is looked
//! up, the way DynamoDB answers those ahead of ResourceNotFoundException.

use http::StatusCode;
use serde_json::{json, Value};

use fakecloud_core::service::AwsServiceError;

/// One step of a document path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathSegment {
    Name(String),
    Index(usize),
}

/// A document path: a top-level attribute followed by map keys and list
/// indexes. Never empty.
pub(crate) type Path = Vec<PathSegment>;

/// The top-level attribute a path starts at.
pub(crate) fn path_root(path: &[PathSegment]) -> &str {
    match path.first() {
        Some(PathSegment::Name(n)) => n,
        _ => "",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub(crate) fn text(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "<>",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }

    fn is_ordering(self) -> bool {
        matches!(self, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithOp {
    Add,
    Sub,
}

/// A PartiQL expression: a WHERE predicate, a SET right-hand side, or an
/// INSERT value.
#[derive(Debug, Clone)]
pub(crate) enum Expr {
    Path(Path),
    /// A literal or a bound `?` parameter, as an AttributeValue.
    Lit(Value),
    Missing,
    List(Vec<Expr>),
    Tuple(Vec<(String, Expr)>),
    /// `<< ... >>`: a string, number or binary set.
    Bag(Vec<Expr>),
    Cmp(Box<Expr>, CmpOp, Box<Expr>),
    Between(Box<Expr>, Box<Expr>, Box<Expr>),
    In(Box<Expr>, Vec<Expr>),
    Like(Box<Expr>, Box<Expr>),
    /// `expr IS [NOT] MISSING` (`null == false`) or `IS [NOT] NULL`.
    Is {
        expr: Box<Expr>,
        negated: bool,
        null: bool,
    },
    Func(String, Vec<Expr>),
    Arith(Box<Expr>, ArithOp, Box<Expr>),
    Neg(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

/// `FROM "table"` or `FROM "table"."index"`.
#[derive(Debug, Clone)]
pub(crate) struct Source {
    pub table: String,
    pub index: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Returning {
    /// `ALL` rather than `MODIFIED`.
    pub all: bool,
    /// `NEW` rather than `OLD`.
    pub new: bool,
}

impl Returning {
    pub(crate) fn text(self) -> String {
        format!(
            "RETURNING {} {} *",
            if self.all { "ALL" } else { "MODIFIED" },
            if self.new { "NEW" } else { "OLD" }
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) enum UpdateOp {
    Set(Path, Expr),
    Remove(Path),
}

#[derive(Debug, Clone)]
pub(crate) enum Projection {
    Star,
    Paths(Vec<Path>),
}

#[derive(Debug, Clone)]
pub(crate) struct OrderBy {
    pub path: Path,
    pub descending: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum Statement {
    Select {
        projection: Projection,
        source: Source,
        filter: Option<Expr>,
        order_by: Vec<OrderBy>,
    },
    Insert {
        table: String,
        value: Expr,
    },
    Update {
        source: Source,
        ops: Vec<UpdateOp>,
        filter: Option<Expr>,
        returning: Option<Returning>,
    },
    Delete {
        source: Source,
        filter: Option<Expr>,
        returning: Option<Returning>,
    },
    /// `EXISTS(SELECT ...)`: a condition check, only meaningful inside
    /// ExecuteTransaction.
    Exists(Box<Statement>),
}

impl Statement {
    /// The table the statement names.
    pub(crate) fn table(&self) -> &str {
        match self {
            Statement::Select { source, .. }
            | Statement::Update { source, .. }
            | Statement::Delete { source, .. } => &source.table,
            Statement::Insert { table, .. } => table,
            Statement::Exists(inner) => inner.table(),
        }
    }

    /// The index a `FROM "table"."index"` names.
    pub(crate) fn index(&self) -> Option<&str> {
        match self {
            Statement::Select { source, .. }
            | Statement::Update { source, .. }
            | Statement::Delete { source, .. } => source.index.as_deref(),
            Statement::Insert { .. } => None,
            Statement::Exists(inner) => inner.index(),
        }
    }

    pub(crate) fn returning(&self) -> Option<Returning> {
        match self {
            Statement::Update { returning, .. } | Statement::Delete { returning, .. } => *returning,
            _ => None,
        }
    }

    pub(crate) fn is_read(&self) -> bool {
        matches!(self, Statement::Select { .. })
    }
}

pub(crate) fn validation(message: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::BAD_REQUEST, "ValidationException", message)
}

fn malformed(detail: &str) -> AwsServiceError {
    validation(format!(
        "Statement wasn't well formed, can't be processed: {detail}"
    ))
}

// --- Lexer ---

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A bare identifier or keyword, as written.
    Ident(String),
    /// A double-quoted identifier.
    Quoted(String),
    /// A single-quoted string literal.
    Str(String),
    Num(String),
    Param,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    BagOpen,
    BagClose,
    Comma,
    Dot,
    Colon,
    Star,
    Plus,
    Minus,
    Op(CmpOp),
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '#'
}

fn lex(src: &str) -> Result<Vec<Tok>, AwsServiceError> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // `--` line comments.
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                let quote = c;
                let mut text = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(malformed("Unterminated literal")),
                        Some(&ch) if ch == quote => {
                            // A doubled quote is an escaped quote.
                            if chars.get(i + 1) == Some(&quote) {
                                text.push(quote);
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(&ch) => {
                            text.push(ch);
                            i += 1;
                        }
                    }
                }
                toks.push(if quote == '\'' {
                    Tok::Str(text)
                } else {
                    Tok::Quoted(text)
                });
                continue;
            }
            '?' => toks.push(Tok::Param),
            '(' => toks.push(Tok::LParen),
            ')' => toks.push(Tok::RParen),
            '[' => toks.push(Tok::LBracket),
            ']' => toks.push(Tok::RBracket),
            '{' => toks.push(Tok::LBrace),
            '}' => toks.push(Tok::RBrace),
            ',' => toks.push(Tok::Comma),
            ':' => toks.push(Tok::Colon),
            '*' => toks.push(Tok::Star),
            '+' => toks.push(Tok::Plus),
            '-' => toks.push(Tok::Minus),
            '=' => toks.push(Tok::Op(CmpOp::Eq)),
            '!' if chars.get(i + 1) == Some(&'=') => {
                toks.push(Tok::Op(CmpOp::Ne));
                i += 1;
            }
            '<' => match chars.get(i + 1) {
                Some('<') => {
                    toks.push(Tok::BagOpen);
                    i += 1;
                }
                Some('=') => {
                    toks.push(Tok::Op(CmpOp::Le));
                    i += 1;
                }
                Some('>') => {
                    toks.push(Tok::Op(CmpOp::Ne));
                    i += 1;
                }
                _ => toks.push(Tok::Op(CmpOp::Lt)),
            },
            '>' => match chars.get(i + 1) {
                Some('>') => {
                    toks.push(Tok::BagClose);
                    i += 1;
                }
                Some('=') => {
                    toks.push(Tok::Op(CmpOp::Ge));
                    i += 1;
                }
                _ => toks.push(Tok::Op(CmpOp::Gt)),
            },
            '.' if !chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()) => toks.push(Tok::Dot),
            _ if c.is_ascii_digit() || c == '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                if matches!(chars.get(i), Some('e' | 'E'))
                    && (chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())
                        || (matches!(chars.get(i + 1), Some('+' | '-'))
                            && chars.get(i + 2).is_some_and(|d| d.is_ascii_digit())))
                {
                    i += 2;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                // Digits running into letters are an identifier (`2fa`).
                if i < chars.len() && is_ident_char(chars[i]) {
                    while i < chars.len() && is_ident_char(chars[i]) {
                        i += 1;
                    }
                    toks.push(Tok::Ident(chars[start..i].iter().collect()));
                } else {
                    toks.push(Tok::Num(chars[start..i].iter().collect()));
                }
                continue;
            }
            _ if is_ident_char(c) => {
                let start = i;
                while i < chars.len() && is_ident_char(chars[i]) {
                    i += 1;
                }
                toks.push(Tok::Ident(chars[start..i].iter().collect()));
                continue;
            }
            _ => return Err(malformed(&format!("Unexpected character '{c}'"))),
        }
        i += 1;
    }
    Ok(toks)
}

// --- Parser ---

/// Keywords that end an expression or a path list and so can never be read
/// as a bare attribute name where an operand is expected.
const RESERVED: &[&str] = &[
    "AND",
    "OR",
    "NOT",
    "BETWEEN",
    "IN",
    "IS",
    "LIKE",
    "WHERE",
    "FROM",
    "SET",
    "REMOVE",
    "RETURNING",
    "ORDER",
    "TRUE",
    "FALSE",
    "NULL",
    "MISSING",
    "SELECT",
];

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    params: &'a [Value],
    next_param: usize,
}

/// Parse a PartiQL statement, binding `parameters` to its `?` placeholders.
pub(crate) fn parse_statement(
    statement: &str,
    parameters: &[Value],
) -> Result<Statement, AwsServiceError> {
    let (stmt, bound) = parse_grammar(statement, parameters)?;
    if bound != parameters.len() {
        return Err(validation(
            "Number of parameters in request and statement don't match.",
        ));
    }
    check_operand_types(&stmt)?;
    Ok(stmt)
}

/// The statement's shape -- its kind, table and index -- for callers that
/// only route or authorize it, without its parameters. `None` when it does
/// not parse; the executor rejects it then.
pub(crate) fn statement_shape(statement: &str) -> Option<Statement> {
    parse_grammar(statement, &[]).ok().map(|(stmt, _)| stmt)
}

/// Parse the grammar alone, returning how many `?` placeholders were bound.
fn parse_grammar(
    statement: &str,
    parameters: &[Value],
) -> Result<(Statement, usize), AwsServiceError> {
    let toks = lex(statement)?;
    let mut p = Parser {
        toks,
        pos: 0,
        params: parameters,
        next_param: 0,
    };
    let stmt = p.statement()?;
    if p.pos != p.toks.len() {
        return Err(p.unexpected());
    }
    Ok((stmt, p.next_param))
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn peek_at(&self, n: usize) -> Option<&Tok> {
        self.toks.get(self.pos + n)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn unexpected(&self) -> AwsServiceError {
        match self.peek() {
            None => malformed("Unexpected end of statement"),
            Some(t) => malformed(&format!("Unexpected token: {}", tok_text(t))),
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn is_kw_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.peek_at(n), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), AwsServiceError> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == Some(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Tok) -> Result<(), AwsServiceError> {
        if self.eat(tok) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn statement(&mut self) -> Result<Statement, AwsServiceError> {
        if self.eat_kw("SELECT") {
            self.select()
        } else if self.eat_kw("INSERT") {
            self.insert()
        } else if self.eat_kw("UPDATE") {
            self.update()
        } else if self.eat_kw("DELETE") {
            self.delete()
        } else if self.is_kw("EXISTS") && self.peek_at(1) == Some(&Tok::LParen) {
            self.pos += 2;
            self.expect_kw("SELECT")?;
            let inner = self.select()?;
            self.expect(&Tok::RParen)?;
            Ok(Statement::Exists(Box::new(inner)))
        } else {
            Err(malformed("Expected data manipulation"))
        }
    }

    fn select(&mut self) -> Result<Statement, AwsServiceError> {
        let projection = if self.eat(&Tok::Star) {
            Projection::Star
        } else {
            let mut paths = vec![self.path()?];
            while self.eat(&Tok::Comma) {
                paths.push(self.path()?);
            }
            Projection::Paths(paths)
        };
        self.expect_kw("FROM")?;
        let source = self.source()?;
        let filter = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut order_by = Vec::new();
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                let path = self.path()?;
                let descending = if self.eat_kw("DESC") {
                    true
                } else {
                    self.eat_kw("ASC");
                    false
                };
                order_by.push(OrderBy { path, descending });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        Ok(Statement::Select {
            projection,
            source,
            filter,
            order_by,
        })
    }

    fn insert(&mut self) -> Result<Statement, AwsServiceError> {
        self.expect_kw("INTO")?;
        let source = self.source()?;
        if source.index.is_some() {
            return Err(validation(
                "FROM clause may only contain a single table name",
            ));
        }
        self.expect_kw("VALUE")?;
        let value = self.expr()?;
        Ok(Statement::Insert {
            table: source.table,
            value,
        })
    }

    fn update(&mut self) -> Result<Statement, AwsServiceError> {
        let source = self.source()?;
        let mut ops = Vec::new();
        loop {
            if self.eat_kw("SET") {
                loop {
                    let path = self.path()?;
                    self.expect(&Tok::Op(CmpOp::Eq))?;
                    let value = self.additive()?;
                    ops.push(UpdateOp::Set(path, value));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            } else if self.eat_kw("REMOVE") {
                loop {
                    ops.push(UpdateOp::Remove(self.path()?));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        if ops.is_empty() {
            return Err(self.unexpected());
        }
        let filter = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        let returning = self.returning()?;
        Ok(Statement::Update {
            source,
            ops,
            filter,
            returning,
        })
    }

    fn delete(&mut self) -> Result<Statement, AwsServiceError> {
        self.expect_kw("FROM")?;
        let source = self.source()?;
        let filter = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        let returning = self.returning()?;
        Ok(Statement::Delete {
            source,
            filter,
            returning,
        })
    }

    fn returning(&mut self) -> Result<Option<Returning>, AwsServiceError> {
        if !self.eat_kw("RETURNING") {
            return Ok(None);
        }
        let all = if self.eat_kw("ALL") {
            true
        } else if self.eat_kw("MODIFIED") {
            false
        } else {
            return Err(self.unexpected());
        };
        let new = if self.eat_kw("NEW") {
            true
        } else if self.eat_kw("OLD") {
            false
        } else {
            return Err(self.unexpected());
        };
        self.expect(&Tok::Star)?;
        Ok(Some(Returning { all, new }))
    }

    /// The FROM path: at most two components (table and index), none empty.
    fn source(&mut self) -> Result<Source, AwsServiceError> {
        let mut parts = vec![self.name()?];
        while self.peek() == Some(&Tok::Dot) {
            self.pos += 1;
            parts.push(self.name()?);
        }
        if parts.iter().any(String::is_empty) {
            return Err(validation("Path component cannot be an empty string"));
        }
        if parts.len() > 2 {
            return Err(validation(
                "A path may contain at most 2 components in the FROM clause",
            ));
        }
        let index = (parts.len() == 2).then(|| parts.pop().unwrap_or_default());
        Ok(Source {
            table: parts.pop().unwrap_or_default(),
            index,
        })
    }

    fn name(&mut self) -> Result<String, AwsServiceError> {
        match self.peek() {
            Some(Tok::Quoted(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            Some(Tok::Ident(s)) if !is_reserved(s) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            _ => Err(self.unexpected()),
        }
    }

    /// A document path: `name`, `"name"`, then `.name` and `[n]` steps.
    fn path(&mut self) -> Result<Path, AwsServiceError> {
        let first = self.name()?;
        self.path_rest(first)
    }

    fn path_rest(&mut self, first: String) -> Result<Path, AwsServiceError> {
        let mut path = vec![PathSegment::Name(first)];
        loop {
            match self.peek() {
                Some(Tok::Dot) => {
                    self.pos += 1;
                    let name = match self.bump() {
                        Some(Tok::Quoted(s)) | Some(Tok::Ident(s)) => s,
                        _ => return Err(malformed("Invalid path")),
                    };
                    path.push(PathSegment::Name(name));
                }
                Some(Tok::LBracket) => {
                    self.pos += 1;
                    let index = match self.bump() {
                        Some(Tok::Num(n)) => {
                            n.parse::<usize>().map_err(|_| malformed("Invalid path"))?
                        }
                        _ => return Err(malformed("Invalid path")),
                    };
                    self.expect(&Tok::RBracket)?;
                    path.push(PathSegment::Index(index));
                }
                _ => return Ok(path),
            }
        }
    }

    fn expr(&mut self) -> Result<Expr, AwsServiceError> {
        let mut left = self.and()?;
        while self.eat_kw("OR") {
            let right = self.and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, AwsServiceError> {
        let mut left = self.not()?;
        while self.eat_kw("AND") {
            let right = self.not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, AwsServiceError> {
        if self.eat_kw("NOT") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.predicate()
    }

    fn predicate(&mut self) -> Result<Expr, AwsServiceError> {
        let left = self.additive()?;
        if let Some(Tok::Op(op)) = self.peek() {
            let op = *op;
            self.pos += 1;
            let right = self.additive()?;
            return Ok(Expr::Cmp(Box::new(left), op, Box::new(right)));
        }
        let negated = self.is_kw("NOT")
            && (self.is_kw_at(1, "BETWEEN") || self.is_kw_at(1, "IN") || self.is_kw_at(1, "LIKE"));
        if negated {
            self.pos += 1;
        }
        let wrap = |e: Expr| if negated { Expr::Not(Box::new(e)) } else { e };
        if self.eat_kw("BETWEEN") {
            let lo = self.additive()?;
            self.expect_kw("AND")?;
            let hi = self.additive()?;
            return Ok(wrap(Expr::Between(
                Box::new(left),
                Box::new(lo),
                Box::new(hi),
            )));
        }
        if self.eat_kw("IN") {
            let close = match self.bump() {
                Some(Tok::LBracket) => Tok::RBracket,
                Some(Tok::LParen) => Tok::RParen,
                _ => return Err(self.unexpected()),
            };
            let mut items = Vec::new();
            if !self.eat(&close) {
                loop {
                    items.push(self.additive()?);
                    if self.eat(&close) {
                        break;
                    }
                    self.expect(&Tok::Comma)?;
                }
            }
            return Ok(wrap(Expr::In(Box::new(left), items)));
        }
        if self.eat_kw("LIKE") {
            let pattern = self.additive()?;
            return Ok(wrap(Expr::Like(Box::new(left), Box::new(pattern))));
        }
        if negated {
            return Err(self.unexpected());
        }
        if self.eat_kw("IS") {
            let negated = self.eat_kw("NOT");
            let null = if self.eat_kw("MISSING") {
                false
            } else if self.eat_kw("NULL") {
                true
            } else {
                return Err(self.unexpected());
            };
            return Ok(Expr::Is {
                expr: Box::new(left),
                negated,
                null,
            });
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr, AwsServiceError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => ArithOp::Add,
                Some(Tok::Minus) => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.unary()?;
            left = Expr::Arith(Box::new(left), op, Box::new(right));
        }
    }

    fn unary(&mut self) -> Result<Expr, AwsServiceError> {
        if self.eat(&Tok::Minus) {
            return Ok(match self.unary()? {
                Expr::Lit(v) if v.get("N").is_some() => {
                    let n = v["N"].as_str().unwrap_or_default();
                    let negated = match n.strip_prefix('-') {
                        Some(rest) => rest.to_string(),
                        None if n == "0" => n.to_string(),
                        None => format!("-{n}"),
                    };
                    Expr::Lit(json!({ "N": negated }))
                }
                other => Expr::Neg(Box::new(other)),
            });
        }
        if self.eat(&Tok::Plus) {
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, AwsServiceError> {
        let Some(tok) = self.bump() else {
            return Err(malformed("Unexpected end of statement"));
        };
        match tok {
            Tok::Param => {
                let value = self.params.get(self.next_param).cloned();
                self.next_param += 1;
                // A count mismatch is reported once the whole statement is
                // read; a missing value is a placeholder until then.
                Ok(Expr::Lit(value.unwrap_or(Value::Null)))
            }
            Tok::Str(s) => Ok(Expr::Lit(json!({ "S": s }))),
            Tok::Num(n) => Ok(Expr::Lit(json!({ "N": number_literal(&n)? }))),
            Tok::LParen => {
                let inner = self.expr()?;
                self.expect(&Tok::RParen)?;
                Ok(inner)
            }
            Tok::LBracket => {
                let items = self.sequence(&Tok::RBracket)?;
                Ok(Expr::List(items))
            }
            Tok::BagOpen => {
                let items = self.sequence(&Tok::BagClose)?;
                Ok(Expr::Bag(items))
            }
            Tok::LBrace => {
                let mut fields = Vec::new();
                if !self.eat(&Tok::RBrace) {
                    loop {
                        let key = match self.bump() {
                            Some(Tok::Str(s)) | Some(Tok::Quoted(s)) | Some(Tok::Ident(s)) => s,
                            _ => return Err(malformed("Invalid tuple key")),
                        };
                        self.expect(&Tok::Colon)?;
                        let value = self.expr()?;
                        fields.push((key, value));
                        if self.eat(&Tok::RBrace) {
                            break;
                        }
                        self.expect(&Tok::Comma)?;
                    }
                }
                Ok(Expr::Tuple(fields))
            }
            Tok::Quoted(s) => self.path_rest(s).map(Expr::Path),
            Tok::Ident(s) => {
                let upper = s.to_ascii_uppercase();
                match upper.as_str() {
                    "TRUE" => return Ok(Expr::Lit(json!({ "BOOL": true }))),
                    "FALSE" => return Ok(Expr::Lit(json!({ "BOOL": false }))),
                    "NULL" => return Ok(Expr::Lit(json!({ "NULL": true }))),
                    "MISSING" => return Ok(Expr::Missing),
                    _ => {}
                }
                if self.peek() == Some(&Tok::LParen) {
                    self.pos += 1;
                    let args = self.sequence(&Tok::RParen)?;
                    return Ok(Expr::Func(s.to_ascii_lowercase(), args));
                }
                if is_reserved(&s) {
                    self.pos -= 1;
                    return Err(self.unexpected());
                }
                self.path_rest(s).map(Expr::Path)
            }
            other => {
                self.pos -= 1;
                let _ = other;
                Err(self.unexpected())
            }
        }
    }

    /// Comma-separated expressions up to `close`.
    fn sequence(&mut self, close: &Tok) -> Result<Vec<Expr>, AwsServiceError> {
        let mut items = Vec::new();
        if self.eat(close) {
            return Ok(items);
        }
        loop {
            items.push(self.expr()?);
            if self.eat(close) {
                return Ok(items);
            }
            self.expect(&Tok::Comma)?;
        }
    }
}

fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|kw| kw.eq_ignore_ascii_case(word))
}

fn tok_text(t: &Tok) -> String {
    match t {
        Tok::Ident(s) | Tok::Num(s) => s.clone(),
        Tok::Quoted(s) => format!("\"{s}\""),
        Tok::Str(s) => format!("'{s}'"),
        Tok::Param => "?".into(),
        Tok::LParen => "(".into(),
        Tok::RParen => ")".into(),
        Tok::LBracket => "[".into(),
        Tok::RBracket => "]".into(),
        Tok::LBrace => "{".into(),
        Tok::RBrace => "}".into(),
        Tok::BagOpen => "<<".into(),
        Tok::BagClose => ">>".into(),
        Tok::Comma => ",".into(),
        Tok::Dot => ".".into(),
        Tok::Colon => ":".into(),
        Tok::Star => "*".into(),
        Tok::Plus => "+".into(),
        Tok::Minus => "-".into(),
        Tok::Op(op) => op.text().into(),
    }
}

/// A numeric literal in DynamoDB's canonical form (`1.50` -> `1.5`).
fn number_literal(text: &str) -> Result<String, AwsServiceError> {
    super::partiql::canonical_number(text).ok_or_else(|| malformed("Invalid number literal"))
}

// --- Static checks ---

/// The AttributeValue type of a constant operand, if it is one.
fn constant_type(e: &Expr) -> Option<&str> {
    match e {
        Expr::Lit(v) => v
            .as_object()
            .and_then(|o| o.keys().next())
            .map(String::as_str),
        Expr::List(_) => Some("L"),
        Expr::Tuple(_) => Some("M"),
        Expr::Bag(items) => match items.first().and_then(constant_type) {
            Some("N") => Some("NS"),
            Some("B") => Some("BS"),
            _ => Some("SS"),
        },
        _ => None,
    }
}

/// An ordering operator (`<`, `<=`, `>`, `>=`, BETWEEN) refuses an operand
/// whose type has no ordering. The rule is about each operand's own type, so
/// it is decided from the statement before any row is read.
fn check_operand_types(stmt: &Statement) -> Result<(), AwsServiceError> {
    fn walk(e: &Expr) -> Result<(), AwsServiceError> {
        let unordered = |op: &str, operand: &Expr| -> Result<(), AwsServiceError> {
            match constant_type(operand) {
                Some(t @ ("BOOL" | "NULL" | "L" | "M" | "SS" | "NS" | "BS")) => Err(validation(
                    format!(
                        "Incorrect operand type for operator or function; operator or function: {op}, operand type: {t}"
                    ),
                )),
                _ => Ok(()),
            }
        };
        match e {
            Expr::Cmp(l, op, r) => {
                if op.is_ordering() {
                    unordered(op.text(), l)?;
                    unordered(op.text(), r)?;
                }
                walk(l)?;
                walk(r)
            }
            Expr::Between(v, lo, hi) => {
                for operand in [v, lo, hi] {
                    unordered("BETWEEN", operand)?;
                }
                walk(v)?;
                walk(lo)?;
                walk(hi)
            }
            Expr::And(l, r) | Expr::Or(l, r) | Expr::Arith(l, _, r) | Expr::Like(l, r) => {
                walk(l)?;
                walk(r)
            }
            Expr::Not(e) | Expr::Neg(e) | Expr::Is { expr: e, .. } => walk(e),
            Expr::In(v, items) => {
                walk(v)?;
                items.iter().try_for_each(walk)
            }
            Expr::Func(_, args) | Expr::List(args) | Expr::Bag(args) => {
                args.iter().try_for_each(walk)
            }
            Expr::Tuple(fields) => fields.iter().try_for_each(|(_, v)| walk(v)),
            Expr::Path(_) | Expr::Lit(_) | Expr::Missing => Ok(()),
        }
    }
    match stmt {
        Statement::Select { filter, .. }
        | Statement::Update { filter, .. }
        | Statement::Delete { filter, .. } => filter.as_ref().map_or(Ok(()), walk),
        Statement::Insert { .. } => Ok(()),
        Statement::Exists(inner) => check_operand_types(inner),
    }
}

// --- Analysis helpers ---

/// The top-level AND conjuncts of an expression.
pub(crate) fn conjuncts(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::And(l, r) => {
            let mut out = conjuncts(l);
            out.extend(conjuncts(r));
            out
        }
        other => vec![other],
    }
}

/// `attr = <constant>` (either way round): the attribute and the value.
pub(crate) fn equality_on(e: &Expr) -> Option<(&str, &Value)> {
    match e {
        Expr::Cmp(l, CmpOp::Eq, r) => match (l.as_ref(), r.as_ref()) {
            (Expr::Path(p), Expr::Lit(v)) | (Expr::Lit(v), Expr::Path(p)) if p.len() == 1 => {
                Some((path_root(p), v))
            }
            _ => None,
        },
        _ => None,
    }
}

/// `attr IN [<constants>]`: the attribute and the values.
pub(crate) fn membership_on(e: &Expr) -> Option<(&str, Vec<&Value>)> {
    match e {
        Expr::In(l, items) => match l.as_ref() {
            Expr::Path(p) if p.len() == 1 => {
                let values: Option<Vec<&Value>> = items
                    .iter()
                    .map(|i| match i {
                        Expr::Lit(v) => Some(v),
                        _ => None,
                    })
                    .collect();
                Some((path_root(p), values?))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Every top-level attribute an expression reads, in order of appearance.
pub(crate) fn expr_attributes(e: &Expr, out: &mut Vec<String>) {
    let mut push = |name: &str| {
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    };
    match e {
        Expr::Path(p) => push(path_root(p)),
        Expr::Lit(_) | Expr::Missing => {}
        Expr::List(items) | Expr::Bag(items) | Expr::Func(_, items) => {
            for i in items {
                expr_attributes(i, out);
            }
        }
        Expr::Tuple(fields) => {
            for (_, v) in fields {
                expr_attributes(v, out);
            }
        }
        Expr::Cmp(l, _, r)
        | Expr::And(l, r)
        | Expr::Or(l, r)
        | Expr::Like(l, r)
        | Expr::Arith(l, _, r) => {
            expr_attributes(l, out);
            expr_attributes(r, out);
        }
        Expr::Between(a, b, c) => {
            expr_attributes(a, out);
            expr_attributes(b, out);
            expr_attributes(c, out);
        }
        Expr::In(v, items) => {
            expr_attributes(v, out);
            for i in items {
                expr_attributes(i, out);
            }
        }
        Expr::Is { expr, .. } | Expr::Not(expr) | Expr::Neg(expr) => expr_attributes(expr, out),
    }
}

/// The values a WHERE expression confines `attr` to, if it confines it at
/// all: every row it selects has `attr` equal to one of them. `None` means
/// rows with any value of `attr` can match.
pub(crate) fn pinned_values(e: &Expr, attr: &str) -> Option<Vec<Value>> {
    if let Some((a, v)) = equality_on(e) {
        return (a == attr).then(|| vec![v.clone()]);
    }
    if let Some((a, vs)) = membership_on(e) {
        return (a == attr).then(|| vs.into_iter().cloned().collect());
    }
    match e {
        Expr::And(l, r) => match (pinned_values(l, attr), pinned_values(r, attr)) {
            (Some(a), Some(b)) => Some(
                a.into_iter()
                    .filter(|v| {
                        b.iter()
                            .any(|w| super::partiql::values_equal(Some(v), Some(w)))
                    })
                    .collect(),
            ),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        },
        Expr::Or(l, r) => {
            let mut a = pinned_values(l, attr)?;
            a.extend(pinned_values(r, attr)?);
            Some(a)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Statement, AwsServiceError> {
        parse_statement(s, &[])
    }

    #[test]
    fn parses_every_statement_kind() {
        assert!(matches!(
            parse("SELECT a, b.c[1] FROM \"t\" WHERE pk = 'x' AND n > 3").unwrap(),
            Statement::Select { .. }
        ));
        assert!(matches!(
            parse("insert into t value {'pk': 'a', 'n': 1, 'l': [1, 'x'], 's': <<'a','b'>>}")
                .unwrap(),
            Statement::Insert { .. }
        ));
        let Statement::Update { ops, returning, .. } =
            parse("UPDATE t SET a = 1, b = b + 2 SET c = 'x' REMOVE d, e[0] WHERE pk = 'k' RETURNING MODIFIED NEW *")
                .unwrap()
        else {
            panic!("not an update");
        };
        assert_eq!(ops.len(), 5);
        assert_eq!(
            returning,
            Some(Returning {
                all: false,
                new: true
            })
        );
        assert!(matches!(
            parse("DELETE FROM t WHERE pk = 'k' RETURNING ALL OLD *").unwrap(),
            Statement::Delete { .. }
        ));
        assert!(matches!(
            parse("EXISTS(SELECT * FROM t WHERE pk = 'k')").unwrap(),
            Statement::Exists(_)
        ));
    }

    #[test]
    fn quoted_text_is_data_not_syntax() {
        let Statement::Update { ops, filter, .. } =
            parse("UPDATE t SET note = 'go WHERE you SET it' WHERE pk = 'it''s'").unwrap()
        else {
            panic!("not an update");
        };
        assert_eq!(ops.len(), 1);
        let (attr, value) = equality_on(filter.as_ref().unwrap()).unwrap();
        assert_eq!(attr, "pk");
        assert_eq!(value, &json!({"S": "it's"}));
    }

    #[test]
    fn from_path_rules() {
        let err = |s: &str| parse(s).err().unwrap().to_string();
        assert!(err("SELECT * FROM \"t\".\"i\".\"x\"").contains("at most 2 components"));
        assert!(
            err("SELECT * FROM \"t\".\"\"").contains("Path component cannot be an empty string")
        );
        assert!(err("SELECT * FROM \"\"").contains("Path component cannot be an empty string"));
        assert!(err("INSERT INTO \"t\".\"i\" VALUE {'pk': 'a'}")
            .contains("FROM clause may only contain a single table name"));
        assert!(err("SLECT * FROM t").contains("Expected data manipulation"));
    }

    #[test]
    fn parameters_bind_in_textual_order_and_must_match() {
        let params = [json!({"S": "v"}), json!({"S": "k"})];
        let Statement::Update { ops, filter, .. } =
            parse_statement("UPDATE t SET a = ? WHERE pk = ?", &params).unwrap()
        else {
            panic!("not an update");
        };
        assert!(matches!(&ops[0], UpdateOp::Set(_, Expr::Lit(v)) if v == &params[0]));
        assert_eq!(equality_on(filter.as_ref().unwrap()).unwrap().1, &params[1]);
        let err = parse_statement("SELECT * FROM t WHERE pk = ?", &[])
            .err()
            .unwrap();
        assert!(err.to_string().contains("Number of parameters"));
    }

    #[test]
    fn ordering_operators_refuse_unordered_operands() {
        let err = parse_statement("SELECT * FROM t WHERE v <= ?", &[json!({"BOOL": true})])
            .err()
            .unwrap();
        assert_eq!(
            err.message(),
            "Incorrect operand type for operator or function; operator or function: <=, operand type: BOOL"
        );
        assert!(parse("SELECT * FROM t WHERE v BETWEEN 1 AND [1]").is_err());
        assert!(parse_statement("SELECT * FROM t WHERE v = ?", &[json!({"BOOL": true})]).is_ok());
    }

    #[test]
    fn numbers_are_canonical() {
        let Statement::Insert { value, .. } =
            parse("INSERT INTO t VALUE {'n': 1.50, 'm': -3, 'e': 1e3}").unwrap()
        else {
            panic!("not an insert");
        };
        let Expr::Tuple(fields) = value else {
            panic!("not a tuple");
        };
        let lit = |i: usize| match &fields[i].1 {
            Expr::Lit(v) => v["N"].as_str().unwrap().to_string(),
            other => panic!("{other:?}"),
        };
        assert_eq!(lit(0), "1.5");
        assert_eq!(lit(1), "-3");
        assert_eq!(lit(2), "1000");
    }
}
