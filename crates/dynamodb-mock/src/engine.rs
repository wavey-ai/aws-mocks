//! The in-memory database and the DynamoDB operations over it.
//!
//! Every operation takes the decoded JSON request body and returns the JSON
//! response body, or a [`DdbError`] carrying the DynamoDB error code.

use crate::{
    expr::{self, CmpOp, Cond, Operand, PathElem, Placeholders, SetValue, UpdOperand, Update},
    value::{AttrValue, Item, ScalarKey, item_from_json, item_size, item_to_json},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_ITEM_BYTES: usize = 400 * 1024;
const ACCOUNT: &str = "000000000000";

// ---------------------------------------------------------------------------
// Errors

#[derive(Debug)]
pub struct DdbError {
    pub code: &'static str,
    pub message: String,
    pub extra: Map<String, Value>,
}

impl DdbError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        DdbError {
            code,
            message: message.into(),
            extra: Map::new(),
        }
    }

    pub fn body(&self) -> Value {
        let mut body = self.extra.clone();
        body.insert(
            "__type".into(),
            json!(format!("com.amazonaws.dynamodb.v20120810#{}", self.code)),
        );
        body.insert("message".into(), json!(self.message));
        Value::Object(body)
    }
}

fn validation(message: impl Into<String>) -> DdbError {
    DdbError::new("ValidationException", message)
}

impl From<String> for DdbError {
    fn from(message: String) -> Self {
        validation(message)
    }
}

type Res<T> = Result<T, DdbError>;

fn not_found(name: &str) -> DdbError {
    DdbError::new(
        "ResourceNotFoundException",
        format!("Requested resource not found: Table: {name} not found"),
    )
}

fn missing(field: &str) -> DdbError {
    validation(format!(
        "1 validation error detected: Value null at '{field}' failed to satisfy constraint: Member must not be null"
    ))
}

const CONDITION_FAILED: &str = "The conditional request failed";
const KEY_MISMATCH: &str = "The provided key element does not match the schema";

// ---------------------------------------------------------------------------
// Schema

#[derive(Debug, Clone)]
struct KeySchema {
    hash: String,
    range: Option<String>,
}

impl KeySchema {
    fn parse(value: Option<&Value>) -> Res<KeySchema> {
        let entries = value
            .and_then(Value::as_array)
            .ok_or_else(|| missing("keySchema"))?;
        let mut hash = None;
        let mut range = None;
        for entry in entries {
            let name = entry
                .get("AttributeName")
                .and_then(Value::as_str)
                .ok_or_else(|| missing("keySchema.attributeName"))?
                .to_owned();
            match entry.get("KeyType").and_then(Value::as_str) {
                Some("HASH") if hash.is_none() => hash = Some(name),
                Some("RANGE") if range.is_none() => range = Some(name),
                _ => {
                    return Err(validation(
                        "Invalid KeySchema: Some index key schema element is not valid",
                    ));
                }
            }
        }
        if entries.len() > 2 {
            return Err(validation(
                "Invalid KeySchema: Too many key schema elements",
            ));
        }
        let hash = hash.ok_or_else(|| {
            validation("Invalid KeySchema: The first KeySchemaElement is not a HASH key type")
        })?;
        if Some(&hash) == range.as_ref() {
            return Err(validation(
                "Invalid KeySchema: Both the Hash Key and the Range Key element in the KeySchema have the same name",
            ));
        }
        Ok(KeySchema { hash, range })
    }

    fn names(&self) -> Vec<&String> {
        std::iter::once(&self.hash)
            .chain(self.range.as_ref())
            .collect()
    }

    fn json(&self) -> Value {
        let mut out = vec![json!({"AttributeName": self.hash, "KeyType": "HASH"})];
        if let Some(range) = &self.range {
            out.push(json!({"AttributeName": range, "KeyType": "RANGE"}));
        }
        Value::Array(out)
    }
}

#[derive(Debug, Clone)]
enum Projection {
    All,
    KeysOnly,
    Include(Vec<String>),
}

impl Projection {
    fn parse(value: Option<&Value>) -> Res<Projection> {
        let value = value.ok_or_else(|| missing("projection"))?;
        let attrs: Vec<String> = value
            .get("NonKeyAttributes")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        match value.get("ProjectionType").and_then(Value::as_str) {
            Some("ALL") | None if attrs.is_empty() => Ok(Projection::All),
            Some("KEYS_ONLY") if attrs.is_empty() => Ok(Projection::KeysOnly),
            Some("INCLUDE") if !attrs.is_empty() => Ok(Projection::Include(attrs)),
            Some("INCLUDE") => Err(validation(
                "One or more parameter values were invalid: ProjectionType is INCLUDE, but NonKeyAttributes is not specified",
            )),
            _ => Err(validation(
                "One or more parameter values were invalid: ProjectionType must be INCLUDE when NonKeyAttributes is specified",
            )),
        }
    }

    fn json(&self) -> Value {
        match self {
            Projection::All => json!({"ProjectionType": "ALL"}),
            Projection::KeysOnly => json!({"ProjectionType": "KEYS_ONLY"}),
            Projection::Include(attrs) => {
                json!({"ProjectionType": "INCLUDE", "NonKeyAttributes": attrs})
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Index {
    name: String,
    keys: KeySchema,
    projection: Projection,
    global: bool,
    throughput: Option<Value>,
}

impl Index {
    fn parse(value: &Value, global: bool) -> Res<Index> {
        let name = value
            .get("IndexName")
            .and_then(Value::as_str)
            .ok_or_else(|| missing("indexName"))?;
        check_name(name, 3)?;
        Ok(Index {
            name: name.to_owned(),
            keys: KeySchema::parse(value.get("KeySchema"))?,
            projection: Projection::parse(value.get("Projection"))?,
            global,
            throughput: value.get("ProvisionedThroughput").cloned(),
        })
    }
}

fn check_name(name: &str, min: usize) -> Res<()> {
    let valid = (min..=255).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if valid {
        Ok(())
    } else {
        Err(validation(format!(
            "1 validation error detected: Value '{name}' failed to satisfy constraint: Member must satisfy regular expression pattern: [a-zA-Z0-9_.-]+ and length between {min} and 255"
        )))
    }
}

/// Table primary key: hash value and optional range value.
type TableKey = (ScalarKey, Option<ScalarKey>);

#[derive(Debug, Clone)]
struct Table {
    name: String,
    id: String,
    keys: KeySchema,
    attrs: BTreeMap<String, String>,
    gsis: Vec<Index>,
    lsis: Vec<Index>,
    items: BTreeMap<TableKey, Item>,
    created: f64,
    billing: String,
    throughput: Value,
    ttl: Option<String>,
    stream: Option<Value>,
    deletion_protection: bool,
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

impl Table {
    fn arn(&self, region: &str) -> String {
        format!("arn:aws:dynamodb:{region}:{ACCOUNT}:table/{}", self.name)
    }

    fn index(&self, name: &str) -> Res<&Index> {
        self.gsis
            .iter()
            .chain(self.lsis.iter())
            .find(|index| index.name == name)
            .ok_or_else(|| {
                validation(format!(
                    "The table does not have the specified index: {name}"
                ))
            })
    }

    fn describe(&self, region: &str, status: &str) -> Value {
        let arn = self.arn(region);
        let size: usize = self.items.values().map(item_size).sum();
        let throughput = |value: &Option<Value>| {
            let source = value.as_ref().unwrap_or(&self.throughput);
            json!({
                "ReadCapacityUnits": source.get("ReadCapacityUnits").cloned().unwrap_or(json!(0)),
                "WriteCapacityUnits": source.get("WriteCapacityUnits").cloned().unwrap_or(json!(0)),
                "NumberOfDecreasesToday": 0,
            })
        };
        let mut table = json!({
            "TableName": self.name,
            "TableId": self.id,
            "TableArn": arn,
            "TableStatus": status,
            "KeySchema": self.keys.json(),
            "AttributeDefinitions": self.attrs.iter()
                .map(|(name, kind)| json!({"AttributeName": name, "AttributeType": kind}))
                .collect::<Vec<_>>(),
            "CreationDateTime": self.created,
            "ItemCount": self.items.len(),
            "TableSizeBytes": size,
            "ProvisionedThroughput": throughput(&None),
            "BillingModeSummary": {"BillingMode": self.billing},
            "DeletionProtectionEnabled": self.deletion_protection,
        });
        let index_json = |index: &Index| {
            let members: Vec<&Item> = self
                .items
                .values()
                .filter(|item| in_index(index, item))
                .collect();
            let mut out = json!({
                "IndexName": index.name,
                "KeySchema": index.keys.json(),
                "Projection": index.projection.json(),
                "IndexSizeBytes": members.iter().map(|item| item_size(item)).sum::<usize>(),
                "ItemCount": members.len(),
                "IndexArn": format!("{arn}/index/{}", index.name),
            });
            if index.global {
                out["IndexStatus"] = json!("ACTIVE");
                out["ProvisionedThroughput"] = throughput(&index.throughput);
            }
            out
        };
        if !self.gsis.is_empty() {
            table["GlobalSecondaryIndexes"] =
                Value::Array(self.gsis.iter().map(index_json).collect());
        }
        if !self.lsis.is_empty() {
            table["LocalSecondaryIndexes"] =
                Value::Array(self.lsis.iter().map(index_json).collect());
        }
        if let Some(stream) = &self.stream
            && stream.get("StreamEnabled").and_then(Value::as_bool) == Some(true)
        {
            table["StreamSpecification"] = stream.clone();
            table["LatestStreamArn"] = json!(format!("{arn}/stream/{}", self.created));
        }
        table
    }

    fn key_attr(&self, item: &Item, name: &str, for_item: bool) -> Res<ScalarKey> {
        let expected = &self.attrs[name];
        let Some(value) = item.get(name) else {
            return Err(validation(if for_item {
                format!(
                    "One or more parameter values were invalid: Missing the key {name} in the item"
                )
            } else {
                KEY_MISMATCH.to_owned()
            }));
        };
        if value.type_tag() != expected {
            return Err(validation(if for_item {
                format!(
                    "One or more parameter values were invalid: Type mismatch for key {name} expected: {expected} actual: {}",
                    value.type_tag()
                )
            } else {
                KEY_MISMATCH.to_owned()
            }));
        }
        match value {
            AttrValue::S(s) if s.is_empty() => Err(validation(format!(
                "One or more parameter values are not valid. The AttributeValue for a key attribute cannot contain an empty string value. Key: {name}"
            ))),
            AttrValue::B(b) if b.is_empty() => Err(validation(format!(
                "One or more parameter values are not valid. The AttributeValue for a key attribute cannot contain an empty binary value. Key: {name}"
            ))),
            _ => Ok(ScalarKey::from_attr(value).unwrap()),
        }
    }

    /// The primary key of a full item (`for_item`) or of a `Key` parameter, which
    /// must name exactly the key attributes.
    fn key_of(&self, item: &Item, for_item: bool) -> Res<TableKey> {
        if !for_item && item.len() != self.keys.names().len() {
            return Err(validation(KEY_MISMATCH));
        }
        let hash = self.key_attr(item, &self.keys.hash, for_item)?;
        let range = match &self.keys.range {
            Some(range) => Some(self.key_attr(item, range, for_item)?),
            None => None,
        };
        Ok((hash, range))
    }

    fn key_item(&self, key: &TableKey) -> Item {
        let mut item = Item::new();
        item.insert(self.keys.hash.clone(), key.0.to_attr());
        if let (Some(name), Some(value)) = (&self.keys.range, &key.1) {
            item.insert(name.clone(), value.to_attr());
        }
        item
    }

    /// Validate a full item about to be stored: index key types and size.
    fn check_item(&self, item: &Item) -> Res<()> {
        for index in self.gsis.iter().chain(self.lsis.iter()) {
            for name in index.keys.names() {
                let Some(value) = item.get(name) else {
                    continue;
                };
                let expected = &self.attrs[name];
                if value.type_tag() != expected {
                    return Err(validation(format!(
                        "One or more parameter values were invalid: Type mismatch for Index Key {name} Expected: {expected} Actual: {} IndexName: {}",
                        value.type_tag(),
                        index.name
                    )));
                }
                let empty = matches!(value, AttrValue::S(s) if s.is_empty())
                    || matches!(value, AttrValue::B(b) if b.is_empty());
                if empty {
                    return Err(validation(format!(
                        "One or more parameter values are not valid. A value specified for a secondary index key is not supported. The AttributeValue for a key attribute cannot contain an empty string value. IndexName: {}, IndexKey: {name}",
                        index.name
                    )));
                }
            }
        }
        if item_size(item) > MAX_ITEM_BYTES {
            return Err(validation(
                "Item size has exceeded the maximum allowed size",
            ));
        }
        Ok(())
    }

    /// Sort position of an item (or a start key) within the table or an index:
    /// index keys first, then the table key as a tie-breaker.
    fn position(&self, index: Option<&Index>, item: &Item) -> Option<Vec<ScalarKey>> {
        let mut names: Vec<&String> = Vec::new();
        if let Some(index) = index {
            names.extend(index.keys.names());
        }
        names.extend(self.keys.names());
        names
            .into_iter()
            .map(|name| item.get(name).and_then(ScalarKey::from_attr))
            .collect()
    }

    /// The attributes an index returns for an item.
    fn view(&self, index: Option<&Index>, item: &Item) -> Item {
        let Some(index) = index else {
            return item.clone();
        };
        let keep: Vec<&String> = match &index.projection {
            Projection::All => return item.clone(),
            Projection::KeysOnly => self
                .keys
                .names()
                .into_iter()
                .chain(index.keys.names())
                .collect(),
            Projection::Include(extra) => self
                .keys
                .names()
                .into_iter()
                .chain(index.keys.names())
                .chain(extra.iter())
                .collect(),
        };
        item.iter()
            .filter(|(name, _)| keep.contains(name))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// The key attributes a `LastEvaluatedKey` carries for an item.
    fn last_key(&self, index: Option<&Index>, item: &Item) -> Item {
        let mut names = self.keys.names();
        if let Some(index) = index {
            names.extend(index.keys.names());
        }
        names
            .into_iter()
            .filter_map(|name| item.get(name).map(|v| (name.clone(), v.clone())))
            .collect()
    }
}

fn in_index(index: &Index, item: &Item) -> bool {
    index
        .keys
        .names()
        .iter()
        .all(|name| item.contains_key(*name))
}

// ---------------------------------------------------------------------------
// Request helpers

fn table_name(body: &Value) -> Res<&str> {
    body.get("TableName")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("tableName"))
}

fn placeholders(body: &Value) -> Res<Placeholders> {
    let names: HashMap<String, String> = match body.get("ExpressionAttributeNames") {
        None | Some(Value::Null) => HashMap::new(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_owned()))
                    .ok_or_else(|| validation("ExpressionAttributeNames values must be strings"))
            })
            .collect::<Res<_>>()?,
        _ => return Err(validation("ExpressionAttributeNames must be a map")),
    };
    let values: HashMap<String, AttrValue> = match body.get("ExpressionAttributeValues") {
        None | Some(Value::Null) => HashMap::new(),
        Some(Value::Object(map)) => {
            if map.is_empty() {
                return Err(validation("ExpressionAttributeValues must not be empty"));
            }
            map.iter()
                .map(|(k, v)| {
                    AttrValue::from_json(v)
                        .map(|v| (k.clone(), v))
                        .map_err(|e| {
                            validation(format!(
                                "ExpressionAttributeValues contains invalid value: {e} for key {k}"
                            ))
                        })
                })
                .collect::<Res<_>>()?
        }
        _ => return Err(validation("ExpressionAttributeValues must be a map")),
    };
    if names.keys().any(|k| !k.starts_with('#')) || values.keys().any(|k| !k.starts_with(':')) {
        return Err(validation(
            "ExpressionAttributeNames keys must start with '#' and ExpressionAttributeValues keys with ':'",
        ));
    }
    Ok(Placeholders::new(names, values))
}

fn item_field(body: &Value, field: &str) -> Res<Item> {
    let value = body
        .get(field)
        .ok_or_else(|| missing(&field.to_ascii_lowercase()))?;
    Ok(item_from_json(value)?)
}

fn str_field<'a>(body: &'a Value, field: &str) -> Option<&'a str> {
    body.get(field).and_then(Value::as_str)
}

fn single_path(name: &str) -> Operand {
    Operand::Path(vec![PathElem::Attr(name.to_owned())])
}

/// Legacy `Expected` / `QueryFilter` / `ScanFilter` / `KeyConditions` maps.
fn legacy_conditions(value: Option<&Value>, operator: Option<&str>) -> Res<Option<Cond>> {
    let Some(Value::Object(map)) = value else {
        return Ok(None);
    };
    let mut conds = Vec::new();
    for (name, spec) in map {
        let path = vec![PathElem::Attr(name.clone())];
        let list: Vec<AttrValue> = match spec.get("AttributeValueList") {
            Some(Value::Array(values)) => values
                .iter()
                .map(AttrValue::from_json)
                .collect::<Result<_, _>>()?,
            _ => match spec.get("Value") {
                Some(value) => vec![AttrValue::from_json(value)?],
                None => Vec::new(),
            },
        };
        let arg = |i: usize| -> Res<Operand> {
            list.get(i).cloned().map(Operand::Value).ok_or_else(|| {
                validation(format!(
                    "One or more parameter values were invalid: Invalid number of argument(s) for the {name} condition"
                ))
            })
        };
        let cond = match spec.get("ComparisonOperator").and_then(Value::as_str) {
            None => match spec.get("Exists").and_then(Value::as_bool) {
                Some(false) => Cond::NotExists(path),
                _ => Cond::Cmp(single_path(name), CmpOp::Eq, arg(0)?),
            },
            Some(op) => {
                let cmp = |op| -> Res<Cond> { Ok(Cond::Cmp(single_path(name), op, arg(0)?)) };
                match op {
                    "EQ" => cmp(CmpOp::Eq)?,
                    "NE" => cmp(CmpOp::Ne)?,
                    "LT" => cmp(CmpOp::Lt)?,
                    "LE" => cmp(CmpOp::Le)?,
                    "GT" => cmp(CmpOp::Gt)?,
                    "GE" => cmp(CmpOp::Ge)?,
                    "NOT_NULL" => Cond::Exists(path),
                    "NULL" => Cond::NotExists(path),
                    "BEGINS_WITH" => Cond::BeginsWith(single_path(name), arg(0)?),
                    "CONTAINS" => Cond::Contains(single_path(name), arg(0)?),
                    "NOT_CONTAINS" => Cond::And(
                        Box::new(Cond::Exists(path)),
                        Box::new(Cond::Not(Box::new(Cond::Contains(
                            single_path(name),
                            arg(0)?,
                        )))),
                    ),
                    "BETWEEN" => Cond::Between(single_path(name), arg(0)?, arg(1)?),
                    "IN" => Cond::In(
                        single_path(name),
                        list.iter().cloned().map(Operand::Value).collect(),
                    ),
                    other => {
                        return Err(validation(format!(
                            "Unsupported ComparisonOperator: {other}"
                        )));
                    }
                }
            }
        };
        conds.push(cond);
    }
    let or = operator == Some("OR");
    Ok(conds.into_iter().reduce(|a, b| {
        if or {
            Cond::Or(Box::new(a), Box::new(b))
        } else {
            Cond::And(Box::new(a), Box::new(b))
        }
    }))
}

fn condition(body: &Value, ph: &Placeholders) -> Res<Option<Cond>> {
    match str_field(body, "ConditionExpression") {
        Some(source) => Ok(Some(expr::parse_condition(
            source,
            "ConditionExpression",
            ph,
        )?)),
        None => legacy_conditions(body.get("Expected"), str_field(body, "ConditionalOperator")),
    }
}

fn filter(body: &Value, ph: &Placeholders, legacy: &str) -> Res<Option<Cond>> {
    match str_field(body, "FilterExpression") {
        Some(source) => Ok(Some(expr::parse_condition(source, "FilterExpression", ph)?)),
        None => legacy_conditions(body.get(legacy), str_field(body, "ConditionalOperator")),
    }
}

fn projection(body: &Value, ph: &Placeholders) -> Res<Option<Vec<expr::Path>>> {
    if let Some(source) = str_field(body, "ProjectionExpression") {
        return Ok(Some(expr::parse_projection(source, ph)?));
    }
    Ok(body
        .get("AttributesToGet")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(|name| vec![PathElem::Attr(name.to_owned())])
                .collect()
        }))
}

fn legacy_update(value: Option<&Value>) -> Res<Option<Update>> {
    let Some(Value::Object(map)) = value else {
        return Ok(None);
    };
    let mut update = Update::default();
    for (name, spec) in map {
        let path = vec![PathElem::Attr(name.clone())];
        let value = spec.get("Value").map(AttrValue::from_json).transpose()?;
        match (
            spec.get("Action").and_then(Value::as_str).unwrap_or("PUT"),
            value,
        ) {
            ("PUT", Some(v)) => update
                .set
                .push((path, SetValue::Plain(UpdOperand::Value(v)))),
            ("DELETE", None) => update.remove.push(path),
            ("DELETE", Some(v)) => update.delete.push((path, v)),
            ("ADD", Some(v)) => update.add.push((path, v)),
            _ => {
                return Err(validation(format!(
                    "One or more parameter values were invalid: Action and Value are not valid for attribute {name}"
                )));
            }
        }
    }
    Ok(Some(update))
}

fn return_values<'a>(body: &'a Value, allowed: &[&str]) -> Res<&'a str> {
    let value = str_field(body, "ReturnValues").unwrap_or("NONE");
    if allowed.contains(&value) {
        Ok(value)
    } else {
        Err(validation(format!(
            "1 validation error detected: Value '{value}' at 'returnValues' failed to satisfy constraint: Member must satisfy enum value set: [{}]",
            allowed.join(", ")
        )))
    }
}

fn fail_returns_old(body: &Value) -> bool {
    str_field(body, "ReturnValuesOnConditionCheckFailure") == Some("ALL_OLD")
}

// ---------------------------------------------------------------------------
// Writes shared by single-item operations and transactions

enum WriteKind {
    Put(Item),
    Delete,
    Update(Update),
    Check,
}

struct PlannedWrite {
    table: String,
    key: TableKey,
    kind: WriteKind,
    cond: Option<Cond>,
    old_on_fail: bool,
}

enum WriteFailure {
    Condition(Option<Item>),
    Invalid(DdbError),
}

/// What a write produces: the item before and after (None = absent).
struct WriteOutcome {
    old: Option<Item>,
    new: Option<Item>,
}

// ---------------------------------------------------------------------------
// Database

#[derive(Default)]
pub struct Database {
    tables: BTreeMap<String, Table>,
    tags: BTreeMap<String, BTreeMap<String, String>>,
}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    /// Dispatch one `DynamoDB_20120810.<operation>` request.
    pub fn handle(&mut self, operation: &str, body: &Value, region: &str) -> Res<Value> {
        match operation {
            "CreateTable" => self.create_table(body, region),
            "DescribeTable" => {
                let table = self.table(table_name(body)?)?;
                Ok(json!({"Table": table.describe(region, "ACTIVE")}))
            }
            "ListTables" => self.list_tables(body),
            "DeleteTable" => self.delete_table(body, region),
            "UpdateTable" => self.update_table(body, region),
            "UpdateTimeToLive" => self.update_ttl(body),
            "DescribeTimeToLive" => {
                let table = self.table(table_name(body)?)?;
                Ok(json!({"TimeToLiveDescription": match &table.ttl {
                    Some(name) => json!({"TimeToLiveStatus": "ENABLED", "AttributeName": name}),
                    None => json!({"TimeToLiveStatus": "DISABLED"}),
                }}))
            }
            "DescribeContinuousBackups" => {
                self.table(table_name(body)?)?;
                Ok(json!({"ContinuousBackupsDescription": {
                    "ContinuousBackupsStatus": "ENABLED",
                    "PointInTimeRecoveryDescription": {"PointInTimeRecoveryStatus": "DISABLED"}
                }}))
            }
            "DescribeEndpoints" => {
                Ok(json!({"Endpoints": [{"Address": "localhost", "CachePeriodInMinutes": 1440}]}))
            }
            "DescribeLimits" => Ok(json!({
                "AccountMaxReadCapacityUnits": 80000, "AccountMaxWriteCapacityUnits": 80000,
                "TableMaxReadCapacityUnits": 40000, "TableMaxWriteCapacityUnits": 40000
            })),
            "TagResource" => self.tag(body, true),
            "UntagResource" => self.tag(body, false),
            "ListTagsOfResource" => {
                let arn = str_field(body, "ResourceArn").ok_or_else(|| missing("resourceArn"))?;
                let tags = self.tags.get(arn).cloned().unwrap_or_default();
                Ok(
                    json!({"Tags": tags.iter().map(|(k, v)| json!({"Key": k, "Value": v})).collect::<Vec<_>>()}),
                )
            }
            "GetItem" => self.get_item(body),
            "PutItem" => self.put_item(body),
            "DeleteItem" => self.delete_item(body),
            "UpdateItem" => self.update_item(body),
            "Query" => self.query(body),
            "Scan" => self.scan(body),
            "BatchGetItem" => self.batch_get(body),
            "BatchWriteItem" => self.batch_write(body),
            "TransactWriteItems" => self.transact_write(body),
            "TransactGetItems" => self.transact_get(body),
            other => Err(DdbError::new(
                "UnknownOperationException",
                format!("Unknown operation: {other}"),
            )),
        }
    }

    fn table(&self, name: &str) -> Res<&Table> {
        self.tables.get(name).ok_or_else(|| not_found(name))
    }

    // -- tables --------------------------------------------------------------

    fn create_table(&mut self, body: &Value, region: &str) -> Res<Value> {
        let name = table_name(body)?;
        check_name(name, 3)?;
        if self.tables.contains_key(name) {
            return Err(DdbError::new(
                "ResourceInUseException",
                format!("Table already exists: {name}"),
            ));
        }
        let keys = KeySchema::parse(body.get("KeySchema"))?;
        let mut attrs = BTreeMap::new();
        for definition in body
            .get("AttributeDefinitions")
            .and_then(Value::as_array)
            .ok_or_else(|| missing("attributeDefinitions"))?
        {
            let attr = definition
                .get("AttributeName")
                .and_then(Value::as_str)
                .ok_or_else(|| missing("attributeDefinitions.attributeName"))?;
            let kind = definition
                .get("AttributeType")
                .and_then(Value::as_str)
                .filter(|kind| ["S", "N", "B"].contains(kind))
                .ok_or_else(|| validation("Member must satisfy enum value set: [B, N, S]"))?;
            attrs.insert(attr.to_owned(), kind.to_owned());
        }
        let parse_indexes = |field: &str, global: bool| -> Res<Vec<Index>> {
            match body.get(field) {
                Some(Value::Array(list)) => list.iter().map(|v| Index::parse(v, global)).collect(),
                _ => Ok(Vec::new()),
            }
        };
        let gsis = parse_indexes("GlobalSecondaryIndexes", true)?;
        let lsis = parse_indexes("LocalSecondaryIndexes", false)?;
        let mut seen = BTreeSet::new();
        for index in gsis.iter().chain(lsis.iter()) {
            if !seen.insert(index.name.clone()) {
                return Err(validation(format!(
                    "One or more parameter values were invalid: Duplicate index name: {}",
                    index.name
                )));
            }
        }
        for index in &lsis {
            if keys.range.is_none() {
                return Err(validation(
                    "One or more parameter values were invalid: Table KeySchema does not have a range key, which is required when specifying a LocalSecondaryIndex",
                ));
            }
            if index.keys.hash != keys.hash || index.keys.range.is_none() {
                return Err(validation(format!(
                    "One or more parameter values were invalid: Index KeySchema does not have the same leading hash key as table KeySchema for index: {}",
                    index.name
                )));
            }
        }
        let mut used = BTreeSet::new();
        for name in keys
            .names()
            .into_iter()
            .chain(gsis.iter().chain(lsis.iter()).flat_map(|i| i.keys.names()))
        {
            if !attrs.contains_key(name) {
                return Err(validation(format!(
                    "One or more parameter values were invalid: Some index key attributes are not defined in AttributeDefinitions. Keys: [{name}], AttributeDefinitions: [{}]",
                    attrs.keys().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
            used.insert(name.clone());
        }
        if used.len() != attrs.len() {
            return Err(validation(
                "One or more parameter values were invalid: Number of attributes in KeySchema does not exactly match number of attributes defined in AttributeDefinitions",
            ));
        }
        let billing = str_field(body, "BillingMode")
            .unwrap_or("PROVISIONED")
            .to_owned();
        let throughput = body
            .get("ProvisionedThroughput")
            .cloned()
            .unwrap_or_else(|| json!({"ReadCapacityUnits": 0, "WriteCapacityUnits": 0}));
        let table = Table {
            name: name.to_owned(),
            id: uuid::Uuid::new_v4().to_string(),
            keys,
            attrs,
            gsis,
            lsis,
            items: BTreeMap::new(),
            created: now_seconds(),
            billing,
            throughput,
            ttl: None,
            stream: body.get("StreamSpecification").cloned(),
            deletion_protection: body
                .get("DeletionProtectionEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        if let Some(Value::Array(tags)) = body.get("Tags") {
            let entry = self.tags.entry(table.arn(region)).or_default();
            for tag in tags {
                if let (Some(k), Some(v)) = (str_field(tag, "Key"), str_field(tag, "Value")) {
                    entry.insert(k.to_owned(), v.to_owned());
                }
            }
        }
        let description = table.describe(region, "ACTIVE");
        self.tables.insert(name.to_owned(), table);
        Ok(json!({"TableDescription": description}))
    }

    fn list_tables(&self, body: &Value) -> Res<Value> {
        let limit = body.get("Limit").and_then(Value::as_u64).unwrap_or(100) as usize;
        if !(1..=100).contains(&limit) {
            return Err(validation("Limit must be between 1 and 100"));
        }
        let start = str_field(body, "ExclusiveStartTableName");
        let names: Vec<&String> = self
            .tables
            .keys()
            .filter(|name| start.is_none_or(|start| name.as_str() > start))
            .collect();
        let page: Vec<&String> = names.iter().take(limit).copied().collect();
        let mut out = json!({"TableNames": page});
        if names.len() > limit {
            out["LastEvaluatedTableName"] = json!(page.last());
        }
        Ok(out)
    }

    fn delete_table(&mut self, body: &Value, region: &str) -> Res<Value> {
        let name = table_name(body)?;
        let table = self.table(name)?;
        if table.deletion_protection {
            return Err(validation(format!(
                "Resource cannot be deleted as it is currently protected against deletion. Disable deletion protection first. Table: {name}"
            )));
        }
        let description = table.describe(region, "DELETING");
        self.tags.remove(&table.arn(region));
        self.tables.remove(name);
        Ok(json!({"TableDescription": description}))
    }

    fn update_table(&mut self, body: &Value, region: &str) -> Res<Value> {
        let name = table_name(body)?;
        let mut table = self.table(name)?.clone();
        if let Some(Value::Array(definitions)) = body.get("AttributeDefinitions") {
            for definition in definitions {
                if let (Some(attr), Some(kind)) = (
                    str_field(definition, "AttributeName"),
                    str_field(definition, "AttributeType"),
                ) {
                    if let Some(existing) = table.attrs.get(attr)
                        && existing != kind
                    {
                        return Err(validation(format!(
                            "Cannot change the type of attribute {attr}"
                        )));
                    }
                    table.attrs.insert(attr.to_owned(), kind.to_owned());
                }
            }
        }
        if let Some(Value::Array(updates)) = body.get("GlobalSecondaryIndexUpdates") {
            for update in updates {
                if let Some(create) = update.get("Create") {
                    let index = Index::parse(create, true)?;
                    if table.index(&index.name).is_ok() {
                        return Err(validation(format!(
                            "One or more parameter values were invalid: Index with name {} already exists",
                            index.name
                        )));
                    }
                    for key in index.keys.names() {
                        if !table.attrs.contains_key(key) {
                            return Err(validation(format!(
                                "One or more parameter values were invalid: Some index key attributes are not defined in AttributeDefinitions. Keys: [{key}]"
                            )));
                        }
                    }
                    // Existing items must already fit the new index key types.
                    for item in table.items.values() {
                        for key in index.keys.names() {
                            if let Some(value) = item.get(key)
                                && value.type_tag() != table.attrs[key]
                            {
                                return Err(validation(format!(
                                    "One or more parameter values were invalid: Type mismatch for Index Key {key}"
                                )));
                            }
                        }
                    }
                    table.gsis.push(index);
                } else if let Some(delete) = update.get("Delete") {
                    let index_name =
                        str_field(delete, "IndexName").ok_or_else(|| missing("indexName"))?;
                    let before = table.gsis.len();
                    table.gsis.retain(|index| index.name != index_name);
                    if table.gsis.len() == before {
                        return Err(DdbError::new(
                            "ResourceNotFoundException",
                            format!("Requested resource not found: Index: {index_name} not found"),
                        ));
                    }
                } else if let Some(change) = update.get("Update") {
                    let index_name =
                        str_field(change, "IndexName").ok_or_else(|| missing("indexName"))?;
                    let index = table
                        .gsis
                        .iter_mut()
                        .find(|index| index.name == index_name)
                        .ok_or_else(|| {
                            DdbError::new(
                                "ResourceNotFoundException",
                                format!(
                                    "Requested resource not found: Index: {index_name} not found"
                                ),
                            )
                        })?;
                    if let Some(throughput) = change.get("ProvisionedThroughput") {
                        index.throughput = Some(throughput.clone());
                    }
                }
            }
        }
        if let Some(billing) = str_field(body, "BillingMode") {
            table.billing = billing.to_owned();
        }
        if let Some(throughput) = body.get("ProvisionedThroughput") {
            table.throughput = throughput.clone();
        }
        if let Some(stream) = body.get("StreamSpecification") {
            table.stream = Some(stream.clone());
        }
        if let Some(protection) = body
            .get("DeletionProtectionEnabled")
            .and_then(Value::as_bool)
        {
            table.deletion_protection = protection;
        }
        let description = table.describe(region, "ACTIVE");
        self.tables.insert(name.to_owned(), table);
        Ok(json!({"TableDescription": description}))
    }

    fn update_ttl(&mut self, body: &Value) -> Res<Value> {
        let name = table_name(body)?;
        let spec = body
            .get("TimeToLiveSpecification")
            .ok_or_else(|| missing("timeToLiveSpecification"))?;
        let enabled = spec
            .get("Enabled")
            .and_then(Value::as_bool)
            .ok_or_else(|| missing("timeToLiveSpecification.enabled"))?;
        let attr = str_field(spec, "AttributeName")
            .ok_or_else(|| missing("timeToLiveSpecification.attributeName"))?;
        let table = self.tables.get_mut(name).ok_or_else(|| not_found(name))?;
        match (&table.ttl, enabled) {
            (Some(_), true) => {
                return Err(validation("TimeToLive is already enabled"));
            }
            (None, false) => {
                return Err(validation("TimeToLive is already disabled"));
            }
            _ => {}
        }
        table.ttl = enabled.then(|| attr.to_owned());
        Ok(json!({"TimeToLiveSpecification": spec}))
    }

    fn tag(&mut self, body: &Value, add: bool) -> Res<Value> {
        let arn = str_field(body, "ResourceArn").ok_or_else(|| missing("resourceArn"))?;
        let known = self
            .tables
            .keys()
            .any(|name| arn.ends_with(&format!(":table/{name}")));
        if !known {
            return Err(DdbError::new(
                "ResourceNotFoundException",
                format!("Requested resource not found: ResourcArn: {arn} not found"),
            ));
        }
        let entry = self.tags.entry(arn.to_owned()).or_default();
        if add {
            for tag in body
                .get("Tags")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let (Some(k), Some(v)) = (str_field(tag, "Key"), str_field(tag, "Value")) {
                    entry.insert(k.to_owned(), v.to_owned());
                }
            }
        } else {
            for key in body
                .get("TagKeys")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(key) = key.as_str() {
                    entry.remove(key);
                }
            }
        }
        Ok(json!({}))
    }

    // -- single items --------------------------------------------------------

    fn get_item(&self, body: &Value) -> Res<Value> {
        let table = self.table(table_name(body)?)?;
        let ph = placeholders(body)?;
        let key = table.key_of(&item_field(body, "Key")?, false)?;
        let paths = projection(body, &ph)?;
        ph.check_unused()?;
        Ok(match table.items.get(&key) {
            Some(item) => {
                let item = match &paths {
                    Some(paths) => expr::project(item, paths),
                    None => item.clone(),
                };
                json!({"Item": item_to_json(&item)})
            }
            None => json!({}),
        })
    }

    /// Parse a Put/Update/Delete/ConditionCheck request into a planned write.
    fn plan(&self, body: &Value, kind: &str) -> Res<PlannedWrite> {
        let name = table_name(body)?;
        let table = self.table(name)?;
        let ph = placeholders(body)?;
        let (key, write) = match kind {
            "Put" => {
                let item = item_field(body, "Item")?;
                let key = table.key_of(&item, true)?;
                table.check_item(&item)?;
                (key, WriteKind::Put(item))
            }
            "Delete" => (
                table.key_of(&item_field(body, "Key")?, false)?,
                WriteKind::Delete,
            ),
            "Check" => (
                table.key_of(&item_field(body, "Key")?, false)?,
                WriteKind::Check,
            ),
            _ => {
                let key = table.key_of(&item_field(body, "Key")?, false)?;
                let update = match str_field(body, "UpdateExpression") {
                    Some(source) => expr::parse_update(source, &ph)?,
                    None => legacy_update(body.get("AttributeUpdates"))?.unwrap_or_default(),
                };
                for path in update.paths() {
                    if let Some(PathElem::Attr(first)) = path.first()
                        && table.keys.names().contains(&first)
                    {
                        return Err(validation(format!(
                            "One or more parameter values were invalid: Cannot update attribute {first}. This attribute is part of the key"
                        )));
                    }
                }
                (key, WriteKind::Update(update))
            }
        };
        let cond = condition(body, &ph)?;
        if kind == "Check" && cond.is_none() {
            return Err(missing("conditionExpression"));
        }
        ph.check_unused()?;
        Ok(PlannedWrite {
            table: name.to_owned(),
            key,
            kind: write,
            cond,
            old_on_fail: fail_returns_old(body),
        })
    }

    /// Evaluate a planned write against current state without committing it.
    fn evaluate(&self, plan: &PlannedWrite) -> Result<WriteOutcome, WriteFailure> {
        let table = &self.tables[&plan.table];
        let old = table.items.get(&plan.key).cloned();
        if let Some(cond) = &plan.cond {
            let subject = old.clone().unwrap_or_default();
            let passed =
                expr::evaluate(&subject, cond).map_err(|e| WriteFailure::Invalid(validation(e)))?;
            if !passed {
                return Err(WriteFailure::Condition(if plan.old_on_fail {
                    old
                } else {
                    None
                }));
            }
        }
        let new = match &plan.kind {
            WriteKind::Put(item) => Some(item.clone()),
            WriteKind::Delete => None,
            WriteKind::Check => old.clone(),
            WriteKind::Update(update) => {
                let base = old.clone().unwrap_or_else(|| table.key_item(&plan.key));
                let next = expr::apply_update(&base, update)
                    .map_err(|e| WriteFailure::Invalid(validation(e)))?;
                table.check_item(&next).map_err(WriteFailure::Invalid)?;
                Some(next)
            }
        };
        Ok(WriteOutcome { old, new })
    }

    fn commit(&mut self, plan: &PlannedWrite, outcome: &WriteOutcome) {
        if matches!(plan.kind, WriteKind::Check) {
            return;
        }
        let table = self.tables.get_mut(&plan.table).unwrap();
        match &outcome.new {
            Some(item) => {
                table.items.insert(plan.key.clone(), item.clone());
            }
            None => {
                table.items.remove(&plan.key);
            }
        }
    }

    fn single_write(&mut self, body: &Value, kind: &str) -> Res<(PlannedWrite, WriteOutcome)> {
        let plan = self.plan(body, kind)?;
        match self.evaluate(&plan) {
            Ok(outcome) => {
                self.commit(&plan, &outcome);
                Ok((plan, outcome))
            }
            Err(WriteFailure::Invalid(error)) => Err(error),
            Err(WriteFailure::Condition(old)) => {
                let mut error = DdbError::new("ConditionalCheckFailedException", CONDITION_FAILED);
                if let Some(old) = old {
                    error.extra.insert("Item".into(), item_to_json(&old));
                }
                Err(error)
            }
        }
    }

    fn put_item(&mut self, body: &Value) -> Res<Value> {
        let returns = return_values(body, &["NONE", "ALL_OLD"])?;
        let (_, outcome) = self.single_write(body, "Put")?;
        Ok(attributes(returns, outcome.old.as_ref()))
    }

    fn delete_item(&mut self, body: &Value) -> Res<Value> {
        let returns = return_values(body, &["NONE", "ALL_OLD"])?;
        let (_, outcome) = self.single_write(body, "Delete")?;
        Ok(attributes(returns, outcome.old.as_ref()))
    }

    fn update_item(&mut self, body: &Value) -> Res<Value> {
        let returns = return_values(
            body,
            &["NONE", "ALL_OLD", "UPDATED_OLD", "ALL_NEW", "UPDATED_NEW"],
        )?;
        let (plan, outcome) = self.single_write(body, "Update")?;
        let WriteKind::Update(update) = &plan.kind else {
            unreachable!()
        };
        let paths = update.paths();
        Ok(match returns {
            "ALL_OLD" => attributes(returns, outcome.old.as_ref()),
            "ALL_NEW" => attributes(returns, outcome.new.as_ref()),
            "UPDATED_OLD" => attributes(
                returns,
                outcome
                    .old
                    .as_ref()
                    .map(|old| expr::project(old, &paths))
                    .as_ref(),
            ),
            "UPDATED_NEW" => attributes(
                returns,
                outcome
                    .new
                    .as_ref()
                    .map(|new| expr::project(new, &paths))
                    .as_ref(),
            ),
            _ => json!({}),
        })
    }

    // -- reads over many items ----------------------------------------------

    fn query(&self, body: &Value) -> Res<Value> {
        let table = self.table(table_name(body)?)?;
        let index = str_field(body, "IndexName")
            .map(|name| table.index(name))
            .transpose()?;
        if index.is_some_and(|i| i.global) && body.get("ConsistentRead") == Some(&json!(true)) {
            return Err(validation(
                "Consistent reads are not supported on global secondary indexes",
            ));
        }
        let ph = placeholders(body)?;
        let keys = index.map(|i| &i.keys).unwrap_or(&table.keys);
        let key_cond = match str_field(body, "KeyConditionExpression") {
            Some(source) => expr::parse_condition(source, "KeyConditionExpression", &ph)?,
            None => legacy_conditions(body.get("KeyConditions"), None)?.ok_or_else(|| {
                validation("Either the KeyConditions or KeyConditionExpression parameter must be specified in the request.")
            })?,
        };
        let hash_value = check_key_condition(&key_cond, keys)?;
        let filter = filter(body, &ph, "QueryFilter")?;
        let paths = projection(body, &ph)?;
        ph.check_unused()?;

        let mut candidates: Vec<(Vec<ScalarKey>, &Item)> = Vec::new();
        for item in table.items.values() {
            if item.get(&keys.hash) != Some(&hash_value) {
                continue;
            }
            if let Some(index) = index
                && !in_index(index, item)
            {
                continue;
            }
            if !expr::evaluate(item, &key_cond)? {
                continue;
            }
            candidates.push((table.position(index, item).unwrap(), item));
        }
        let forward = body
            .get("ScanIndexForward")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        page(
            table,
            index,
            body,
            candidates,
            forward,
            filter.as_ref(),
            paths.as_deref(),
        )
    }

    fn scan(&self, body: &Value) -> Res<Value> {
        let table = self.table(table_name(body)?)?;
        let index = str_field(body, "IndexName")
            .map(|name| table.index(name))
            .transpose()?;
        let ph = placeholders(body)?;
        let filter = filter(body, &ph, "ScanFilter")?;
        let paths = projection(body, &ph)?;
        ph.check_unused()?;
        let segment = match (body.get("Segment"), body.get("TotalSegments")) {
            (None, None) => None,
            (Some(segment), Some(total)) => {
                let segment = segment.as_u64().unwrap_or(u64::MAX);
                let total = total.as_u64().unwrap_or(0);
                if total == 0 || total > 1_000_000 || segment >= total {
                    return Err(validation(
                        "The Segment parameter is zero-based and must be less than parameter TotalSegments",
                    ));
                }
                Some((segment, total))
            }
            _ => {
                return Err(validation(
                    "The TotalSegments parameter is required but was not present in the request when Segment parameter is present",
                ));
            }
        };
        let hash_name = index.map(|i| &i.keys.hash).unwrap_or(&table.keys.hash);
        let candidates: Vec<(Vec<ScalarKey>, &Item)> = table
            .items
            .values()
            .filter(|item| index.is_none_or(|index| in_index(index, item)))
            .filter(|item| match segment {
                None => true,
                Some((segment, total)) => {
                    let hash = item
                        .get(hash_name)
                        .map(|v| v.to_json().to_string())
                        .unwrap_or_default();
                    fnv(hash.as_bytes()) % total == segment
                }
            })
            .map(|item| (table.position(index, item).unwrap(), item))
            .collect();
        page(
            table,
            index,
            body,
            candidates,
            true,
            filter.as_ref(),
            paths.as_deref(),
        )
    }

    fn batch_get(&self, body: &Value) -> Res<Value> {
        let requests = body
            .get("RequestItems")
            .and_then(Value::as_object)
            .ok_or_else(|| missing("requestItems"))?;
        let total: usize = requests
            .values()
            .map(|r| r.get("Keys").and_then(Value::as_array).map_or(0, Vec::len))
            .sum();
        if total > 100 {
            return Err(validation(
                "Too many items requested for the BatchGetItem call",
            ));
        }
        let mut responses = Map::new();
        for (name, request) in requests {
            let table = self.table(name)?;
            let ph = placeholders(request)?;
            let paths = projection(request, &ph)?;
            ph.check_unused()?;
            let mut seen = BTreeSet::new();
            let mut found = Vec::new();
            for key in request
                .get("Keys")
                .and_then(Value::as_array)
                .ok_or_else(|| missing("requestItems.keys"))?
            {
                let key = table.key_of(&item_from_json(key)?, false)?;
                if !seen.insert(key.clone()) {
                    return Err(validation("Provided list of item keys contains duplicates"));
                }
                if let Some(item) = table.items.get(&key) {
                    let item = match &paths {
                        Some(paths) => expr::project(item, paths),
                        None => item.clone(),
                    };
                    found.push(item_to_json(&item));
                }
            }
            responses.insert(name.clone(), Value::Array(found));
        }
        Ok(json!({"Responses": responses, "UnprocessedKeys": {}}))
    }

    fn batch_write(&mut self, body: &Value) -> Res<Value> {
        let requests = body
            .get("RequestItems")
            .and_then(Value::as_object)
            .ok_or_else(|| missing("requestItems"))?;
        let mut plans = Vec::new();
        let mut seen = BTreeSet::new();
        for (name, list) in requests {
            let table = self.table(name)?;
            for request in list.as_array().ok_or_else(|| missing("requestItems"))? {
                let (key, kind) = if let Some(put) = request.get("PutRequest") {
                    let item = item_field(put, "Item")?;
                    let key = table.key_of(&item, true)?;
                    table.check_item(&item)?;
                    (key, WriteKind::Put(item))
                } else if let Some(delete) = request.get("DeleteRequest") {
                    (
                        table.key_of(&item_field(delete, "Key")?, false)?,
                        WriteKind::Delete,
                    )
                } else {
                    return Err(validation(
                        "Supplied AttributeValue has neither PutRequest nor DeleteRequest",
                    ));
                };
                if !seen.insert((name.clone(), key.clone())) {
                    return Err(validation("Provided list of item keys contains duplicates"));
                }
                plans.push(PlannedWrite {
                    table: name.clone(),
                    key,
                    kind,
                    cond: None,
                    old_on_fail: false,
                });
            }
        }
        if plans.is_empty() || plans.len() > 25 {
            return Err(validation(
                "1 validation error detected: Value at 'requestItems' failed to satisfy constraint: Map value must satisfy constraint: [Member must have length less than or equal to 25, Member must have length greater than or equal to 1]",
            ));
        }
        for plan in &plans {
            let outcome = self.evaluate(plan).map_err(|failure| match failure {
                WriteFailure::Invalid(error) => error,
                WriteFailure::Condition(_) => validation(CONDITION_FAILED),
            })?;
            self.commit(plan, &outcome);
        }
        Ok(json!({"UnprocessedItems": {}}))
    }

    fn transact_write(&mut self, body: &Value) -> Res<Value> {
        let items = body
            .get("TransactItems")
            .and_then(Value::as_array)
            .ok_or_else(|| missing("transactItems"))?;
        if items.is_empty() || items.len() > 100 {
            return Err(validation(
                "1 validation error detected: Value at 'transactItems' failed to satisfy constraint: Member must have length less than or equal to 100, Member must have length greater than or equal to 1",
            ));
        }
        let mut plans = Vec::with_capacity(items.len());
        let mut targets = BTreeSet::new();
        for entry in items {
            let (kind, request) = [
                ("Put", "Put"),
                ("Update", "Update"),
                ("Delete", "Delete"),
                ("ConditionCheck", "Check"),
            ]
            .iter()
            .find_map(|(field, kind)| entry.get(*field).map(|r| (*kind, r)))
            .ok_or_else(|| {
                validation(
                    "TransactItems can only contain one of ConditionCheck, Put, Update or Delete",
                )
            })?;
            let plan = self.plan(request, kind)?;
            if !targets.insert((plan.table.clone(), plan.key.clone())) {
                return Err(validation(
                    "Transaction request cannot include multiple operations on one item",
                ));
            }
            plans.push(plan);
        }
        let mut outcomes = Vec::with_capacity(plans.len());
        let mut reasons = Vec::with_capacity(plans.len());
        let mut cancelled = false;
        for plan in &plans {
            match self.evaluate(plan) {
                Ok(outcome) => {
                    reasons.push(json!({"Code": "None"}));
                    outcomes.push(Some(outcome));
                }
                Err(WriteFailure::Condition(old)) => {
                    cancelled = true;
                    let mut reason =
                        json!({"Code": "ConditionalCheckFailed", "Message": CONDITION_FAILED});
                    if let Some(old) = old {
                        reason["Item"] = item_to_json(&old);
                    }
                    reasons.push(reason);
                    outcomes.push(None);
                }
                Err(WriteFailure::Invalid(error)) => {
                    cancelled = true;
                    reasons.push(json!({"Code": "ValidationError", "Message": error.message}));
                    outcomes.push(None);
                }
            }
        }
        if cancelled {
            let codes: Vec<&str> = reasons
                .iter()
                .map(|r| r["Code"].as_str().unwrap_or("None"))
                .collect();
            let mut error = DdbError::new(
                "TransactionCanceledException",
                format!(
                    "Transaction cancelled, please refer cancellation reasons for specific reasons [{}]",
                    codes.join(", ")
                ),
            );
            error
                .extra
                .insert("CancellationReasons".into(), Value::Array(reasons));
            return Err(error);
        }
        for (plan, outcome) in plans.iter().zip(outcomes) {
            self.commit(plan, &outcome.unwrap());
        }
        Ok(json!({}))
    }

    fn transact_get(&self, body: &Value) -> Res<Value> {
        let items = body
            .get("TransactItems")
            .and_then(Value::as_array)
            .ok_or_else(|| missing("transactItems"))?;
        if items.is_empty() || items.len() > 100 {
            return Err(validation(
                "TransactItems must have between 1 and 100 entries",
            ));
        }
        let mut responses = Vec::with_capacity(items.len());
        for entry in items {
            let request = entry
                .get("Get")
                .ok_or_else(|| missing("transactItems.get"))?;
            let table = self.table(table_name(request)?)?;
            let ph = placeholders(request)?;
            let key = table.key_of(&item_field(request, "Key")?, false)?;
            let paths = projection(request, &ph)?;
            ph.check_unused()?;
            responses.push(match table.items.get(&key) {
                Some(item) => {
                    let item = match &paths {
                        Some(paths) => expr::project(item, paths),
                        None => item.clone(),
                    };
                    json!({"Item": item_to_json(&item)})
                }
                None => json!({}),
            });
        }
        Ok(json!({"Responses": responses}))
    }
}

fn attributes(returns: &str, item: Option<&Item>) -> Value {
    match item {
        Some(item) if returns != "NONE" && !item.is_empty() => {
            json!({"Attributes": item_to_json(item)})
        }
        _ => json!({}),
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

/// A key condition must be `hash = :v` optionally ANDed with one sort-key
/// condition. Returns the hash key value.
fn check_key_condition(cond: &Cond, keys: &KeySchema) -> Res<AttrValue> {
    fn flatten<'a>(cond: &'a Cond, out: &mut Vec<&'a Cond>) -> Res<()> {
        match cond {
            Cond::And(a, b) => {
                flatten(a, out)?;
                flatten(b, out)
            }
            Cond::Or(..) => Err(validation(
                "Invalid operator used in KeyConditionExpression: OR",
            )),
            Cond::Not(..) => Err(validation(
                "Invalid operator used in KeyConditionExpression: NOT",
            )),
            other => {
                out.push(other);
                Ok(())
            }
        }
    }
    fn attr_of(operand: &Operand) -> Option<&str> {
        match operand {
            Operand::Path(path) if path.len() == 1 => match &path[0] {
                PathElem::Attr(name) => Some(name),
                PathElem::Index(_) => None,
            },
            _ => None,
        }
    }
    let unsupported = || validation("Query key condition not supported");
    let mut parts = Vec::new();
    flatten(cond, &mut parts)?;
    let mut hash = None;
    let mut ranged = false;
    for part in parts {
        let attr = match part {
            Cond::Cmp(a, op, b) => {
                let (attr, value_side) = match (attr_of(a), attr_of(b)) {
                    (Some(attr), None) => (attr, b),
                    (None, Some(attr)) => (attr, a),
                    _ => return Err(unsupported()),
                };
                if *op == CmpOp::Ne {
                    return Err(validation(
                        "Unsupported operator in KeyConditionExpression: <>",
                    ));
                }
                if attr == keys.hash && *op == CmpOp::Eq {
                    if hash.is_some() {
                        return Err(unsupported());
                    }
                    let Operand::Value(value) = value_side else {
                        return Err(unsupported());
                    };
                    hash = Some(value.clone());
                    continue;
                }
                attr
            }
            Cond::Between(a, Operand::Value(_), Operand::Value(_)) => {
                attr_of(a).ok_or_else(unsupported)?
            }
            Cond::BeginsWith(a, Operand::Value(_)) => attr_of(a).ok_or_else(unsupported)?,
            _ => return Err(unsupported()),
        };
        if Some(attr) != keys.range.as_deref() {
            return Err(validation(format!(
                "Query condition missed key schema element: {}",
                if attr == keys.hash {
                    attr
                } else {
                    keys.range.as_deref().unwrap_or(&keys.hash)
                }
            )));
        }
        if ranged {
            return Err(unsupported());
        }
        ranged = true;
    }
    hash.ok_or_else(|| {
        validation(format!(
            "Query condition missed key schema element: {}",
            keys.hash
        ))
    })
}

/// Order, page, filter and project query/scan candidates.
fn page(
    table: &Table,
    index: Option<&Index>,
    body: &Value,
    mut candidates: Vec<(Vec<ScalarKey>, &Item)>,
    forward: bool,
    filter: Option<&Cond>,
    paths: Option<&[expr::Path]>,
) -> Res<Value> {
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    if !forward {
        candidates.reverse();
    }
    if let Some(start) = body.get("ExclusiveStartKey") {
        let start = item_from_json(start)?;
        let position = table
            .position(index, &start)
            .ok_or_else(|| validation("The provided starting key is invalid: The provided key element does not match the schema"))?;
        candidates.retain(|(pos, _)| {
            if forward {
                *pos > position
            } else {
                *pos < position
            }
        });
    }
    let limit = match body.get("Limit") {
        None => None,
        Some(limit) => Some(
            limit
                .as_u64()
                .filter(|n| *n >= 1)
                .ok_or_else(|| validation("1 validation error detected: Value at 'limit' failed to satisfy constraint: Member must have value greater than or equal to 1"))?
                as usize,
        ),
    };
    let select = str_field(body, "Select");
    match select {
        None | Some("ALL_ATTRIBUTES" | "ALL_PROJECTED_ATTRIBUTES" | "COUNT") => {}
        Some("SPECIFIC_ATTRIBUTES") if paths.is_some() => {}
        Some("SPECIFIC_ATTRIBUTES") => {
            return Err(validation(
                "Select type SPECIFIC_ATTRIBUTES requires a ProjectionExpression or AttributesToGet",
            ));
        }
        Some(other) => return Err(validation(format!("Unsupported Select value: {other}"))),
    }
    if paths.is_some()
        && matches!(
            select,
            Some("ALL_ATTRIBUTES" | "ALL_PROJECTED_ATTRIBUTES" | "COUNT")
        )
    {
        return Err(validation(
            "Cannot specify the AttributesToGet or ProjectionExpression when choosing to get ALL_ATTRIBUTES, ALL_PROJECTED_ATTRIBUTES or COUNT",
        ));
    }
    let evaluated = limit.map_or(candidates.len(), |limit| limit.min(candidates.len()));
    let mut items = Vec::new();
    let mut count = 0usize;
    for (_, item) in &candidates[..evaluated] {
        let view = table.view(index, item);
        if let Some(filter) = filter
            && !expr::evaluate(&view, filter)?
        {
            continue;
        }
        count += 1;
        if select != Some("COUNT") {
            let out = match paths {
                Some(paths) => expr::project(&view, paths),
                None => view,
            };
            items.push(item_to_json(&out));
        }
    }
    let mut out = json!({"Count": count, "ScannedCount": evaluated});
    if select != Some("COUNT") {
        out["Items"] = Value::Array(items);
    }
    if limit.is_some_and(|limit| evaluated == limit) && evaluated > 0 {
        let last = candidates[evaluated - 1].1;
        out["LastEvaluatedKey"] = item_to_json(&table.last_key(index, last));
    }
    Ok(out)
}
