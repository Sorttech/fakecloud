//! Parser, validator and evaluator for the DynamoDB expression grammars
//! (ConditionExpression / FilterExpression / KeyConditionExpression,
//! ProjectionExpression and UpdateExpression).
//!
//! Real DynamoDB parses every expression of a request before it touches the
//! table: syntax errors, reserved words, undefined or unused placeholders,
//! redundant parentheses, overlapping document paths and operand-type errors
//! on literal values are all ValidationExceptions raised up front, with the
//! exact messages reproduced here.

use super::reserved_words::is_reserved_word;
use super::*;

/// The maximum size of any single expression string, in bytes, measured on
/// the raw expression as sent (placeholders are not substituted).
pub(crate) const MAX_EXPRESSION_BYTES: usize = 4096;

/// Which expression parameter an expression came from. Determines the
/// `Invalid <Param>:` prefix of every error and the grammar used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExprKind {
    Condition,
    Filter,
    KeyCondition,
    Projection,
    Update,
}

impl ExprKind {
    pub(crate) fn param(self) -> &'static str {
        match self {
            ExprKind::Condition => "ConditionExpression",
            ExprKind::Filter => "FilterExpression",
            ExprKind::KeyCondition => "KeyConditionExpression",
            ExprKind::Projection => "ProjectionExpression",
            ExprKind::Update => "UpdateExpression",
        }
    }
}

fn validation(message: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::BAD_REQUEST, "ValidationException", message)
}

fn invalid(kind: ExprKind, message: impl AsRef<str>) -> AwsServiceError {
    validation(format!("Invalid {}: {}", kind.param(), message.as_ref()))
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A bare word: attribute name, keyword or function name.
    Ident,
    /// `#name` placeholder.
    Name,
    /// `:value` placeholder.
    Value,
    /// A double-quoted name (`"my attr"`). Not part of the expression
    /// grammar AWS accepts; only PartiQL-derived update text carries it.
    Quoted,
    /// An all-digit word (list index).
    Num,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    /// Any character the grammar does not know.
    Unknown,
    Eof,
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `allow_quoted` lexes a double-quoted span as one name token; strict
/// (request) parsing leaves `"` an unknown character, as AWS does.
fn lex(src: &str, allow_quoted: bool) -> Vec<Token> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let tok = match b {
            b'#' | b':' => {
                let mut j = i + 1;
                while j < bytes.len() && is_word_byte(bytes[j]) {
                    j += 1;
                }
                if j == i + 1 {
                    i += 1;
                    Tok::Unknown
                } else {
                    i = j;
                    if b == b'#' {
                        Tok::Name
                    } else {
                        Tok::Value
                    }
                }
            }
            _ if is_word_byte(b) => {
                let mut j = i;
                while j < bytes.len() && is_word_byte(bytes[j]) {
                    j += 1;
                }
                let all_digits = bytes[i..j].iter().all(u8::is_ascii_digit);
                i = j;
                if all_digits {
                    Tok::Num
                } else {
                    Tok::Ident
                }
            }
            b'"' if allow_quoted => match src[i + 1..].find('"') {
                Some(close) => {
                    i += close + 2;
                    Tok::Quoted
                }
                None => {
                    i += 1;
                    Tok::Unknown
                }
            },
            b'(' => {
                i += 1;
                Tok::LParen
            }
            b')' => {
                i += 1;
                Tok::RParen
            }
            b'[' => {
                i += 1;
                Tok::LBracket
            }
            b']' => {
                i += 1;
                Tok::RBracket
            }
            b',' => {
                i += 1;
                Tok::Comma
            }
            b'.' => {
                i += 1;
                Tok::Dot
            }
            b'=' => {
                i += 1;
                Tok::Eq
            }
            b'+' => {
                i += 1;
                Tok::Plus
            }
            b'-' => {
                i += 1;
                Tok::Minus
            }
            b'<' => match bytes.get(i + 1) {
                Some(b'=') => {
                    i += 2;
                    Tok::Le
                }
                Some(b'>') => {
                    i += 2;
                    Tok::Ne
                }
                _ => {
                    i += 1;
                    Tok::Lt
                }
            },
            b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    i += 2;
                    Tok::Ge
                } else {
                    i += 1;
                    Tok::Gt
                }
            }
            _ => {
                // Consume one whole (possibly multi-byte) character.
                let ch_len = src[i..].chars().next().map(char::len_utf8).unwrap_or(1);
                i += ch_len;
                Tok::Unknown
            }
        };
        out.push(Token { tok, start, end: i });
    }
    out.push(Token {
        tok: Tok::Eof,
        start: src.len(),
        end: src.len(),
    });
    out
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

/// One element of a resolved document path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum PathElem {
    Attr(String),
    Index(usize),
}

pub(crate) type DocPath = Vec<PathElem>;

/// Render a path the way DynamoDB does in its messages: `[a, b]`, `[l, [0]]`.
pub(crate) fn render_path(path: &[PathElem]) -> String {
    let parts: Vec<String> = path
        .iter()
        .map(|e| match e {
            PathElem::Attr(a) => a.clone(),
            PathElem::Index(i) => format!("[{i}]"),
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

fn is_prefix(a: &[PathElem], b: &[PathElem]) -> bool {
    a.len() <= b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

#[derive(Debug, Clone)]
pub(crate) enum Operand {
    Path(DocPath),
    Value(String),
    Size(DocPath),
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
    fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "<>",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Cond {
    Compare(Operand, CmpOp, Operand),
    Between(Operand, Operand, Operand),
    In(Operand, Vec<Operand>),
    Exists(DocPath),
    NotExists(DocPath),
    AttrType(DocPath, Operand),
    BeginsWith(Operand, Operand),
    Contains(Operand, Operand),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
    Paren(Box<Cond>),
}

/// The right-hand side of a `SET` action.
#[derive(Debug, Clone)]
pub(crate) enum SetValue {
    Operand(Operand),
    IfNotExists(Box<SetValue>),
    ListAppend(Box<SetValue>, Box<SetValue>),
    Arith(Box<SetValue>, Box<SetValue>),
    Paren(Box<SetValue>),
}

/// A parsed UpdateExpression. Only the shape needed for validation and for
/// the `UPDATED_*` return values is kept; the update itself is applied by the
/// update-expression machinery in `helpers`.
#[derive(Debug, Clone, Default)]
pub(crate) struct UpdateAst {
    pub(crate) sets: Vec<(DocPath, SetValue)>,
    pub(crate) removes: Vec<DocPath>,
    pub(crate) adds: Vec<(DocPath, String)>,
    pub(crate) deletes: Vec<(DocPath, String)>,
}

impl UpdateAst {
    /// Every document path the update writes, in expression order.
    pub(crate) fn target_paths(&self) -> Vec<DocPath> {
        let mut out: Vec<DocPath> = Vec::new();
        out.extend(self.sets.iter().map(|(p, _)| p.clone()));
        out.extend(self.removes.iter().cloned());
        out.extend(self.adds.iter().map(|(p, _)| p.clone()));
        out.extend(self.deletes.iter().map(|(p, _)| p.clone()));
        out
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Resolution context shared by every expression of one request, so that
/// placeholder usage can be tracked across all of them.
pub(crate) struct ExprContext<'a> {
    pub(crate) names: &'a HashMap<String, String>,
    pub(crate) values: &'a HashMap<String, Value>,
    pub(crate) used_names: std::collections::HashSet<String>,
    pub(crate) used_values: std::collections::HashSet<String>,
    /// Strict mode applies every request-level rule (reserved words,
    /// undefined placeholders). Lenient mode is for re-parsing an expression
    /// that was already validated or that fakecloud synthesized itself.
    pub(crate) strict: bool,
}

impl<'a> ExprContext<'a> {
    pub(crate) fn new(
        names: &'a HashMap<String, String>,
        values: &'a HashMap<String, Value>,
        strict: bool,
    ) -> Self {
        Self {
            names,
            values,
            used_names: Default::default(),
            used_values: Default::default(),
            strict,
        }
    }
}

struct Parser<'s, 'c, 'a> {
    src: &'s str,
    toks: Vec<Token>,
    pos: usize,
    kind: ExprKind,
    ctx: &'c mut ExprContext<'a>,
}

impl<'s, 'c, 'a> Parser<'s, 'c, 'a> {
    fn new(src: &'s str, kind: ExprKind, ctx: &'c mut ExprContext<'a>) -> Self {
        Self {
            src,
            toks: lex(src, !ctx.strict),
            pos: 0,
            kind,
            ctx,
        }
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, off: usize) -> &Tok {
        let i = (self.pos + off).min(self.toks.len() - 1);
        &self.toks[i].tok
    }

    fn text(&self, i: usize) -> &'s str {
        let t = &self.toks[i];
        &self.src[t.start..t.end]
    }

    fn cur_text(&self) -> &'s str {
        self.text(self.pos)
    }

    fn advance(&mut self) -> usize {
        let i = self.pos;
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        i
    }

    fn is_keyword(&self, kw: &str) -> bool {
        *self.peek() == Tok::Ident && self.cur_text().eq_ignore_ascii_case(kw)
    }

    /// `Syntax error; token: "<tok>", near: "<prev tok next>"` for the
    /// current token.
    fn syntax_error(&self) -> AwsServiceError {
        let i = self.pos;
        let t = &self.toks[i];
        let token = if t.tok == Tok::Eof {
            "<EOF>"
        } else {
            &self.src[t.start..t.end]
        };
        let near_start = if i > 0 {
            self.toks[i - 1].start
        } else {
            t.start
        };
        let near_end = match self.toks.get(i + 1) {
            Some(next) if next.tok != Tok::Eof => next.end,
            _ => t.end,
        };
        let near = &self.src[near_start..near_end.max(near_start)];
        invalid(
            self.kind,
            format!("Syntax error; token: \"{token}\", near: \"{near}\""),
        )
    }

    fn expect(&mut self, tok: Tok) -> Result<usize, AwsServiceError> {
        if *self.peek() == tok {
            Ok(self.advance())
        } else {
            Err(self.syntax_error())
        }
    }

    fn at_function(&self) -> bool {
        *self.peek() == Tok::Ident && *self.peek_at(1) == Tok::LParen
    }

    // -- paths ------------------------------------------------------------

    fn path_name(&mut self) -> Result<String, AwsServiceError> {
        match self.peek() {
            Tok::Ident => {
                let word = self.cur_text();
                const GRAMMAR_KEYWORDS: &[&str] = &[
                    "AND", "OR", "NOT", "BETWEEN", "IN", "SET", "REMOVE", "ADD", "DELETE",
                ];
                if GRAMMAR_KEYWORDS
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(word))
                {
                    return Err(self.syntax_error());
                }
                if self.ctx.strict && is_reserved_word(word) {
                    return Err(invalid(
                        self.kind,
                        format!("Attribute name is a reserved keyword; reserved keyword: {word}"),
                    ));
                }
                self.advance();
                Ok(word.to_string())
            }
            Tok::Quoted if !self.ctx.strict => {
                let text = self.cur_text();
                self.advance();
                Ok(text[1..text.len() - 1].to_string())
            }
            Tok::Name => {
                let ph = self.cur_text();
                let resolved = match self.ctx.names.get(ph) {
                    Some(n) => n.clone(),
                    None if self.ctx.strict => {
                        return Err(invalid(
                            self.kind,
                            format!(
                                "An expression attribute name used in the document path is \
                                 not defined; attribute name: {ph}"
                            ),
                        ));
                    }
                    None => ph.to_string(),
                };
                self.ctx.used_names.insert(ph.to_string());
                self.advance();
                Ok(resolved)
            }
            _ => Err(self.syntax_error()),
        }
    }

    fn path(&mut self) -> Result<DocPath, AwsServiceError> {
        let mut out = vec![PathElem::Attr(self.path_name()?)];
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.advance();
                    out.push(PathElem::Attr(self.path_name()?));
                }
                Tok::LBracket => {
                    self.advance();
                    if *self.peek() != Tok::Num {
                        return Err(self.syntax_error());
                    }
                    let idx: usize = self.cur_text().parse().map_err(|_| self.syntax_error())?;
                    self.advance();
                    self.expect(Tok::RBracket)?;
                    out.push(PathElem::Index(idx));
                }
                _ => return Ok(out),
            }
        }
    }

    fn value_ref(&mut self) -> Result<String, AwsServiceError> {
        let ph = self.cur_text().to_string();
        if self.ctx.strict && !self.ctx.values.contains_key(&ph) {
            return Err(invalid(
                self.kind,
                format!(
                    "An expression attribute value used in expression is not defined; \
                     attribute value: {ph}"
                ),
            ));
        }
        self.ctx.used_values.insert(ph.clone());
        self.advance();
        Ok(ph)
    }

    // -- conditions -------------------------------------------------------

    fn condition(&mut self) -> Result<Cond, AwsServiceError> {
        let c = self.or_expr()?;
        if *self.peek() != Tok::Eof {
            return Err(self.syntax_error());
        }
        Ok(c)
    }

    fn or_expr(&mut self) -> Result<Cond, AwsServiceError> {
        let mut left = self.and_expr()?;
        while self.is_keyword("OR") {
            self.advance();
            let right = self.and_expr()?;
            left = Cond::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Cond, AwsServiceError> {
        let mut left = self.not_expr()?;
        while self.is_keyword("AND") {
            self.advance();
            let right = self.not_expr()?;
            left = Cond::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Cond, AwsServiceError> {
        if self.is_keyword("NOT") {
            self.advance();
            return Ok(Cond::Not(Box::new(self.not_expr()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Cond, AwsServiceError> {
        if *self.peek() == Tok::LParen {
            self.advance();
            let inner = self.or_expr()?;
            self.expect(Tok::RParen)?;
            return Ok(Cond::Paren(Box::new(inner)));
        }
        if self.at_function() {
            let name = self.cur_text();
            match name {
                "attribute_exists"
                | "attribute_not_exists"
                | "attribute_type"
                | "begins_with"
                | "contains" => return self.condition_function(),
                "size" => {}
                _ => return Err(self.unknown_function(name)),
            }
        }
        let left = self.operand()?;
        let op = match self.peek() {
            Tok::Eq => Some(CmpOp::Eq),
            Tok::Ne => Some(CmpOp::Ne),
            Tok::Lt => Some(CmpOp::Lt),
            Tok::Le => Some(CmpOp::Le),
            Tok::Gt => Some(CmpOp::Gt),
            Tok::Ge => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let right = self.operand()?;
            return Ok(Cond::Compare(left, op, right));
        }
        if self.is_keyword("BETWEEN") {
            self.advance();
            let lo = self.operand()?;
            if !self.is_keyword("AND") {
                return Err(self.syntax_error());
            }
            self.advance();
            let hi = self.operand()?;
            return Ok(Cond::Between(left, lo, hi));
        }
        if self.is_keyword("IN") {
            self.advance();
            self.expect(Tok::LParen)?;
            let mut list = vec![self.operand()?];
            while *self.peek() == Tok::Comma {
                self.advance();
                list.push(self.operand()?);
            }
            self.expect(Tok::RParen)?;
            return Ok(Cond::In(left, list));
        }
        Err(self.syntax_error())
    }

    fn unknown_function(&self, name: &str) -> AwsServiceError {
        invalid(
            self.kind,
            format!("Invalid function name; function: {name}"),
        )
    }

    /// Parse `name(operand, ...)` returning the name and its operands.
    fn call_args(&mut self) -> Result<(String, Vec<Operand>), AwsServiceError> {
        let name = self.cur_text().to_string();
        self.advance();
        self.expect(Tok::LParen)?;
        let mut args = vec![self.operand()?];
        while *self.peek() == Tok::Comma {
            self.advance();
            args.push(self.operand()?);
        }
        self.expect(Tok::RParen)?;
        Ok((name, args))
    }

    fn condition_function(&mut self) -> Result<Cond, AwsServiceError> {
        let (name, mut args) = self.call_args()?;
        let expected = match name.as_str() {
            "attribute_exists" | "attribute_not_exists" => 1,
            _ => 2,
        };
        if args.len() != expected {
            return Err(invalid(
                self.kind,
                format!(
                    "Incorrect number of operands for operator or function; operator or \
                     function: {name}, number of operands: {}",
                    args.len()
                ),
            ));
        }
        let requires_path = |op: &Operand| -> Result<DocPath, AwsServiceError> {
            match op {
                Operand::Path(p) => Ok(p.clone()),
                _ => Err(invalid(
                    self.kind,
                    format!(
                        "Operator or function requires a document path; operator or function: \
                         {name}"
                    ),
                )),
            }
        };
        Ok(match name.as_str() {
            "attribute_exists" => Cond::Exists(requires_path(&args[0])?),
            "attribute_not_exists" => Cond::NotExists(requires_path(&args[0])?),
            "attribute_type" => {
                let path = requires_path(&args[0])?;
                Cond::AttrType(path, args.remove(1))
            }
            "begins_with" => {
                let b = args.remove(1);
                Cond::BeginsWith(args.remove(0), b)
            }
            _ => {
                let b = args.remove(1);
                Cond::Contains(args.remove(0), b)
            }
        })
    }

    fn operand(&mut self) -> Result<Operand, AwsServiceError> {
        match self.peek() {
            Tok::Value => Ok(Operand::Value(self.value_ref()?)),
            Tok::Ident if self.at_function() => {
                let name = self.cur_text();
                match name {
                    "size" => {
                        let (_, args) = self.call_args()?;
                        if args.len() != 1 {
                            return Err(invalid(
                                self.kind,
                                format!(
                                    "Incorrect number of operands for operator or function; \
                                     operator or function: size, number of operands: {}",
                                    args.len()
                                ),
                            ));
                        }
                        match args.into_iter().next() {
                            Some(Operand::Path(p)) => Ok(Operand::Size(p)),
                            _ => Err(invalid(
                                self.kind,
                                "Operator or function requires a document path; operator or \
                                 function: size",
                            )),
                        }
                    }
                    "attribute_exists"
                    | "attribute_not_exists"
                    | "attribute_type"
                    | "begins_with"
                    | "contains"
                    | "if_not_exists"
                    | "list_append" => Err(invalid(
                        self.kind,
                        format!(
                            "The function is not allowed to be used this way in an \
                                 expression; function: {name}"
                        ),
                    )),
                    _ => Err(self.unknown_function(name)),
                }
            }
            Tok::Ident | Tok::Name => Ok(Operand::Path(self.path()?)),
            _ => Err(self.syntax_error()),
        }
    }

    // -- projection -------------------------------------------------------

    fn projection(&mut self) -> Result<Vec<DocPath>, AwsServiceError> {
        let mut out = vec![self.path()?];
        while *self.peek() == Tok::Comma {
            self.advance();
            out.push(self.path()?);
        }
        if *self.peek() != Tok::Eof {
            return Err(self.syntax_error());
        }
        Ok(out)
    }

    // -- update -----------------------------------------------------------

    fn update(&mut self) -> Result<UpdateAst, AwsServiceError> {
        let mut ast = UpdateAst::default();
        let mut seen: Vec<&'static str> = Vec::new();
        loop {
            let section = if self.is_keyword("SET") {
                "SET"
            } else if self.is_keyword("REMOVE") {
                "REMOVE"
            } else if self.is_keyword("ADD") {
                "ADD"
            } else if self.is_keyword("DELETE") {
                "DELETE"
            } else {
                return Err(self.syntax_error());
            };
            if seen.contains(&section) {
                return Err(invalid(
                    self.kind,
                    format!(
                        "The \"{section}\" section can only be used once in an update expression;"
                    ),
                ));
            }
            seen.push(section);
            self.advance();
            loop {
                let path = self.path()?;
                match section {
                    "SET" => {
                        self.expect(Tok::Eq)?;
                        let v = self.set_value()?;
                        ast.sets.push((path, v));
                    }
                    "REMOVE" => ast.removes.push(path),
                    _ => {
                        if *self.peek() != Tok::Value {
                            return Err(self.syntax_error());
                        }
                        let v = self.value_ref()?;
                        if section == "ADD" {
                            ast.adds.push((path, v));
                        } else {
                            ast.deletes.push((path, v));
                        }
                    }
                }
                if *self.peek() == Tok::Comma {
                    self.advance();
                } else {
                    break;
                }
            }
            if *self.peek() == Tok::Eof {
                return Ok(ast);
            }
        }
    }

    fn set_value(&mut self) -> Result<SetValue, AwsServiceError> {
        let left = self.set_operand()?;
        if !matches!(self.peek(), Tok::Plus | Tok::Minus) {
            return Ok(left);
        }
        self.advance();
        let right = self.set_operand()?;
        Ok(SetValue::Arith(Box::new(left), Box::new(right)))
    }

    fn set_operand(&mut self) -> Result<SetValue, AwsServiceError> {
        match self.peek() {
            Tok::LParen => {
                self.advance();
                let inner = self.set_value()?;
                self.expect(Tok::RParen)?;
                Ok(SetValue::Paren(Box::new(inner)))
            }
            Tok::Value => Ok(SetValue::Operand(Operand::Value(self.value_ref()?))),
            Tok::Ident if self.at_function() => {
                let name = self.cur_text();
                match name {
                    "if_not_exists" | "list_append" => {
                        let name = name.to_string();
                        self.advance();
                        self.expect(Tok::LParen)?;
                        let mut args = vec![self.set_value()?];
                        while *self.peek() == Tok::Comma {
                            self.advance();
                            args.push(self.set_value()?);
                        }
                        self.expect(Tok::RParen)?;
                        if args.len() != 2 {
                            return Err(invalid(
                                self.kind,
                                format!(
                                    "Incorrect number of operands for operator or function; \
                                     operator or function: {name}, number of operands: {}",
                                    args.len()
                                ),
                            ));
                        }
                        let b = args.pop().expect("two args");
                        let a = args.pop().expect("two args");
                        if name == "if_not_exists" {
                            match a {
                                SetValue::Operand(Operand::Path(_)) => {}
                                _ => {
                                    return Err(invalid(
                                        self.kind,
                                        "Operator or function requires a document path; \
                                         operator or function: if_not_exists",
                                    ))
                                }
                            }
                            Ok(SetValue::IfNotExists(Box::new(b)))
                        } else {
                            Ok(SetValue::ListAppend(Box::new(a), Box::new(b)))
                        }
                    }
                    "attribute_exists"
                    | "attribute_not_exists"
                    | "attribute_type"
                    | "begins_with"
                    | "contains"
                    | "size" => Err(invalid(
                        self.kind,
                        format!(
                            "The function is not allowed in an update expression; function: \
                             {name}"
                        ),
                    )),
                    _ => Err(self.unknown_function(name)),
                }
            }
            Tok::Ident | Tok::Name => Ok(SetValue::Operand(Operand::Path(self.path()?))),
            _ => Err(self.syntax_error()),
        }
    }
}

// ---------------------------------------------------------------------------
// Public parse entry points
// ---------------------------------------------------------------------------

fn check_size_and_empty(kind: ExprKind, src: &str) -> Result<(), AwsServiceError> {
    if src.trim().is_empty() {
        return Err(invalid(kind, "The expression can not be empty;"));
    }
    if src.len() > MAX_EXPRESSION_BYTES {
        return Err(invalid(
            kind,
            format!(
                "Expression size has exceeded the maximum allowed size; expression size: {}",
                src.len()
            ),
        ));
    }
    Ok(())
}

/// Parse and validate a condition-grammar expression (Condition, Filter or
/// KeyCondition).
pub(crate) fn parse_condition_expression(
    src: &str,
    kind: ExprKind,
    ctx: &mut ExprContext<'_>,
) -> Result<Cond, AwsServiceError> {
    if ctx.strict {
        check_size_and_empty(kind, src)?;
    }
    let cond = Parser::new(src, kind, ctx).condition()?;
    if ctx.strict {
        check_redundant_parens(&cond, kind)?;
        check_condition_semantics(&cond, kind, ctx.values)?;
        if kind == ExprKind::KeyCondition {
            check_key_condition_shape(&cond)?;
        }
    }
    Ok(cond)
}

/// Parse and validate a ProjectionExpression, rejecting overlapping paths.
pub(crate) fn parse_projection_expression(
    src: &str,
    ctx: &mut ExprContext<'_>,
) -> Result<Vec<DocPath>, AwsServiceError> {
    let kind = ExprKind::Projection;
    if ctx.strict {
        check_size_and_empty(kind, src)?;
    }
    let paths = Parser::new(src, kind, ctx).projection()?;
    check_overlap(&paths, kind)?;
    Ok(paths)
}

/// Parse and validate an UpdateExpression.
pub(crate) fn parse_update_expression(
    src: &str,
    ctx: &mut ExprContext<'_>,
) -> Result<UpdateAst, AwsServiceError> {
    let kind = ExprKind::Update;
    if ctx.strict {
        check_size_and_empty(kind, src)?;
    }
    let ast = Parser::new(src, kind, ctx).update()?;
    for (_, v) in &ast.sets {
        check_set_value_parens(v, false)?;
    }
    check_overlap(&ast.target_paths(), kind)?;
    Ok(ast)
}

fn overlap_error(kind: ExprKind, a: &[PathElem], b: &[PathElem]) -> AwsServiceError {
    invalid(
        kind,
        format!(
            "Two document paths overlap with each other; must remove or rewrite one of these \
             paths; path one: {}, path two: {}",
            render_path(a),
            render_path(b)
        ),
    )
}

fn check_overlap(paths: &[DocPath], kind: ExprKind) -> Result<(), AwsServiceError> {
    for (i, a) in paths.iter().enumerate() {
        for b in &paths[i + 1..] {
            if is_prefix(a, b) || is_prefix(b, a) {
                return Err(overlap_error(kind, a, b));
            }
        }
    }
    Ok(())
}

fn redundant(kind: ExprKind) -> AwsServiceError {
    invalid(kind, "The expression has redundant parentheses;")
}

fn check_redundant_parens(c: &Cond, kind: ExprKind) -> Result<(), AwsServiceError> {
    match c {
        Cond::Paren(inner) => {
            if matches!(**inner, Cond::Paren(_)) {
                return Err(redundant(kind));
            }
            check_redundant_parens(inner, kind)
        }
        Cond::And(a, b) | Cond::Or(a, b) => {
            check_redundant_parens(a, kind)?;
            check_redundant_parens(b, kind)
        }
        Cond::Not(a) => check_redundant_parens(a, kind),
        _ => Ok(()),
    }
}

fn check_set_value_parens(v: &SetValue, parent_paren: bool) -> Result<(), AwsServiceError> {
    match v {
        SetValue::Paren(inner) => {
            if parent_paren {
                return Err(redundant(ExprKind::Update));
            }
            check_set_value_parens(inner, true)
        }
        SetValue::Arith(a, b) | SetValue::ListAppend(a, b) => {
            check_set_value_parens(a, false)?;
            check_set_value_parens(b, false)
        }
        SetValue::IfNotExists(b) => check_set_value_parens(b, false),
        SetValue::Operand(_) => Ok(()),
    }
}

/// The DynamoDB type tag of an AttributeValue (`S`, `N`, `BOOL`, ...).
pub(crate) fn value_type(v: &Value) -> Option<&str> {
    v.as_object()
        .and_then(|o| o.keys().next())
        .map(String::as_str)
}

fn literal_type<'v>(op: &Operand, values: &'v HashMap<String, Value>) -> Option<&'v str> {
    match op {
        Operand::Value(ph) => values.get(ph).and_then(value_type),
        _ => None,
    }
}

fn incorrect_operand_type(kind: ExprKind, op: &str, ty: &str) -> AwsServiceError {
    invalid(
        kind,
        format!(
            "Incorrect operand type for operator or function; operator or function: {op}, \
             operand type: {ty}"
        ),
    )
}

/// Render a literal the way DynamoDB echoes it in BETWEEN errors:
/// `AttributeValue: {N:10}`.
fn render_literal(v: &Value) -> String {
    let ty = value_type(v).unwrap_or("");
    let inner = v.get(ty).map(|x| match x {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    format!("AttributeValue: {{{ty}:{}}}", inner.unwrap_or_default())
}

fn check_condition_semantics(
    c: &Cond,
    kind: ExprKind,
    values: &HashMap<String, Value>,
) -> Result<(), AwsServiceError> {
    let orderable = |t: &str| matches!(t, "S" | "N" | "B");
    match c {
        Cond::And(a, b) | Cond::Or(a, b) => {
            check_condition_semantics(a, kind, values)?;
            check_condition_semantics(b, kind, values)
        }
        Cond::Not(a) | Cond::Paren(a) => check_condition_semantics(a, kind, values),
        Cond::Compare(l, op, r) => {
            if !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                for side in [l, r] {
                    if let Some(t) = literal_type(side, values) {
                        if !orderable(t) {
                            return Err(incorrect_operand_type(kind, op.symbol(), t));
                        }
                    }
                }
            }
            Ok(())
        }
        Cond::Between(x, lo, hi) => {
            for side in [x, lo, hi] {
                if let Some(t) = literal_type(side, values) {
                    if !orderable(t) {
                        return Err(incorrect_operand_type(kind, "BETWEEN", t));
                    }
                }
            }
            if let (Operand::Value(lp), Operand::Value(hp)) = (lo, hi) {
                if let (Some(lv), Some(hv)) = (values.get(lp), values.get(hp)) {
                    if value_type(lv) != value_type(hv) {
                        return Err(invalid(
                            kind,
                            format!(
                                "The BETWEEN operator requires same data type for lower and \
                                 upper bounds; lower bound operand: {}, upper bound operand: {}",
                                render_literal(lv),
                                render_literal(hv)
                            ),
                        ));
                    }
                    if compare_attribute_values(Some(lv), Some(hv)) == std::cmp::Ordering::Greater {
                        return Err(invalid(
                            kind,
                            format!(
                                "The BETWEEN operator requires upper bound to be greater than or \
                                 equal to lower bound; lower bound operand: {}, upper bound \
                                 operand: {}",
                                render_literal(lv),
                                render_literal(hv)
                            ),
                        ));
                    }
                }
            }
            Ok(())
        }
        Cond::BeginsWith(a, b) => {
            for side in [a, b] {
                if let Some(t) = literal_type(side, values) {
                    if !matches!(t, "S" | "B") {
                        return Err(incorrect_operand_type(kind, "begins_with", t));
                    }
                }
            }
            Ok(())
        }
        Cond::Contains(a, b) => {
            if let (Operand::Path(pa), Operand::Path(pb)) = (a, b) {
                if pa == pb {
                    return Err(invalid(
                        kind,
                        format!(
                            "The first operand must be distinct from the remaining operands for \
                             this operator or function; operator: contains, first operand: {}",
                            render_path(pa)
                        ),
                    ));
                }
            }
            Ok(())
        }
        Cond::AttrType(_, t) => {
            if let Some(ty) = literal_type(t, values) {
                if ty != "S" {
                    return Err(incorrect_operand_type(kind, "attribute_type", ty));
                }
            }
            if let Operand::Value(ph) = t {
                if let Some(name) = values
                    .get(ph)
                    .and_then(|v| v.get("S"))
                    .and_then(Value::as_str)
                {
                    const TYPES: &[&str] =
                        &["B", "BOOL", "BS", "L", "M", "N", "NS", "NULL", "S", "SS"];
                    if !TYPES.contains(&name) {
                        return Err(invalid(
                            kind,
                            format!(
                                "Invalid attribute type name found; type: {name}, valid types: \
                                 {{ B; BOOL; BS; L; M; N; NS; NULL; S; SS }}"
                            ),
                        ));
                    }
                }
            }
            Ok(())
        }
        Cond::In(..) | Cond::Exists(_) | Cond::NotExists(_) => Ok(()),
    }
}

/// KeyConditionExpression only supports AND-ed key comparisons, BETWEEN and
/// begins_with, each on a top-level key attribute.
fn check_key_condition_shape(c: &Cond) -> Result<(), AwsServiceError> {
    let bad_op = |op: &str| {
        validation(format!(
            "Invalid operator used in KeyConditionExpression: {op}"
        ))
    };
    let nested =
        || validation("KeyConditionExpressions cannot have conditions on nested attributes");
    let check_operand = |o: &Operand| -> Result<(), AwsServiceError> {
        match o {
            Operand::Path(p) if p.len() > 1 => Err(nested()),
            Operand::Size(_) => Err(bad_op("size")),
            _ => Ok(()),
        }
    };
    match c {
        Cond::And(a, b) => {
            check_key_condition_shape(a)?;
            check_key_condition_shape(b)
        }
        Cond::Paren(a) => check_key_condition_shape(a),
        Cond::Or(..) => Err(bad_op("OR")),
        Cond::Not(_) => Err(bad_op("NOT")),
        Cond::In(..) => Err(bad_op("IN")),
        Cond::Exists(_) => Err(bad_op("attribute_exists")),
        Cond::NotExists(_) => Err(bad_op("attribute_not_exists")),
        Cond::AttrType(..) => Err(bad_op("attribute_type")),
        Cond::Contains(..) => Err(bad_op("contains")),
        Cond::Compare(l, op, r) => {
            if *op == CmpOp::Ne {
                return Err(bad_op("<>"));
            }
            check_operand(l)?;
            check_operand(r)
        }
        Cond::Between(x, lo, hi) => {
            check_operand(x)?;
            check_operand(lo)?;
            check_operand(hi)
        }
        Cond::BeginsWith(a, b) => {
            check_operand(a)?;
            check_operand(b)
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Resolve a document path against an item.
pub(crate) fn resolve_doc_path<'i>(
    item: &'i HashMap<String, AttributeValue>,
    path: &[PathElem],
) -> Option<&'i Value> {
    let (first, rest) = path.split_first()?;
    let PathElem::Attr(name) = first else {
        return None;
    };
    let mut cur = item.get(name)?;
    for elem in rest {
        cur = match elem {
            PathElem::Attr(k) => cur.get("M")?.get(k)?,
            PathElem::Index(i) => cur.get("L")?.get(*i)?,
        };
    }
    Some(cur)
}

fn resolve_operand(
    op: &Operand,
    item: &HashMap<String, AttributeValue>,
    values: &HashMap<String, Value>,
) -> Option<Value> {
    match op {
        Operand::Path(p) => resolve_doc_path(item, p).cloned(),
        Operand::Value(ph) => values.get(ph).cloned(),
        Operand::Size(p) => resolve_doc_path(item, p)
            .and_then(attribute_size)
            .map(|n| json!({ "N": n.to_string() })),
    }
}

/// DynamoDB equality between two AttributeValues: numbers compare by value,
/// sets ignore element order, lists and maps compare element-wise.
pub(crate) fn attribute_values_equal(a: &Value, b: &Value) -> bool {
    let (Some(ta), Some(tb)) = (value_type(a), value_type(b)) else {
        return a == b;
    };
    if ta != tb {
        return false;
    }
    let (va, vb) = (&a[ta], &b[tb]);
    match ta {
        "N" => values_equal(Some(a), Some(b)),
        "SS" | "BS" | "NS" => {
            let (Some(xa), Some(xb)) = (va.as_array(), vb.as_array()) else {
                return false;
            };
            let same = |x: &Value, y: &Value| {
                if ta == "NS" {
                    ns_members_equal(x, y)
                } else {
                    x == y
                }
            };
            xa.len() == xb.len() && xa.iter().all(|x| xb.iter().any(|y| same(x, y)))
        }
        "L" => {
            let (Some(xa), Some(xb)) = (va.as_array(), vb.as_array()) else {
                return false;
            };
            xa.len() == xb.len() && xa.iter().zip(xb).all(|(x, y)| attribute_values_equal(x, y))
        }
        "M" => {
            let (Some(xa), Some(xb)) = (va.as_object(), vb.as_object()) else {
                return false;
            };
            xa.len() == xb.len()
                && xa
                    .iter()
                    .all(|(k, x)| xb.get(k).is_some_and(|y| attribute_values_equal(x, y)))
        }
        _ => a == b,
    }
}

fn contains_value(container: &Value, needle: &Value) -> bool {
    if let (Some(a), Some(e)) = (
        container.get("S").and_then(Value::as_str),
        needle.get("S").and_then(Value::as_str),
    ) {
        return a.contains(e);
    }
    if let (Some(a), Some(e)) = (
        container.get("B").and_then(Value::as_str),
        needle.get("B").and_then(Value::as_str),
    ) {
        let dec = |s: &str| base64::engine::general_purpose::STANDARD.decode(s).ok();
        return match (dec(a), dec(e)) {
            (Some(a), Some(e)) => e.is_empty() || a.windows(e.len()).any(|w| w == e.as_slice()),
            _ => false,
        };
    }
    for (set, scalar) in [("SS", "S"), ("NS", "N"), ("BS", "B")] {
        if let Some(members) = container.get(set).and_then(Value::as_array) {
            let Some(val) = needle.get(scalar) else {
                return false;
            };
            return members.iter().any(|m| {
                if set == "NS" {
                    ns_members_equal(m, val)
                } else {
                    m == val
                }
            });
        }
    }
    if let Some(list) = container.get("L").and_then(Value::as_array) {
        return list.iter().any(|el| attribute_values_equal(el, needle));
    }
    false
}

/// Evaluate a parsed condition against an item (an empty map models a
/// missing item).
pub(crate) fn eval_cond(
    c: &Cond,
    item: &HashMap<String, AttributeValue>,
    values: &HashMap<String, Value>,
) -> bool {
    match c {
        Cond::And(a, b) => eval_cond(a, item, values) && eval_cond(b, item, values),
        Cond::Or(a, b) => eval_cond(a, item, values) || eval_cond(b, item, values),
        Cond::Not(a) => !eval_cond(a, item, values),
        Cond::Paren(a) => eval_cond(a, item, values),
        Cond::Exists(p) => resolve_doc_path(item, p).is_some(),
        Cond::NotExists(p) => resolve_doc_path(item, p).is_none(),
        Cond::AttrType(p, t) => {
            let actual = resolve_doc_path(item, p).and_then(value_type);
            let wanted = resolve_operand(t, item, values);
            let wanted = wanted
                .as_ref()
                .and_then(|v| v.get("S"))
                .and_then(Value::as_str);
            matches!((actual, wanted), (Some(a), Some(w)) if a == w)
        }
        Cond::BeginsWith(a, b) => {
            match (
                resolve_operand(a, item, values),
                resolve_operand(b, item, values),
            ) {
                (Some(a), Some(b)) => attribute_begins_with(&a, &b),
                _ => false,
            }
        }
        Cond::Contains(a, b) => {
            match (
                resolve_operand(a, item, values),
                resolve_operand(b, item, values),
            ) {
                (Some(a), Some(b)) => contains_value(&a, &b),
                _ => false,
            }
        }
        Cond::In(x, list) => {
            let Some(x) = resolve_operand(x, item, values) else {
                return false;
            };
            list.iter().any(|o| {
                resolve_operand(o, item, values).is_some_and(|v| attribute_values_equal(&x, &v))
            })
        }
        Cond::Between(x, lo, hi) => {
            let (Some(x), Some(lo), Some(hi)) = (
                resolve_operand(x, item, values),
                resolve_operand(lo, item, values),
                resolve_operand(hi, item, values),
            ) else {
                return false;
            };
            comparable_types(Some(&x), Some(&lo))
                && comparable_types(Some(&x), Some(&hi))
                && compare_attribute_values(Some(&x), Some(&lo)) != std::cmp::Ordering::Less
                && compare_attribute_values(Some(&x), Some(&hi)) != std::cmp::Ordering::Greater
        }
        Cond::Compare(l, op, r) => {
            let a = resolve_operand(l, item, values);
            let b = resolve_operand(r, item, values);
            match op {
                CmpOp::Eq => matches!((&a, &b), (Some(a), Some(b)) if attribute_values_equal(a, b)),
                // A missing operand is not equal to anything, so `<>` against
                // an absent attribute is true.
                CmpOp::Ne => match (&a, &b) {
                    (Some(a), Some(b)) => !attribute_values_equal(a, b),
                    (None, None) => false,
                    _ => true,
                },
                _ => {
                    if !comparable_types(a.as_ref(), b.as_ref()) {
                        return false;
                    }
                    let ord = compare_attribute_values(a.as_ref(), b.as_ref());
                    match op {
                        CmpOp::Lt => ord == std::cmp::Ordering::Less,
                        CmpOp::Le => ord != std::cmp::Ordering::Greater,
                        CmpOp::Gt => ord == std::cmp::Ordering::Greater,
                        _ => ord != std::cmp::Ordering::Less,
                    }
                }
            }
        }
    }
}

/// Parse a single document path (`a`, `#a.b`, `l[0][1]`, `a.m[0][2]`) with the
/// same segmentation every expression uses, resolving `#name` placeholders
/// through `names`. Used by the update machinery to locate SET/REMOVE targets.
pub(crate) fn parse_document_path(src: &str, names: &HashMap<String, String>) -> Option<DocPath> {
    let values = HashMap::new();
    let mut ctx = ExprContext::new(names, &values, false);
    let mut parser = Parser::new(src, ExprKind::Update, &mut ctx);
    let path = parser.path().ok()?;
    (*parser.peek() == Tok::Eof).then_some(path)
}

/// Parse `expr` leniently (placeholders already validated or synthesized)
/// for evaluation. Returns `None` when the text is outside the grammar.
pub(crate) fn parse_condition_lenient(
    expr: &str,
    names: &HashMap<String, String>,
    values: &HashMap<String, Value>,
) -> Option<Cond> {
    let mut ctx = ExprContext::new(names, values, false);
    parse_condition_expression(expr, ExprKind::Filter, &mut ctx).ok()
}

// ---------------------------------------------------------------------------
// Request-level validation
// ---------------------------------------------------------------------------

/// The item operations whose expression parameters are validated up front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExprOp {
    GetItem,
    PutItem,
    DeleteItem,
    UpdateItem,
    Query,
    Scan,
}

impl ExprOp {
    /// Legacy (non-expression) parameters, in AWS's reporting order.
    fn legacy_params(self) -> &'static [&'static str] {
        match self {
            ExprOp::GetItem => &["AttributesToGet"],
            ExprOp::PutItem | ExprOp::DeleteItem => &["Expected", "ConditionalOperator"],
            ExprOp::UpdateItem => &["AttributeUpdates", "Expected", "ConditionalOperator"],
            ExprOp::Query => &[
                "AttributesToGet",
                "KeyConditions",
                "QueryFilter",
                "ConditionalOperator",
            ],
            ExprOp::Scan => &["AttributesToGet", "ScanFilter", "ConditionalOperator"],
        }
    }

    /// Expression parameters, in the order they are parsed.
    fn expression_params(self) -> &'static [(&'static str, ExprKind)] {
        match self {
            ExprOp::GetItem => &[("ProjectionExpression", ExprKind::Projection)],
            ExprOp::PutItem | ExprOp::DeleteItem => &[("ConditionExpression", ExprKind::Condition)],
            ExprOp::UpdateItem => &[
                ("UpdateExpression", ExprKind::Update),
                ("ConditionExpression", ExprKind::Condition),
            ],
            ExprOp::Query => &[
                ("KeyConditionExpression", ExprKind::KeyCondition),
                ("FilterExpression", ExprKind::Filter),
                ("ProjectionExpression", ExprKind::Projection),
            ],
            ExprOp::Scan => &[
                ("FilterExpression", ExprKind::Filter),
                ("ProjectionExpression", ExprKind::Projection),
            ],
        }
    }
}

/// The expressions of a request, parsed and validated.
#[derive(Debug, Default)]
pub(crate) struct ParsedExpressions {
    pub(crate) condition: Option<Cond>,
    pub(crate) filter: Option<Cond>,
    pub(crate) key_condition: Option<Cond>,
    pub(crate) projection: Option<Vec<DocPath>>,
    pub(crate) update: Option<UpdateAst>,
}

fn present(body: &Value, field: &str) -> bool {
    !body[field].is_null()
}

fn placeholder_key_ok(key: &str, prefix: char) -> bool {
    let mut chars = key.chars();
    chars.next() == Some(prefix) && {
        let rest = chars.as_str();
        !rest.is_empty() && rest.bytes().all(is_word_byte)
    }
}

/// Depth of an AttributeValue: 1 for a scalar, plus one per enclosing M/L.
pub(crate) fn attribute_depth(v: &Value) -> usize {
    let children: Box<dyn Iterator<Item = &Value>> =
        if let Some(l) = v.get("L").and_then(Value::as_array) {
            Box::new(l.iter())
        } else if let Some(m) = v.get("M").and_then(Value::as_object) {
            Box::new(m.values())
        } else {
            return 1;
        };
    1 + children.map(attribute_depth).max().unwrap_or(0)
}

/// DynamoDB documents nest at most 32 levels deep.
pub(crate) const MAX_NESTING_DEPTH: usize = 32;

/// Validate every expression parameter of an item-level request, in the
/// order real DynamoDB does, before the table is looked up.
pub(crate) fn validate_request_expressions(
    body: &Value,
    op: ExprOp,
) -> Result<ParsedExpressions, AwsServiceError> {
    let legacy: Vec<&str> = op
        .legacy_params()
        .iter()
        .copied()
        .filter(|p| present(body, p))
        .collect();
    let exprs: Vec<&str> = op
        .expression_params()
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| present(body, p))
        .collect();
    if !legacy.is_empty() && !exprs.is_empty() {
        return Err(validation(format!(
            "Can not use both expression and non-expression parameters in the same request: \
             Non-expression parameters: {{{}}} Expression parameters: {{{}}}",
            legacy.join(", "),
            exprs.join(", ")
        )));
    }

    for field in ["ExpressionAttributeNames", "ExpressionAttributeValues"] {
        if present(body, field) && exprs.is_empty() {
            return Err(validation(format!(
                "{field} can only be specified when using expressions"
            )));
        }
    }
    for field in ["ExpressionAttributeNames", "ExpressionAttributeValues"] {
        if let Some(map) = body[field].as_object() {
            if map.is_empty() {
                return Err(validation(format!("{field} must not be empty")));
            }
            let prefix = if field == "ExpressionAttributeNames" {
                '#'
            } else {
                ':'
            };
            for key in map.keys() {
                if !placeholder_key_ok(key, prefix) {
                    return Err(validation(format!(
                        "{field} contains invalid key: Syntax error; key: \"{key}\""
                    )));
                }
            }
        }
    }

    let names = parse_expression_attribute_names(body);
    let values = parse_expression_attribute_values(body);
    for (k, v) in body["ExpressionAttributeValues"]
        .as_object()
        .into_iter()
        .flatten()
    {
        if attribute_depth(v) > MAX_NESTING_DEPTH {
            return Err(validation(format!(
                "ExpressionAttributeValues contains invalid value: Nesting Levels have exceeded \
                 supported limits for key {k}"
            )));
        }
    }

    let mut ctx = ExprContext::new(&names, &values, true);
    let mut parsed = ParsedExpressions::default();
    for (param, kind) in op.expression_params() {
        let Some(src) = body[*param].as_str() else {
            continue;
        };
        match kind {
            ExprKind::Projection => {
                parsed.projection = Some(parse_projection_expression(src, &mut ctx)?)
            }
            ExprKind::Update => parsed.update = Some(parse_update_expression(src, &mut ctx)?),
            ExprKind::Condition => {
                parsed.condition = Some(parse_condition_expression(src, *kind, &mut ctx)?)
            }
            ExprKind::Filter => {
                parsed.filter = Some(parse_condition_expression(src, *kind, &mut ctx)?)
            }
            ExprKind::KeyCondition => {
                parsed.key_condition = Some(parse_condition_expression(src, *kind, &mut ctx)?)
            }
        }
    }

    let unused = |declared: Vec<&String>, used: &std::collections::HashSet<String>| {
        let mut keys: Vec<String> = declared
            .into_iter()
            .filter(|k| !used.contains(*k))
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    let unused_names = unused(names.keys().collect(), &ctx.used_names);
    if !unused_names.is_empty() {
        return Err(validation(format!(
            "Value provided in ExpressionAttributeNames unused in expressions: keys: {{{}}}",
            unused_names.join(", ")
        )));
    }
    let unused_values = unused(values.keys().collect(), &ctx.used_values);
    if !unused_values.is_empty() {
        return Err(validation(format!(
            "Value provided in ExpressionAttributeValues unused in expressions: keys: {{{}}}",
            unused_values.join(", ")
        )));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(body: Value, op: ExprOp) -> Result<ParsedExpressions, String> {
        validate_request_expressions(&body, op).map_err(|e| e.message().to_string())
    }

    #[test]
    fn syntax_error_reports_token_and_neighbourhood() {
        let err = validate(
            json!({"ProjectionExpression": "!!! INVALID !!!"}),
            ExprOp::GetItem,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Invalid ProjectionExpression: Syntax error; token: \"!\", near: \"!!\""
        );
        let err = validate(
            json!({"UpdateExpression": "INVALID SYNTAX HERE"}),
            ExprOp::UpdateItem,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Invalid UpdateExpression: Syntax error; token: \"INVALID\", near: \"INVALID SYNTAX\""
        );
    }

    #[test]
    fn strict_mode_rejects_double_quote_as_single_character() {
        let err =
            validate(json!({"ProjectionExpression": "\"a b\""}), ExprOp::GetItem).unwrap_err();
        assert_eq!(
            err,
            "Invalid ProjectionExpression: Syntax error; token: \"\"\", near: \"\"a\""
        );
    }

    #[test]
    fn redundant_parentheses_rejected_single_wrap_accepted() {
        let err = validate(
            json!({"ConditionExpression": "((attribute_not_exists(pk)))"}),
            ExprOp::PutItem,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Invalid ConditionExpression: The expression has redundant parentheses;"
        );
        assert!(validate(
            json!({
                "ConditionExpression": "(attribute_not_exists(pk) AND (#s = :v))",
                "ExpressionAttributeNames": {"#s": "s"},
                "ExpressionAttributeValues": {":v": {"S": "x"}},
            }),
            ExprOp::PutItem,
        )
        .is_ok());
        let err = validate(
            json!({
                "UpdateExpression": "SET c = ((c - :v))",
                "ExpressionAttributeValues": {":v": {"N": "1"}},
            }),
            ExprOp::UpdateItem,
        )
        .unwrap_err();
        assert!(err.contains("redundant parentheses"), "{err}");
    }

    #[test]
    fn reserved_words_need_placeholders() {
        let err = validate(
            json!({
                "UpdateExpression": "SET status = :v",
                "ExpressionAttributeValues": {":v": {"S": "x"}},
            }),
            ExprOp::UpdateItem,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Invalid UpdateExpression: Attribute name is a reserved keyword; reserved keyword: status"
        );
    }

    #[test]
    fn placeholder_hygiene() {
        let unused = validate(
            json!({
                "UpdateExpression": "SET attr1 = :v",
                "ExpressionAttributeValues": {":v": {"S": "x"}, ":unused": {"S": "y"}},
            }),
            ExprOp::UpdateItem,
        )
        .unwrap_err();
        assert_eq!(
            unused,
            "Value provided in ExpressionAttributeValues unused in expressions: keys: {:unused}"
        );
        let undefined = validate(
            json!({"UpdateExpression": "SET attr1 = :v"}),
            ExprOp::UpdateItem,
        )
        .unwrap_err();
        assert!(undefined.contains("attribute value: :v"), "{undefined}");
        let bad_key = validate(
            json!({
                "UpdateExpression": "SET #s = :v",
                "ExpressionAttributeNames": {"s": "status"},
                "ExpressionAttributeValues": {":v": {"S": "x"}},
            }),
            ExprOp::UpdateItem,
        )
        .unwrap_err();
        assert!(bad_key.contains("invalid key"), "{bad_key}");
        let no_expr = validate(
            json!({"ExpressionAttributeValues": {":v": {"S": "x"}}}),
            ExprOp::PutItem,
        )
        .unwrap_err();
        assert_eq!(
            no_expr,
            "ExpressionAttributeValues can only be specified when using expressions"
        );
    }

    #[test]
    fn projection_overlaps_render_resolved_paths() {
        let err =
            validate(json!({"ProjectionExpression": "l, l[0]"}), ExprOp::GetItem).unwrap_err();
        assert!(err.ends_with("path one: [l], path two: [l, [0]]"), "{err}");
        let err = validate(
            json!({
                "ProjectionExpression": "#x, #y.#b",
                "ExpressionAttributeNames": {"#x": "a", "#y": "a", "#b": "b"},
            }),
            ExprOp::GetItem,
        )
        .unwrap_err();
        assert!(err.ends_with("path one: [a], path two: [a, b]"), "{err}");
    }

    #[test]
    fn expression_size_limit_is_on_raw_bytes() {
        let at_limit = format!("a{}", "b".repeat(MAX_EXPRESSION_BYTES - 1));
        assert!(validate(json!({"ProjectionExpression": at_limit}), ExprOp::GetItem).is_ok());
        let over = "b".repeat(MAX_EXPRESSION_BYTES + 1);
        let err = validate(json!({"ProjectionExpression": over}), ExprOp::GetItem).unwrap_err();
        assert!(
            err.contains("Expression size has exceeded the maximum allowed size"),
            "{err}"
        );
    }

    #[test]
    fn operand_type_and_between_checks() {
        let err = validate(
            json!({
                "FilterExpression": "begins_with(#a, :n)",
                "ExpressionAttributeNames": {"#a": "data"},
                "ExpressionAttributeValues": {":n": {"N": "1"}},
            }),
            ExprOp::Scan,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Invalid FilterExpression: Incorrect operand type for operator or function; operator or function: begins_with, operand type: N"
        );
        let err = validate(
            json!({
                "ConditionExpression": "#n BETWEEN :hi AND :lo",
                "ExpressionAttributeNames": {"#n": "n"},
                "ExpressionAttributeValues": {":hi": {"N": "10"}, ":lo": {"N": "1"}},
            }),
            ExprOp::PutItem,
        )
        .unwrap_err();
        assert!(err.contains("upper bound to be greater than"), "{err}");
        let err = validate(
            json!({
                "ConditionExpression": "contains(#a, #a)",
                "ExpressionAttributeNames": {"#a": "data"},
            }),
            ExprOp::PutItem,
        )
        .unwrap_err();
        assert!(
            err.ends_with("operator: contains, first operand: [data]"),
            "{err}"
        );
    }

    #[test]
    fn key_condition_rejects_nested_paths() {
        let err = validate(
            json!({
                "KeyConditionExpression": "#pk = :pk AND #sk.foo = :v",
                "ExpressionAttributeNames": {"#pk": "pk", "#sk": "sk"},
                "ExpressionAttributeValues": {":pk": {"S": "a"}, ":v": {"S": "b"}},
            }),
            ExprOp::Query,
        )
        .unwrap_err();
        assert!(err.contains("cannot have conditions on nested attributes"));
    }

    fn eval(expr: &str, item: Value, values: Value) -> bool {
        let names = HashMap::new();
        let values: HashMap<String, Value> = serde_json::from_value(values).unwrap();
        let item: HashMap<String, Value> = serde_json::from_value(item).unwrap();
        let c = parse_condition_lenient(expr, &names, &values).expect("parses");
        eval_cond(&c, &item, &values)
    }

    #[test]
    fn evaluation_semantics() {
        // `<>` against a missing attribute is true; ordering is false.
        assert!(eval("s <> :v", json!({}), json!({":v": {"S": "x"}})));
        assert!(!eval("s < :v", json!({}), json!({":v": {"S": "x"}})));
        // Set equality ignores element order.
        assert!(eval(
            "s = :v",
            json!({"s": {"SS": ["a", "b"]}}),
            json!({":v": {"SS": ["b", "a"]}})
        ));
        // A value on the left of the comparator works.
        assert!(eval(
            ":lo <= sk",
            json!({"sk": {"S": "m"}}),
            json!({":lo": {"S": "a"}})
        ));
        // size() of a string counts UTF-16 code units.
        assert!(eval(
            "size(s) = :three",
            json!({"s": {"S": "\u{e9}\u{e8}\u{e0}"}}),
            json!({":three": {"N": "3"}})
        ));
    }
}
