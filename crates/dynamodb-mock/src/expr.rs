//! Expression grammar: condition/filter/key-condition, update, and projection
//! expressions, with `ExpressionAttributeNames`/`Values` substitution.

use crate::{
    num::Num,
    value::{AttrValue, Item},
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
};

// ---------------------------------------------------------------------------
// Paths

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathElem {
    Attr(String),
    Index(usize),
}

pub type Path = Vec<PathElem>;

pub fn path_display(path: &Path) -> String {
    let mut out = String::new();
    for elem in path {
        match elem {
            PathElem::Attr(name) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(name);
            }
            PathElem::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

pub fn resolve<'a>(item: &'a Item, path: &Path) -> Option<&'a AttrValue> {
    let (first, rest) = path.split_first()?;
    let PathElem::Attr(name) = first else {
        return None;
    };
    let mut current = item.get(name)?;
    for elem in rest {
        current = match (elem, current) {
            (PathElem::Attr(name), AttrValue::M(map)) => map.get(name)?,
            (PathElem::Index(i), AttrValue::L(list)) => list.get(*i)?,
            _ => return None,
        };
    }
    Some(current)
}

const INVALID_PATH: &str =
    "The document path provided in the update expression is invalid for update";

fn parent_mut<'a>(item: &'a mut Item, path: &Path) -> Result<&'a mut AttrValue, String> {
    let PathElem::Attr(name) = &path[0] else {
        return Err(INVALID_PATH.into());
    };
    let mut current = item.get_mut(name).ok_or(INVALID_PATH)?;
    for elem in &path[1..path.len() - 1] {
        current = match (elem, current) {
            (PathElem::Attr(name), AttrValue::M(map)) => map.get_mut(name).ok_or(INVALID_PATH)?,
            (PathElem::Index(i), AttrValue::L(list)) => list.get_mut(*i).ok_or(INVALID_PATH)?,
            _ => return Err(INVALID_PATH.into()),
        };
    }
    Ok(current)
}

pub fn set_path(item: &mut Item, path: &Path, value: AttrValue) -> Result<(), String> {
    if path.len() == 1 {
        let PathElem::Attr(name) = &path[0] else {
            return Err(INVALID_PATH.into());
        };
        item.insert(name.clone(), value);
        return Ok(());
    }
    match (parent_mut(item, path)?, path.last().unwrap()) {
        (AttrValue::M(map), PathElem::Attr(name)) => {
            map.insert(name.clone(), value);
        }
        (AttrValue::L(list), PathElem::Index(i)) => {
            if *i < list.len() {
                list[*i] = value;
            } else {
                list.push(value);
            }
        }
        _ => return Err(INVALID_PATH.into()),
    }
    Ok(())
}

pub fn remove_path(item: &mut Item, path: &Path) -> Result<(), String> {
    if path.len() == 1 {
        if let PathElem::Attr(name) = &path[0] {
            item.remove(name);
        }
        return Ok(());
    }
    match (parent_mut(item, path)?, path.last().unwrap()) {
        (AttrValue::M(map), PathElem::Attr(name)) => {
            map.remove(name);
        }
        (AttrValue::L(list), PathElem::Index(i)) => {
            if *i < list.len() {
                list.remove(*i);
            }
        }
        _ => return Err(INVALID_PATH.into()),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Placeholders

/// `ExpressionAttributeNames`/`Values` for one request, tracking which were used so
/// unused ones are refused the way DynamoDB refuses them.
#[derive(Default)]
pub struct Placeholders {
    names: HashMap<String, String>,
    values: HashMap<String, AttrValue>,
    used_names: RefCell<BTreeSet<String>>,
    used_values: RefCell<BTreeSet<String>>,
}

impl Placeholders {
    pub fn new(names: HashMap<String, String>, values: HashMap<String, AttrValue>) -> Self {
        Placeholders {
            names,
            values,
            ..Default::default()
        }
    }

    fn name(&self, key: &str) -> Result<String, String> {
        let name = self.names.get(key).ok_or_else(|| {
            format!("An expression attribute name used in the document path is not defined; attribute name: {key}")
        })?;
        self.used_names.borrow_mut().insert(key.to_owned());
        Ok(name.clone())
    }

    fn value(&self, key: &str) -> Result<AttrValue, String> {
        let value = self.values.get(key).ok_or_else(|| {
            format!("An expression attribute value used in expression is not defined; attribute value: {key}")
        })?;
        self.used_values.borrow_mut().insert(key.to_owned());
        Ok(value.clone())
    }

    /// Refuse names/values that no expression referenced.
    pub fn check_unused(&self) -> Result<(), String> {
        let used = self.used_names.borrow();
        let unused: Vec<&String> = self.names.keys().filter(|k| !used.contains(*k)).collect();
        if !unused.is_empty() {
            let mut keys: Vec<_> = unused.into_iter().cloned().collect();
            keys.sort();
            return Err(format!(
                "Value provided in ExpressionAttributeNames unused in expressions: keys: {{{}}}",
                keys.join(", ")
            ));
        }
        let used = self.used_values.borrow();
        let unused: Vec<&String> = self.values.keys().filter(|k| !used.contains(*k)).collect();
        if !unused.is_empty() {
            let mut keys: Vec<_> = unused.into_iter().cloned().collect();
            keys.sort();
            return Err(format!(
                "Value provided in ExpressionAttributeValues unused in expressions: keys: {{{}}}",
                keys.join(", ")
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tokens

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Name(String),
    Value(String),
    Int(usize),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Plus,
    Minus,
    Cmp(CmpOp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn tok_text(tok: &Tok) -> String {
    match tok {
        Tok::Ident(s) | Tok::Name(s) | Tok::Value(s) => s.clone(),
        Tok::Int(i) => i.to_string(),
        Tok::LParen => "(".into(),
        Tok::RParen => ")".into(),
        Tok::LBracket => "[".into(),
        Tok::RBracket => "]".into(),
        Tok::Comma => ",".into(),
        Tok::Dot => ".".into(),
        Tok::Plus => "+".into(),
        Tok::Minus => "-".into(),
        Tok::Cmp(op) => match op {
            CmpOp::Eq => "=",
            CmpOp::Ne => "<>",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
        .into(),
    }
}

fn tokenize(source: &str, kind: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let word = |i: &mut usize| -> String {
        let start = *i;
        while *i < chars.len() && (chars[*i].is_ascii_alphanumeric() || chars[*i] == '_') {
            *i += 1;
        }
        chars[start..*i].iter().collect()
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' | ')' | '[' | ']' | ',' | '.' | '+' | '-' | '=' => {
                out.push(match c {
                    '(' => Tok::LParen,
                    ')' => Tok::RParen,
                    '[' => Tok::LBracket,
                    ']' => Tok::RBracket,
                    ',' => Tok::Comma,
                    '.' => Tok::Dot,
                    '+' => Tok::Plus,
                    '-' => Tok::Minus,
                    _ => Tok::Cmp(CmpOp::Eq),
                });
                i += 1;
            }
            '<' => {
                i += 1;
                out.push(match chars.get(i) {
                    Some('>') => {
                        i += 1;
                        Tok::Cmp(CmpOp::Ne)
                    }
                    Some('=') => {
                        i += 1;
                        Tok::Cmp(CmpOp::Le)
                    }
                    _ => Tok::Cmp(CmpOp::Lt),
                });
            }
            '>' => {
                i += 1;
                if chars.get(i) == Some(&'=') {
                    i += 1;
                    out.push(Tok::Cmp(CmpOp::Ge));
                } else {
                    out.push(Tok::Cmp(CmpOp::Gt));
                }
            }
            '#' | ':' => {
                i += 1;
                let rest = word(&mut i);
                if rest.is_empty() {
                    return Err(format!("Invalid {kind}: Syntax error; token: \"{c}\""));
                }
                out.push(if c == '#' {
                    Tok::Name(format!("#{rest}"))
                } else {
                    Tok::Value(format!(":{rest}"))
                });
            }
            c if c.is_ascii_digit() => {
                let text = word(&mut i);
                match text.parse::<usize>() {
                    Ok(n) => out.push(Tok::Int(n)),
                    Err(_) => out.push(Tok::Ident(text)),
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => out.push(Tok::Ident(word(&mut i))),
            other => {
                return Err(format!(
                    "Invalid {kind}: Syntax error; token: \"{other}\", near: \"{source}\""
                ));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// AST

#[derive(Debug, Clone)]
pub enum Operand {
    Path(Path),
    Value(AttrValue),
    Size(Path),
}

#[derive(Debug, Clone)]
pub enum Cond {
    Cmp(Operand, CmpOp, Operand),
    Between(Operand, Operand, Operand),
    In(Operand, Vec<Operand>),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
    Exists(Path),
    NotExists(Path),
    Type(Path, Operand),
    BeginsWith(Operand, Operand),
    Contains(Operand, Operand),
}

#[derive(Debug, Clone)]
pub enum UpdOperand {
    Path(Path),
    Value(AttrValue),
    IfNotExists(Path, Box<UpdOperand>),
    ListAppend(Box<UpdOperand>, Box<UpdOperand>),
}

#[derive(Debug, Clone)]
pub enum SetValue {
    Plain(UpdOperand),
    Plus(UpdOperand, UpdOperand),
    Minus(UpdOperand, UpdOperand),
}

#[derive(Debug, Clone, Default)]
pub struct Update {
    pub set: Vec<(Path, SetValue)>,
    pub remove: Vec<Path>,
    pub add: Vec<(Path, AttrValue)>,
    pub delete: Vec<(Path, AttrValue)>,
}

impl Update {
    /// Every document path the update writes, in clause order.
    pub fn paths(&self) -> Vec<Path> {
        self.set
            .iter()
            .map(|(p, _)| p.clone())
            .chain(self.remove.iter().cloned())
            .chain(self.add.iter().map(|(p, _)| p.clone()))
            .chain(self.delete.iter().map(|(p, _)| p.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Parser

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    ph: &'a Placeholders,
    kind: &'a str,
}

fn is_kw(tok: Option<&Tok>, kw: &str) -> bool {
    matches!(tok, Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw))
}

impl<'a> Parser<'a> {
    fn new(source: &str, kind: &'a str, ph: &'a Placeholders) -> Result<Self, String> {
        if source.trim().is_empty() {
            return Err(format!("Invalid {kind}: The expression can not be empty;"));
        }
        Ok(Parser {
            toks: tokenize(source, kind)?,
            pos: 0,
            ph,
            kind,
        })
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn peek_at(&self, offset: usize) -> Option<&Tok> {
        self.toks.get(self.pos + offset)
    }

    fn next(&mut self) -> Option<Tok> {
        let tok = self.toks.get(self.pos).cloned();
        self.pos += 1;
        tok
    }

    fn syntax(&self) -> String {
        match self.toks.get(self.pos) {
            Some(tok) => format!(
                "Invalid {}: Syntax error; token: \"{}\"",
                self.kind,
                tok_text(tok)
            ),
            None => format!("Invalid {}: Syntax error; token: \"<EOF>\"", self.kind),
        }
    }

    fn expect(&mut self, want: Tok) -> Result<(), String> {
        if self.peek() == Some(&want) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.syntax())
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), String> {
        if is_kw(self.peek(), kw) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.syntax())
        }
    }

    fn done(&self) -> Result<(), String> {
        if self.pos < self.toks.len() {
            Err(self.syntax())
        } else {
            Ok(())
        }
    }

    fn path(&mut self) -> Result<Path, String> {
        let mut path = Vec::new();
        let first = match self.next() {
            Some(Tok::Ident(name)) => name,
            Some(Tok::Name(key)) => self.ph.name(&key)?,
            _ => {
                self.pos -= 1;
                return Err(self.syntax());
            }
        };
        path.push(PathElem::Attr(first));
        loop {
            match self.peek() {
                Some(Tok::Dot) => {
                    self.pos += 1;
                    match self.next() {
                        Some(Tok::Ident(name)) => path.push(PathElem::Attr(name)),
                        Some(Tok::Name(key)) => path.push(PathElem::Attr(self.ph.name(&key)?)),
                        _ => {
                            self.pos -= 1;
                            return Err(self.syntax());
                        }
                    }
                }
                Some(Tok::LBracket) => {
                    self.pos += 1;
                    match self.next() {
                        Some(Tok::Int(i)) => path.push(PathElem::Index(i)),
                        _ => {
                            self.pos -= 1;
                            return Err(self.syntax());
                        }
                    }
                    self.expect(Tok::RBracket)?;
                }
                _ => return Ok(path),
            }
        }
    }

    fn is_call(&self, name: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s == name)
            && self.peek_at(1) == Some(&Tok::LParen)
    }

    fn operand(&mut self) -> Result<Operand, String> {
        if self.is_call("size") {
            self.pos += 2;
            let path = self.path()?;
            self.expect(Tok::RParen)?;
            return Ok(Operand::Size(path));
        }
        match self.peek() {
            Some(Tok::Value(key)) => {
                let key = key.clone();
                self.pos += 1;
                Ok(Operand::Value(self.ph.value(&key)?))
            }
            Some(Tok::Ident(_)) | Some(Tok::Name(_)) => Ok(Operand::Path(self.path()?)),
            _ => Err(self.syntax()),
        }
    }

    fn cond_or(&mut self) -> Result<Cond, String> {
        let mut left = self.cond_and()?;
        while is_kw(self.peek(), "OR") {
            self.pos += 1;
            let right = self.cond_and()?;
            left = Cond::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn cond_and(&mut self) -> Result<Cond, String> {
        let mut left = self.cond_not()?;
        while is_kw(self.peek(), "AND") {
            self.pos += 1;
            let right = self.cond_not()?;
            left = Cond::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn cond_not(&mut self) -> Result<Cond, String> {
        if is_kw(self.peek(), "NOT") {
            self.pos += 1;
            return Ok(Cond::Not(Box::new(self.cond_not()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Cond, String> {
        if self.peek() == Some(&Tok::LParen) {
            self.pos += 1;
            let cond = self.cond_or()?;
            self.expect(Tok::RParen)?;
            return Ok(cond);
        }
        for function in [
            "attribute_exists",
            "attribute_not_exists",
            "attribute_type",
            "begins_with",
            "contains",
        ] {
            if self.is_call(function) {
                self.pos += 2;
                let cond = match function {
                    "attribute_exists" => Cond::Exists(self.path()?),
                    "attribute_not_exists" => Cond::NotExists(self.path()?),
                    "attribute_type" => {
                        let path = self.path()?;
                        self.expect(Tok::Comma)?;
                        Cond::Type(path, self.operand()?)
                    }
                    "begins_with" => {
                        let target = self.operand()?;
                        self.expect(Tok::Comma)?;
                        Cond::BeginsWith(target, self.operand()?)
                    }
                    _ => {
                        let target = self.operand()?;
                        self.expect(Tok::Comma)?;
                        Cond::Contains(target, self.operand()?)
                    }
                };
                self.expect(Tok::RParen)?;
                return Ok(cond);
            }
        }
        let left = self.operand()?;
        match self.peek().cloned() {
            Some(Tok::Cmp(op)) => {
                self.pos += 1;
                Ok(Cond::Cmp(left, op, self.operand()?))
            }
            Some(Tok::Ident(word)) if word.eq_ignore_ascii_case("BETWEEN") => {
                self.pos += 1;
                let low = self.operand()?;
                self.expect_kw("AND")?;
                let high = self.operand()?;
                if let (Operand::Value(lo), Operand::Value(hi)) = (&low, &high)
                    && lo.compare(hi) == Some(std::cmp::Ordering::Greater)
                {
                    return Err(format!(
                        "Invalid {}: The BETWEEN operator requires upper bound to be greater than or equal to lower bound",
                        self.kind
                    ));
                }
                Ok(Cond::Between(left, low, high))
            }
            Some(Tok::Ident(word)) if word.eq_ignore_ascii_case("IN") => {
                self.pos += 1;
                self.expect(Tok::LParen)?;
                let mut options = vec![self.operand()?];
                while self.peek() == Some(&Tok::Comma) {
                    self.pos += 1;
                    options.push(self.operand()?);
                }
                self.expect(Tok::RParen)?;
                Ok(Cond::In(left, options))
            }
            _ => Err(self.syntax()),
        }
    }

    fn upd_operand(&mut self) -> Result<UpdOperand, String> {
        if self.is_call("if_not_exists") {
            self.pos += 2;
            let path = self.path()?;
            self.expect(Tok::Comma)?;
            let fallback = self.upd_operand()?;
            self.expect(Tok::RParen)?;
            return Ok(UpdOperand::IfNotExists(path, Box::new(fallback)));
        }
        if self.is_call("list_append") {
            self.pos += 2;
            let a = self.upd_operand()?;
            self.expect(Tok::Comma)?;
            let b = self.upd_operand()?;
            self.expect(Tok::RParen)?;
            return Ok(UpdOperand::ListAppend(Box::new(a), Box::new(b)));
        }
        match self.peek() {
            Some(Tok::Value(key)) => {
                let key = key.clone();
                self.pos += 1;
                Ok(UpdOperand::Value(self.ph.value(&key)?))
            }
            Some(Tok::Ident(_)) | Some(Tok::Name(_)) => Ok(UpdOperand::Path(self.path()?)),
            _ => Err(self.syntax()),
        }
    }

    fn value_placeholder(&mut self) -> Result<AttrValue, String> {
        match self.next() {
            Some(Tok::Value(key)) => self.ph.value(&key),
            _ => {
                self.pos -= 1;
                Err(self.syntax())
            }
        }
    }

    fn update(&mut self) -> Result<Update, String> {
        let mut update = Update::default();
        let mut seen = BTreeSet::new();
        while self.peek().is_some() {
            let clause = match self.next() {
                Some(Tok::Ident(word)) => word.to_ascii_uppercase(),
                _ => {
                    self.pos -= 1;
                    return Err(self.syntax());
                }
            };
            if !["SET", "REMOVE", "ADD", "DELETE"].contains(&clause.as_str()) {
                self.pos -= 1;
                return Err(self.syntax());
            }
            if !seen.insert(clause.clone()) {
                return Err(format!(
                    "Invalid UpdateExpression: The \"{clause}\" section can only be used once in an update expression;"
                ));
            }
            loop {
                match clause.as_str() {
                    "SET" => {
                        let path = self.path()?;
                        self.expect(Tok::Cmp(CmpOp::Eq))?;
                        let left = self.upd_operand()?;
                        let value = match self.peek() {
                            Some(Tok::Plus) => {
                                self.pos += 1;
                                SetValue::Plus(left, self.upd_operand()?)
                            }
                            Some(Tok::Minus) => {
                                self.pos += 1;
                                SetValue::Minus(left, self.upd_operand()?)
                            }
                            _ => SetValue::Plain(left),
                        };
                        update.set.push((path, value));
                    }
                    "REMOVE" => update.remove.push(self.path()?),
                    "ADD" => {
                        let path = self.path()?;
                        let value = self.value_placeholder()?;
                        update.add.push((path, value));
                    }
                    _ => {
                        let path = self.path()?;
                        let value = self.value_placeholder()?;
                        update.delete.push((path, value));
                    }
                }
                if self.peek() == Some(&Tok::Comma) {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        Ok(update)
    }
}

pub fn parse_condition(source: &str, kind: &str, ph: &Placeholders) -> Result<Cond, String> {
    let mut parser = Parser::new(source, kind, ph)?;
    let cond = parser.cond_or()?;
    parser.done()?;
    Ok(cond)
}

pub fn parse_update(source: &str, ph: &Placeholders) -> Result<Update, String> {
    let mut parser = Parser::new(source, "UpdateExpression", ph)?;
    let update = parser.update()?;
    parser.done()?;
    let paths = update.paths();
    for (i, a) in paths.iter().enumerate() {
        for b in &paths[i + 1..] {
            let shared = a.len().min(b.len());
            if a[..shared] == b[..shared] {
                return Err(format!(
                    "Invalid UpdateExpression: Two document paths overlap with each other; must remove or rewrite one of these paths; path one: [{}], path two: [{}]",
                    path_display(a),
                    path_display(b)
                ));
            }
        }
    }
    Ok(update)
}

pub fn parse_projection(source: &str, ph: &Placeholders) -> Result<Vec<Path>, String> {
    let mut parser = Parser::new(source, "ProjectionExpression", ph)?;
    let mut paths = vec![parser.path()?];
    while parser.peek() == Some(&Tok::Comma) {
        parser.pos += 1;
        paths.push(parser.path()?);
    }
    parser.done()?;
    Ok(paths)
}

// ---------------------------------------------------------------------------
// Evaluation

fn operand_value(item: &Item, operand: &Operand) -> Option<AttrValue> {
    match operand {
        Operand::Value(v) => Some(v.clone()),
        Operand::Path(p) => resolve(item, p).cloned(),
        Operand::Size(p) => {
            let size = match resolve(item, p)? {
                AttrValue::S(s) => s.chars().count(),
                AttrValue::B(b) => b.len(),
                AttrValue::SS(s) => s.len(),
                AttrValue::NS(s) => s.len(),
                AttrValue::BS(s) => s.len(),
                AttrValue::L(l) => l.len(),
                AttrValue::M(m) => m.len(),
                _ => return None,
            };
            Some(AttrValue::N(Num::parse(&size.to_string()).unwrap()))
        }
    }
}

pub fn evaluate(item: &Item, cond: &Cond) -> Result<bool, String> {
    use std::cmp::Ordering::*;
    Ok(match cond {
        Cond::And(a, b) => evaluate(item, a)? && evaluate(item, b)?,
        Cond::Or(a, b) => evaluate(item, a)? || evaluate(item, b)?,
        Cond::Not(a) => !evaluate(item, a)?,
        Cond::Exists(p) => resolve(item, p).is_some(),
        Cond::NotExists(p) => resolve(item, p).is_none(),
        Cond::Cmp(a, op, b) => {
            let (a, b) = (operand_value(item, a), operand_value(item, b));
            match op {
                CmpOp::Eq => matches!((&a, &b), (Some(x), Some(y)) if x == y),
                CmpOp::Ne => !matches!((&a, &b), (Some(x), Some(y)) if x == y),
                _ => match (a, b) {
                    (Some(x), Some(y)) => match x.compare(&y) {
                        Some(ord) => match op {
                            CmpOp::Lt => ord == Less,
                            CmpOp::Le => ord != Greater,
                            CmpOp::Gt => ord == Greater,
                            _ => ord != Less,
                        },
                        None => false,
                    },
                    _ => false,
                },
            }
        }
        Cond::Between(x, lo, hi) => {
            match (
                operand_value(item, x),
                operand_value(item, lo),
                operand_value(item, hi),
            ) {
                (Some(x), Some(lo), Some(hi)) => {
                    matches!(x.compare(&lo), Some(Greater | Equal))
                        && matches!(x.compare(&hi), Some(Less | Equal))
                }
                _ => false,
            }
        }
        Cond::In(x, options) => match operand_value(item, x) {
            Some(x) => options
                .iter()
                .any(|o| operand_value(item, o).as_ref() == Some(&x)),
            None => false,
        },
        Cond::Type(p, t) => {
            let wanted = match operand_value(item, t) {
                Some(AttrValue::S(s)) => s,
                _ => return Err("Invalid ConditionExpression: Incorrect operand type for operator or function; operator or function: attribute_type".into()),
            };
            if !["S", "N", "B", "BOOL", "NULL", "M", "L", "SS", "NS", "BS"]
                .contains(&wanted.as_str())
            {
                return Err(format!(
                    "Invalid ConditionExpression: Invalid attribute type name found; type: {wanted}, valid types: {{ S,SS,N,NS,B,BS,BOOL,NULL,L,M }}"
                ));
            }
            resolve(item, p).is_some_and(|v| v.type_tag() == wanted)
        }
        Cond::BeginsWith(target, prefix) => {
            match (operand_value(item, target), operand_value(item, prefix)) {
                (Some(AttrValue::S(s)), Some(AttrValue::S(p))) => s.starts_with(&p),
                (Some(AttrValue::B(s)), Some(AttrValue::B(p))) => s.starts_with(&p),
                _ => false,
            }
        }
        Cond::Contains(target, needle) => {
            match (operand_value(item, target), operand_value(item, needle)) {
                (Some(AttrValue::S(s)), Some(AttrValue::S(n))) => s.contains(&n),
                (Some(AttrValue::B(s)), Some(AttrValue::B(n))) => {
                    n.is_empty() || s.windows(n.len()).any(|w| w == n.as_slice())
                }
                (Some(AttrValue::SS(s)), Some(AttrValue::S(n))) => s.contains(&n),
                (Some(AttrValue::NS(s)), Some(AttrValue::N(n))) => s.contains(&n),
                (Some(AttrValue::BS(s)), Some(AttrValue::B(n))) => s.contains(&n),
                (Some(AttrValue::L(l)), Some(n)) => l.contains(&n),
                _ => false,
            }
        }
    })
}

const BAD_OPERAND: &str = "An operand in the update expression has an incorrect data type";
const MISSING_ATTR: &str =
    "The provided expression refers to an attribute that does not exist in the item";

fn upd_value(item: &Item, operand: &UpdOperand) -> Result<AttrValue, String> {
    match operand {
        UpdOperand::Value(v) => Ok(v.clone()),
        UpdOperand::Path(p) => resolve(item, p).cloned().ok_or_else(|| MISSING_ATTR.into()),
        UpdOperand::IfNotExists(p, fallback) => match resolve(item, p) {
            Some(v) => Ok(v.clone()),
            None => upd_value(item, fallback),
        },
        UpdOperand::ListAppend(a, b) => match (upd_value(item, a)?, upd_value(item, b)?) {
            (AttrValue::L(mut a), AttrValue::L(b)) => {
                a.extend(b);
                Ok(AttrValue::L(a))
            }
            _ => Err(format!("{BAD_OPERAND}; operator or function: list_append")),
        },
    }
}

fn arithmetic(
    item: &Item,
    a: &UpdOperand,
    b: &UpdOperand,
    plus: bool,
) -> Result<AttrValue, String> {
    match (upd_value(item, a)?, upd_value(item, b)?) {
        (AttrValue::N(a), AttrValue::N(b)) => {
            let result = if plus { a.add(&b) } else { a.sub(&b) };
            result.check_range()?;
            Ok(AttrValue::N(result))
        }
        _ => Err(format!(
            "{BAD_OPERAND}; operator or function: {}",
            if plus { "+" } else { "-" }
        )),
    }
}

fn set_union(existing: AttrValue, add: AttrValue) -> Result<AttrValue, String> {
    fn merge<T: PartialEq>(mut a: Vec<T>, b: Vec<T>) -> Vec<T> {
        for x in b {
            if !a.contains(&x) {
                a.push(x);
            }
        }
        a
    }
    Ok(match (existing, add) {
        (AttrValue::N(a), AttrValue::N(b)) => {
            let sum = a.add(&b);
            sum.check_range()?;
            AttrValue::N(sum)
        }
        (AttrValue::SS(a), AttrValue::SS(b)) => AttrValue::SS(merge(a, b)),
        (AttrValue::NS(a), AttrValue::NS(b)) => AttrValue::NS(merge(a, b)),
        (AttrValue::BS(a), AttrValue::BS(b)) => AttrValue::BS(merge(a, b)),
        _ => return Err(format!("{BAD_OPERAND}; operator: ADD")),
    })
}

/// `None` means the set became empty and the attribute is removed.
fn set_difference(existing: AttrValue, remove: &AttrValue) -> Result<Option<AttrValue>, String> {
    fn minus<T: PartialEq>(a: Vec<T>, b: &[T]) -> Vec<T> {
        a.into_iter().filter(|x| !b.contains(x)).collect()
    }
    let result = match (existing, remove) {
        (AttrValue::SS(a), AttrValue::SS(b)) => AttrValue::SS(minus(a, b)),
        (AttrValue::NS(a), AttrValue::NS(b)) => AttrValue::NS(minus(a, b)),
        (AttrValue::BS(a), AttrValue::BS(b)) => AttrValue::BS(minus(a, b)),
        _ => return Err(format!("{BAD_OPERAND}; operator: DELETE")),
    };
    let empty = match &result {
        AttrValue::SS(s) => s.is_empty(),
        AttrValue::NS(s) => s.is_empty(),
        AttrValue::BS(s) => s.is_empty(),
        _ => false,
    };
    Ok(if empty { None } else { Some(result) })
}

/// Apply `update` to `original`. Every right-hand side reads the original item.
pub fn apply_update(original: &Item, update: &Update) -> Result<Item, String> {
    let mut item = original.clone();
    let mut assignments = Vec::with_capacity(update.set.len());
    for (path, value) in &update.set {
        let value = match value {
            SetValue::Plain(op) => upd_value(original, op)?,
            SetValue::Plus(a, b) => arithmetic(original, a, b, true)?,
            SetValue::Minus(a, b) => arithmetic(original, a, b, false)?,
        };
        assignments.push((path, value));
    }
    for (path, value) in assignments {
        set_path(&mut item, path, value)?;
    }
    let mut removals: Vec<&Path> = update.remove.iter().collect();
    // Later list indexes first, so earlier ones still name the original elements.
    removals.sort_by(|a, b| b.cmp(a));
    for path in removals {
        remove_path(&mut item, path)?;
    }
    for (path, value) in &update.add {
        if !matches!(
            value,
            AttrValue::N(_) | AttrValue::SS(_) | AttrValue::NS(_) | AttrValue::BS(_)
        ) {
            return Err(format!(
                "Invalid UpdateExpression: Incorrect operand type for operator or function; operator: ADD, operand type: {}",
                value.type_tag()
            ));
        }
        let next = match resolve(&item, path) {
            Some(existing) => set_union(existing.clone(), value.clone())?,
            None => value.clone(),
        };
        set_path(&mut item, path, next)?;
    }
    for (path, value) in &update.delete {
        if !matches!(
            value,
            AttrValue::SS(_) | AttrValue::NS(_) | AttrValue::BS(_)
        ) {
            return Err(format!(
                "Invalid UpdateExpression: Incorrect operand type for operator or function; operator: DELETE, operand type: {}",
                value.type_tag()
            ));
        }
        if let Some(existing) = resolve(&item, path) {
            match set_difference(existing.clone(), value)? {
                Some(next) => set_path(&mut item, path, next)?,
                None => remove_path(&mut item, path)?,
            }
        }
    }
    Ok(item)
}

// ---------------------------------------------------------------------------
// Projection

#[derive(Default)]
struct Mask {
    whole: bool,
    attrs: BTreeMap<String, Mask>,
    indexes: BTreeMap<usize, Mask>,
}

impl Mask {
    fn insert(&mut self, path: &[PathElem]) {
        let Some((first, rest)) = path.split_first() else {
            self.whole = true;
            return;
        };
        let child = match first {
            PathElem::Attr(name) => self.attrs.entry(name.clone()).or_default(),
            PathElem::Index(i) => self.indexes.entry(*i).or_default(),
        };
        child.insert(rest);
    }

    fn apply(&self, value: &AttrValue) -> Option<AttrValue> {
        if self.whole {
            return Some(value.clone());
        }
        match value {
            AttrValue::M(map) if !self.attrs.is_empty() => {
                let out: Item = self
                    .attrs
                    .iter()
                    .filter_map(|(k, m)| Some((k.clone(), m.apply(map.get(k)?)?)))
                    .collect();
                (!out.is_empty()).then_some(AttrValue::M(out))
            }
            AttrValue::L(list) if !self.indexes.is_empty() => {
                let out: Vec<AttrValue> = self
                    .indexes
                    .iter()
                    .filter_map(|(i, m)| m.apply(list.get(*i)?))
                    .collect();
                (!out.is_empty()).then_some(AttrValue::L(out))
            }
            _ => None,
        }
    }
}

/// Keep only the attributes named by `paths`.
pub fn project(item: &Item, paths: &[Path]) -> Item {
    let mut mask = Mask::default();
    for path in paths {
        mask.insert(path);
    }
    mask.attrs
        .iter()
        .filter_map(|(k, m)| Some((k.clone(), m.apply(item.get(k)?)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ph(names: serde_json::Value, values: serde_json::Value) -> Placeholders {
        let names = names
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        let values = values
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), AttrValue::from_json(v).unwrap()))
            .collect();
        Placeholders::new(names, values)
    }

    fn item(value: serde_json::Value) -> Item {
        crate::value::item_from_json(&value).unwrap()
    }

    fn check(source: &str, it: &Item, p: &Placeholders) -> bool {
        evaluate(
            it,
            &parse_condition(source, "ConditionExpression", p).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn precedence_and_parentheses() {
        let p = ph(
            json!({}),
            json!({":t": {"BOOL": true}, ":f": {"BOOL": false}}),
        );
        let it = item(json!({"t": {"BOOL": true}, "f": {"BOOL": false}}));
        // AND binds tighter than OR.
        assert!(check("t = :t OR t = :f AND f = :t", &it, &p));
        assert!(!check("(t = :t OR t = :f) AND f = :t", &it, &p));
        assert!(check("NOT f = :t AND t = :t", &it, &p));
        assert!(!check("NOT (f = :f OR t = :f)", &it, &p));
    }

    #[test]
    fn functions_and_paths() {
        let p = ph(
            json!({"#m": "meta"}),
            json!({":p": {"S": "ab"}, ":n": {"N": "3"}, ":s": {"S": "x"}, ":ty": {"S": "L"}, ":e": {"N": "2"}}),
        );
        let it = item(json!({
            "name": {"S": "abc"},
            "meta": {"M": {"tags": {"SS": ["x", "y"]}, "list": {"L": [{"N": "1"}, {"N": "2"}]}}}
        }));
        assert!(check("begins_with(name, :p)", &it, &p));
        assert!(check("size(name) = :n", &it, &p));
        assert!(check("contains(#m.tags, :s)", &it, &p));
        assert!(check("contains(#m.list, :e)", &it, &p));
        assert!(check("attribute_type(#m.list, :ty)", &it, &p));
        assert!(check("#m.list[1] = :e", &it, &p));
        assert!(check("attribute_not_exists(#m.list[5])", &it, &p));
        assert!(check("size(#m.list) BETWEEN :e AND :n", &it, &p));
        assert!(check("size(#m.list) IN (:n, :e)", &it, &p));
        assert!(check("missing <> :n", &it, &p));
        assert!(!check("missing = :n", &it, &p));
        p.check_unused().unwrap();
    }

    #[test]
    fn unused_and_undefined_placeholders() {
        let p = ph(json!({"#a": "a", "#b": "b"}), json!({":v": {"S": "x"}}));
        parse_condition("#a = :v", "ConditionExpression", &p).unwrap();
        assert!(p.check_unused().unwrap_err().contains("#b"));
        let p = ph(json!({}), json!({}));
        assert!(parse_condition("a = :nope", "ConditionExpression", &p).is_err());
        assert!(parse_condition("a = ", "ConditionExpression", &p).is_err());
        assert!(parse_condition("a = b c", "ConditionExpression", &p).is_err());
    }

    #[test]
    fn update_clauses() {
        let p = ph(
            json!({"#c": "count"}),
            json!({
                ":one": {"N": "1"}, ":zero": {"N": "0"}, ":l": {"L": [{"S": "z"}]},
                ":ss": {"SS": ["b", "c"]}, ":del": {"SS": ["a"]}, ":v": {"S": "new"}
            }),
        );
        let original = item(json!({
            "count": {"N": "1700000000000"},
            "list": {"L": [{"S": "x"}, {"S": "y"}]},
            "tags": {"SS": ["a"]},
            "gone": {"S": "bye"},
            "m": {"M": {}}
        }));
        let update = parse_update(
            "REMOVE gone ADD tags :ss SET #c = #c + :one, list = list_append(list, :l), fresh = if_not_exists(fresh, :zero), m.inner = :v DELETE other :del",
            &p,
        )
        .unwrap();
        p.check_unused().unwrap();
        let next = apply_update(&original, &update).unwrap();
        assert_eq!(next["count"].to_json(), json!({"N": "1700000000001"}));
        assert_eq!(
            next["list"].to_json(),
            json!({"L": [{"S": "x"}, {"S": "y"}, {"S": "z"}]})
        );
        assert_eq!(next["fresh"].to_json(), json!({"N": "0"}));
        assert_eq!(next["m"].to_json(), json!({"M": {"inner": {"S": "new"}}}));
        assert!(next["tags"] == AttrValue::SS(vec!["c".into(), "b".into(), "a".into()]));
        assert!(!next.contains_key("gone"));
    }

    #[test]
    fn update_errors() {
        let p = ph(json!({}), json!({":one": {"N": "1"}, ":s": {"S": "x"}}));
        let base = item(json!({"s": {"S": "x"}}));
        let update = parse_update("SET n = n + :one", &p).unwrap();
        assert!(
            apply_update(&base, &update)
                .unwrap_err()
                .contains("does not exist")
        );
        let update = parse_update("SET s = s + :one", &p).unwrap();
        assert!(
            apply_update(&base, &update)
                .unwrap_err()
                .contains("incorrect data type")
        );
        let update = parse_update("SET a.b = :s", &p).unwrap();
        assert!(
            apply_update(&base, &update)
                .unwrap_err()
                .contains("document path")
        );
        assert!(
            parse_update("SET a = :s, a.b = :one", &p)
                .unwrap_err()
                .contains("overlap")
        );
        assert!(parse_update("SET a = :s SET b = :one", &p).is_err());
    }

    #[test]
    fn list_removal_uses_original_indexes() {
        let p = ph(json!({}), json!({}));
        let original = item(json!({"l": {"L": [{"N": "0"}, {"N": "1"}, {"N": "2"}, {"N": "3"}]}}));
        let update = parse_update("REMOVE l[0], l[2]", &p).unwrap();
        let next = apply_update(&original, &update).unwrap();
        assert_eq!(next["l"].to_json(), json!({"L": [{"N": "1"}, {"N": "3"}]}));
    }

    #[test]
    fn projection_nested() {
        let p = ph(json!({"#a": "a"}), json!({}));
        let it = item(json!({
            "a": {"M": {"b": {"S": "1"}, "c": {"S": "2"}}},
            "l": {"L": [{"S": "x"}, {"S": "y"}, {"S": "z"}]},
            "other": {"S": "o"}
        }));
        let paths = parse_projection("#a.b, l[2], l[0], missing", &p).unwrap();
        let out = project(&it, &paths);
        assert_eq!(
            crate::value::item_to_json(&out),
            json!({"a": {"M": {"b": {"S": "1"}}}, "l": {"L": [{"S": "x"}, {"S": "z"}]}})
        );
    }
}
