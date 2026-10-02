//! Attribute values and their DynamoDB JSON encoding.

use crate::num::Num;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use std::{cmp::Ordering, collections::BTreeMap};

pub type Item = BTreeMap<String, AttrValue>;

/// One attribute value. Sets keep insertion order but compare unordered.
#[derive(Debug, Clone)]
pub enum AttrValue {
    S(String),
    N(Num),
    B(Vec<u8>),
    Bool(bool),
    Null,
    M(Item),
    L(Vec<AttrValue>),
    SS(Vec<String>),
    NS(Vec<Num>),
    BS(Vec<Vec<u8>>),
}

fn same_set<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.contains(x))
}

impl PartialEq for AttrValue {
    fn eq(&self, other: &AttrValue) -> bool {
        use AttrValue::*;
        match (self, other) {
            (S(a), S(b)) => a == b,
            (N(a), N(b)) => a == b,
            (B(a), B(b)) => a == b,
            (Bool(a), Bool(b)) => a == b,
            (Null, Null) => true,
            (M(a), M(b)) => a == b,
            (L(a), L(b)) => a == b,
            (SS(a), SS(b)) => same_set(a, b),
            (NS(a), NS(b)) => same_set(a, b),
            (BS(a), BS(b)) => same_set(a, b),
            _ => false,
        }
    }
}

/// A key attribute value: the only types a key may have, ordered the way
/// DynamoDB orders sort keys (numbers numerically, strings and binary bytewise).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScalarKey {
    N(Num),
    S(String),
    B(Vec<u8>),
}

impl ScalarKey {
    pub fn from_attr(value: &AttrValue) -> Option<ScalarKey> {
        match value {
            AttrValue::S(s) => Some(ScalarKey::S(s.clone())),
            AttrValue::N(n) => Some(ScalarKey::N(n.clone())),
            AttrValue::B(b) => Some(ScalarKey::B(b.clone())),
            _ => None,
        }
    }

    pub fn to_attr(&self) -> AttrValue {
        match self {
            ScalarKey::S(s) => AttrValue::S(s.clone()),
            ScalarKey::N(n) => AttrValue::N(n.clone()),
            ScalarKey::B(b) => AttrValue::B(b.clone()),
        }
    }
}

fn invalid(message: impl Into<String>) -> String {
    message.into()
}

fn number(value: &Value) -> Result<Num, String> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("NUMBER_VALUE cannot be converted to String"))?;
    let num = Num::parse(text)
        .ok_or_else(|| invalid("A value provided cannot be converted into a number"))?;
    num.check_range().map_err(invalid)?;
    Ok(num)
}

fn binary(value: &Value) -> Result<Vec<u8>, String> {
    let text = value
        .as_str()
        .ok_or_else(|| invalid("Binary values must be base64 strings"))?;
    STANDARD
        .decode(text)
        .map_err(|_| invalid("Invalid Base64 value for a binary attribute"))
}

fn set_of<T: PartialEq>(
    value: &Value,
    kind: &str,
    parse: impl Fn(&Value) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    let entries = value
        .as_array()
        .ok_or_else(|| invalid(format!("{kind} must be a list")))?;
    if entries.is_empty() {
        return Err(invalid(format!(
            "One or more parameter values were invalid: An {kind} may not be empty"
        )));
    }
    let mut out: Vec<T> = Vec::with_capacity(entries.len());
    for entry in entries {
        let parsed = parse(entry)?;
        if out.contains(&parsed) {
            return Err(invalid(format!(
                "One or more parameter values were invalid: Input collection {value} contains duplicates."
            )));
        }
        out.push(parsed);
    }
    Ok(out)
}

impl AttrValue {
    /// Decode `{"S": "x"}`-style DynamoDB JSON.
    pub fn from_json(value: &Value) -> Result<AttrValue, String> {
        let object = value
            .as_object()
            .ok_or_else(|| invalid("Supplied AttributeValue must be an object"))?;
        if object.is_empty() {
            return Err(invalid(
                "Supplied AttributeValue is empty, must contain exactly one of the supported datatypes",
            ));
        }
        if object.len() > 1 {
            return Err(invalid(
                "Supplied AttributeValue has more than one datatypes set, must contain exactly one of the supported datatypes",
            ));
        }
        let (tag, inner) = object.iter().next().unwrap();
        Ok(match tag.as_str() {
            "S" => AttrValue::S(
                inner
                    .as_str()
                    .ok_or_else(|| invalid("STRING_VALUE must be a string"))?
                    .to_owned(),
            ),
            "N" => AttrValue::N(number(inner)?),
            "B" => AttrValue::B(binary(inner)?),
            "BOOL" => AttrValue::Bool(
                inner
                    .as_bool()
                    .ok_or_else(|| invalid("BOOL must be a boolean"))?,
            ),
            "NULL" => {
                if inner.as_bool() != Some(true) {
                    return Err(invalid(
                        "One or more parameter values were invalid: Null attribute value types must have the value of true",
                    ));
                }
                AttrValue::Null
            }
            "M" => AttrValue::M(item_from_json(inner)?),
            "L" => AttrValue::L(
                inner
                    .as_array()
                    .ok_or_else(|| invalid("L must be a list"))?
                    .iter()
                    .map(AttrValue::from_json)
                    .collect::<Result<_, _>>()?,
            ),
            "SS" => AttrValue::SS(set_of(inner, "string set", |v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("SS entries must be strings"))
            })?),
            "NS" => AttrValue::NS(set_of(inner, "number set", number)?),
            "BS" => AttrValue::BS(set_of(inner, "binary set", binary)?),
            other => {
                return Err(invalid(format!(
                    "Supplied AttributeValue has an unsupported datatype {other}"
                )));
            }
        })
    }

    pub fn to_json(&self) -> Value {
        match self {
            AttrValue::S(s) => json!({"S": s}),
            AttrValue::N(n) => json!({"N": n.to_string()}),
            AttrValue::B(b) => json!({"B": STANDARD.encode(b)}),
            AttrValue::Bool(b) => json!({"BOOL": b}),
            AttrValue::Null => json!({"NULL": true}),
            AttrValue::M(m) => json!({"M": item_to_json(m)}),
            AttrValue::L(l) => json!({"L": l.iter().map(AttrValue::to_json).collect::<Vec<_>>()}),
            AttrValue::SS(s) => json!({"SS": s}),
            AttrValue::NS(s) => json!({"NS": s.iter().map(Num::to_string).collect::<Vec<_>>()}),
            AttrValue::BS(s) => {
                json!({"BS": s.iter().map(|b| STANDARD.encode(b)).collect::<Vec<_>>()})
            }
        }
    }

    /// The DynamoDB type descriptor (`S`, `N`, `SS`, `BOOL`, ...).
    pub fn type_tag(&self) -> &'static str {
        match self {
            AttrValue::S(_) => "S",
            AttrValue::N(_) => "N",
            AttrValue::B(_) => "B",
            AttrValue::Bool(_) => "BOOL",
            AttrValue::Null => "NULL",
            AttrValue::M(_) => "M",
            AttrValue::L(_) => "L",
            AttrValue::SS(_) => "SS",
            AttrValue::NS(_) => "NS",
            AttrValue::BS(_) => "BS",
        }
    }

    /// Ordering for `<`, `<=`, `>`, `>=`, `BETWEEN`: only scalar values of one type.
    pub fn compare(&self, other: &AttrValue) -> Option<Ordering> {
        match (self, other) {
            (AttrValue::S(a), AttrValue::S(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
            (AttrValue::N(a), AttrValue::N(b)) => Some(a.cmp(b)),
            (AttrValue::B(a), AttrValue::B(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    /// Approximate stored size in bytes, following DynamoDB's sizing rules.
    pub fn size(&self) -> usize {
        fn num_size(n: &Num) -> usize {
            n.to_string()
                .trim_start_matches('-')
                .replace('.', "")
                .len()
                .div_ceil(2)
                + 1
        }
        match self {
            AttrValue::S(s) => s.len(),
            AttrValue::N(n) => num_size(n),
            AttrValue::B(b) => b.len(),
            AttrValue::Bool(_) | AttrValue::Null => 1,
            AttrValue::M(m) => 3 + m.iter().map(|(k, v)| k.len() + v.size() + 1).sum::<usize>(),
            AttrValue::L(l) => 3 + l.iter().map(|v| v.size() + 1).sum::<usize>(),
            AttrValue::SS(s) => s.iter().map(String::len).sum(),
            AttrValue::NS(s) => s.iter().map(num_size).sum(),
            AttrValue::BS(s) => s.iter().map(Vec::len).sum(),
        }
    }
}

pub fn item_from_json(value: &Value) -> Result<Item, String> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("Expected a map of attribute values"))?;
    object
        .iter()
        .map(|(k, v)| Ok((k.clone(), AttrValue::from_json(v)?)))
        .collect()
}

pub fn item_to_json(item: &Item) -> Value {
    Value::Object(
        item.iter()
            .map(|(k, v)| (k.clone(), v.to_json()))
            .collect::<Map<_, _>>(),
    )
}

pub fn item_size(item: &Item) -> usize {
    item.iter().map(|(k, v)| k.len() + v.size()).sum()
}
