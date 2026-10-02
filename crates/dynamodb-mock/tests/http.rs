//! Drive the emulator over HTTP the way an AWS SDK does.

use serde_json::{Value, json};

struct Client {
    http: reqwest::Client,
    endpoint: String,
}

impl Client {
    async fn start() -> (Client, dynamodb_mock::Running) {
        let running = dynamodb_mock::start().await.unwrap();
        let client = Client {
            http: reqwest::Client::new(),
            endpoint: running.endpoint(),
        };
        (client, running)
    }

    /// Returns `Ok(body)` on 200 and `Err((code, body))` on an error reply.
    async fn call(&self, op: &str, body: Value) -> Result<Value, (String, Value)> {
        let response = self
            .http
            .post(&self.endpoint)
            .header("X-Amz-Target", format!("DynamoDB_20120810.{op}"))
            .header("Content-Type", "application/x-amz-json-1.0")
            .header(
                "Authorization",
                "AWS4-HMAC-SHA256 Credential=local/20260101/us-west-2/dynamodb/aws4_request, SignedHeaders=host, Signature=00",
            )
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        if status.is_success() {
            Ok(body)
        } else {
            assert_eq!(status, 400);
            let code = body["__type"]
                .as_str()
                .unwrap()
                .rsplit('#')
                .next()
                .unwrap()
                .to_owned();
            Err((code, body))
        }
    }

    async fn ok(&self, op: &str, body: Value) -> Value {
        match self.call(op, body).await {
            Ok(body) => body,
            Err((code, body)) => panic!("{op} failed with {code}: {body}"),
        }
    }

    async fn err(&self, op: &str, body: Value) -> (String, Value) {
        match self.call(op, body).await {
            Ok(body) => panic!("{op} unexpectedly succeeded: {body}"),
            Err(error) => error,
        }
    }
}

fn s(v: &str) -> Value {
    json!({"S": v})
}

fn n(v: &str) -> Value {
    json!({"N": v})
}

async fn items_table(client: &Client) {
    client
        .ok(
            "CreateTable",
            json!({
                "TableName": "items",
                "BillingMode": "PAY_PER_REQUEST",
                "KeySchema": [
                    {"AttributeName": "pk", "KeyType": "HASH"},
                    {"AttributeName": "sk", "KeyType": "RANGE"}
                ],
                "AttributeDefinitions": [
                    {"AttributeName": "pk", "AttributeType": "S"},
                    {"AttributeName": "sk", "AttributeType": "S"},
                    {"AttributeName": "group", "AttributeType": "S"},
                    {"AttributeName": "rank", "AttributeType": "N"},
                    {"AttributeName": "stamp", "AttributeType": "N"}
                ],
                "GlobalSecondaryIndexes": [
                    {
                        "IndexName": "by-group",
                        "KeySchema": [
                            {"AttributeName": "group", "KeyType": "HASH"},
                            {"AttributeName": "rank", "KeyType": "RANGE"}
                        ],
                        "Projection": {"ProjectionType": "KEYS_ONLY"}
                    }
                ],
                "LocalSecondaryIndexes": [
                    {
                        "IndexName": "by-stamp",
                        "KeySchema": [
                            {"AttributeName": "pk", "KeyType": "HASH"},
                            {"AttributeName": "stamp", "KeyType": "RANGE"}
                        ],
                        "Projection": {"ProjectionType": "INCLUDE", "NonKeyAttributes": ["note"]}
                    }
                ]
            }),
        )
        .await;
}

#[tokio::test]
async fn table_lifecycle() {
    let (client, _running) = Client::start().await;
    items_table(&client).await;
    let described = client
        .ok("DescribeTable", json!({"TableName": "items"}))
        .await;
    assert_eq!(described["Table"]["TableStatus"], "ACTIVE");
    assert_eq!(
        described["Table"]["GlobalSecondaryIndexes"][0]["IndexStatus"],
        "ACTIVE"
    );
    assert!(
        described["Table"]["TableArn"]
            .as_str()
            .unwrap()
            .contains(":us-west-2:")
    );
    let (code, _) = client
        .err(
            "CreateTable",
            json!({
                "TableName": "items",
                "KeySchema": [{"AttributeName": "pk", "KeyType": "HASH"}],
                "AttributeDefinitions": [{"AttributeName": "pk", "AttributeType": "S"}]
            }),
        )
        .await;
    assert_eq!(code, "ResourceInUseException");
    for name in ["b-table", "a-table"] {
        client
            .ok(
                "CreateTable",
                json!({
                    "TableName": name,
                    "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
                    "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}]
                }),
            )
            .await;
    }
    let page = client.ok("ListTables", json!({"Limit": 2})).await;
    assert_eq!(page["TableNames"], json!(["a-table", "b-table"]));
    assert_eq!(page["LastEvaluatedTableName"], "b-table");
    let page = client
        .ok("ListTables", json!({"ExclusiveStartTableName": "b-table"}))
        .await;
    assert_eq!(page["TableNames"], json!(["items"]));
    assert!(page.get("LastEvaluatedTableName").is_none());

    client
        .ok("UpdateTimeToLive", json!({"TableName": "a-table", "TimeToLiveSpecification": {"Enabled": true, "AttributeName": "expires_at"}}))
        .await;
    let ttl = client
        .ok("DescribeTimeToLive", json!({"TableName": "a-table"}))
        .await;
    assert_eq!(ttl["TimeToLiveDescription"]["TimeToLiveStatus"], "ENABLED");

    client
        .ok(
            "UpdateTable",
            json!({
                "TableName": "a-table",
                "AttributeDefinitions": [{"AttributeName": "kind", "AttributeType": "S"}],
                "GlobalSecondaryIndexUpdates": [{"Create": {
                    "IndexName": "by-kind",
                    "KeySchema": [{"AttributeName": "kind", "KeyType": "HASH"}],
                    "Projection": {"ProjectionType": "ALL"}
                }}]
            }),
        )
        .await;
    client
        .ok(
            "PutItem",
            json!({"TableName": "a-table", "Item": {"id": s("1"), "kind": s("k")}}),
        )
        .await;
    let found = client
        .ok(
            "Query",
            json!({
                "TableName": "a-table", "IndexName": "by-kind",
                "KeyConditionExpression": "kind = :k", "ExpressionAttributeValues": {":k": s("k")}
            }),
        )
        .await;
    assert_eq!(found["Count"], 1);

    client
        .ok("DeleteTable", json!({"TableName": "a-table"}))
        .await;
    let (code, _) = client
        .err("DescribeTable", json!({"TableName": "a-table"}))
        .await;
    assert_eq!(code, "ResourceNotFoundException");
    let (code, _) = client
        .err(
            "GetItem",
            json!({"TableName": "a-table", "Key": {"id": s("1")}}),
        )
        .await;
    assert_eq!(code, "ResourceNotFoundException");
}

#[tokio::test]
async fn item_writes_conditions_and_return_values() {
    let (client, _running) = Client::start().await;
    items_table(&client).await;
    let key = json!({"pk": s("a"), "sk": s("1")});
    client
        .ok("PutItem", json!({
            "TableName": "items",
            "Item": {"pk": s("a"), "sk": s("1"), "count": n("1700000000000"), "nested": {"M": {"kind": s("open")}}},
            "ConditionExpression": "attribute_not_exists(pk)"
        }))
        .await;
    let (code, body) = client
        .err(
            "PutItem",
            json!({
                "TableName": "items",
                "Item": {"pk": s("a"), "sk": s("1")},
                "ConditionExpression": "attribute_not_exists(pk)",
                "ReturnValuesOnConditionCheckFailure": "ALL_OLD"
            }),
        )
        .await;
    assert_eq!(code, "ConditionalCheckFailedException");
    assert_eq!(body["message"], "The conditional request failed");
    assert_eq!(body["Item"]["count"], n("1700000000000"));

    let updated = client
        .ok("UpdateItem", json!({
            "TableName": "items", "Key": key,
            "UpdateExpression": "SET #c = #c + :one, nested.#k = :closed ADD tags :tags",
            "ConditionExpression": "nested.#k = :open",
            "ExpressionAttributeNames": {"#c": "count", "#k": "kind"},
            "ExpressionAttributeValues": {":one": n("1"), ":closed": s("closed"), ":open": s("open"), ":tags": {"SS": ["x"]}},
            "ReturnValues": "UPDATED_NEW"
        }))
        .await;
    assert_eq!(
        updated["Attributes"],
        json!({"count": n("1700000000001"), "nested": {"M": {"kind": s("closed")}}, "tags": {"SS": ["x"]}})
    );
    let updated = client
        .ok(
            "UpdateItem",
            json!({
                "TableName": "items", "Key": key,
                "UpdateExpression": "SET #c = #c - :half",
                "ExpressionAttributeNames": {"#c": "count"},
                "ExpressionAttributeValues": {":half": n("0.5")},
                "ReturnValues": "UPDATED_OLD"
            }),
        )
        .await;
    assert_eq!(updated["Attributes"], json!({"count": n("1700000000001")}));

    // Failed condition leaves the item untouched.
    let (code, _) = client
        .err(
            "UpdateItem",
            json!({
                "TableName": "items", "Key": key,
                "UpdateExpression": "SET nested.#k = :open",
                "ConditionExpression": "nested.#k = :open",
                "ExpressionAttributeNames": {"#k": "kind"},
                "ExpressionAttributeValues": {":open": s("open")}
            }),
        )
        .await;
    assert_eq!(code, "ConditionalCheckFailedException");
    let item = client
        .ok(
            "GetItem",
            json!({"TableName": "items", "Key": key, "ConsistentRead": true}),
        )
        .await;
    assert_eq!(item["Item"]["count"], n("1700000000000.5"));
    assert_eq!(item["Item"]["nested"], json!({"M": {"kind": s("closed")}}));

    // Update on a missing item creates it with the key.
    let created = client
        .ok("UpdateItem", json!({
            "TableName": "items", "Key": {"pk": s("a"), "sk": s("2")},
            "UpdateExpression": "SET messages = list_append(if_not_exists(messages, :empty), :m)",
            "ExpressionAttributeValues": {":empty": {"L": []}, ":m": {"L": [s("hi")]}},
            "ReturnValues": "ALL_NEW"
        }))
        .await;
    assert_eq!(
        created["Attributes"],
        json!({"pk": s("a"), "sk": s("2"), "messages": {"L": [s("hi")]}})
    );

    let projected = client
        .ok(
            "GetItem",
            json!({
                "TableName": "items", "Key": key,
                "ProjectionExpression": "#n.#k, sk",
                "ExpressionAttributeNames": {"#n": "nested", "#k": "kind"}
            }),
        )
        .await;
    assert_eq!(
        projected["Item"],
        json!({"sk": s("1"), "nested": {"M": {"kind": s("closed")}}})
    );

    // Validation: unused placeholder, key update, wrong key shape.
    let (code, body) = client
        .err(
            "UpdateItem",
            json!({
                "TableName": "items", "Key": key,
                "UpdateExpression": "SET a = :a",
                "ExpressionAttributeValues": {":a": s("1"), ":unused": s("2")}
            }),
        )
        .await;
    assert_eq!(code, "ValidationException");
    assert!(body["message"].as_str().unwrap().contains(":unused"));
    let (code, _) = client
        .err(
            "UpdateItem",
            json!({
                "TableName": "items", "Key": key,
                "UpdateExpression": "SET sk = :a", "ExpressionAttributeValues": {":a": s("1")}
            }),
        )
        .await;
    assert_eq!(code, "ValidationException");
    let (code, _) = client
        .err(
            "GetItem",
            json!({"TableName": "items", "Key": {"pk": s("a")}}),
        )
        .await;
    assert_eq!(code, "ValidationException");

    let deleted = client
        .ok(
            "DeleteItem",
            json!({"TableName": "items", "Key": key, "ReturnValues": "ALL_OLD"}),
        )
        .await;
    assert_eq!(deleted["Attributes"]["pk"], s("a"));
    let gone = client
        .ok("GetItem", json!({"TableName": "items", "Key": key}))
        .await;
    assert_eq!(gone, json!({}));
}

#[tokio::test]
async fn query_ordering_indexes_and_paging() {
    let (client, _running) = Client::start().await;
    items_table(&client).await;
    // Numeric ranks must sort numerically, not lexically.
    let ranks = ["10", "9", "1700000000000", "-1", "2.5"];
    for (i, rank) in ranks.iter().enumerate() {
        client
            .ok("PutItem", json!({
                "TableName": "items",
                "Item": {
                    "pk": s("p"), "sk": s(&format!("item#{i}")), "group": s("g"),
                    "rank": n(rank), "stamp": n(&(100 - i).to_string()), "note": s("n"), "secret": s("x")
                }
            }))
            .await;
    }
    // Not in the GSI: no group attribute.
    client
        .ok("PutItem", json!({"TableName": "items", "Item": {"pk": s("p"), "sk": s("other"), "stamp": n("1")}}))
        .await;

    let base = client
        .ok(
            "Query",
            json!({
                "TableName": "items",
                "KeyConditionExpression": "pk = :p AND begins_with(sk, :prefix)",
                "ExpressionAttributeValues": {":p": s("p"), ":prefix": s("item#")},
                "ScanIndexForward": false
            }),
        )
        .await;
    let sks: Vec<&str> = base["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["sk"]["S"].as_str().unwrap())
        .collect();
    assert_eq!(sks, ["item#4", "item#3", "item#2", "item#1", "item#0"]);

    let mut ranks_seen = Vec::new();
    let mut start: Option<Value> = None;
    let mut pages = 0;
    loop {
        let mut request = json!({
            "TableName": "items", "IndexName": "by-group",
            "KeyConditionExpression": "#g = :g",
            "ExpressionAttributeNames": {"#g": "group"},
            "ExpressionAttributeValues": {":g": s("g")},
            "Limit": 2
        });
        if let Some(start) = &start {
            request["ExclusiveStartKey"] = start.clone();
        }
        let page = client.ok("Query", request).await;
        pages += 1;
        for item in page["Items"].as_array().unwrap() {
            // KEYS_ONLY projection: table and index keys only.
            let mut keys: Vec<&String> = item.as_object().unwrap().keys().collect();
            keys.sort();
            assert_eq!(keys, ["group", "pk", "rank", "sk"]);
            ranks_seen.push(item["rank"]["N"].as_str().unwrap().to_owned());
        }
        match page.get("LastEvaluatedKey") {
            Some(key) => {
                let mut names: Vec<&String> = key.as_object().unwrap().keys().collect();
                names.sort();
                assert_eq!(names, ["group", "pk", "rank", "sk"]);
                start = Some(key.clone());
            }
            None => break,
        }
    }
    assert_eq!(ranks_seen, ["-1", "2.5", "9", "10", "1700000000000"]);
    assert_eq!(pages, 3);

    let between = client
        .ok(
            "Query",
            json!({
                "TableName": "items", "IndexName": "by-group",
                "KeyConditionExpression": "#g = :g AND #r BETWEEN :lo AND :hi",
                "ExpressionAttributeNames": {"#g": "group", "#r": "rank"},
                "ExpressionAttributeValues": {":g": s("g"), ":lo": n("2"), ":hi": n("10")},
                "Select": "COUNT"
            }),
        )
        .await;
    assert_eq!(between["Count"], 3);
    assert!(between.get("Items").is_none());

    // LSI with INCLUDE projection, filter on a projected attribute.
    let lsi = client
        .ok(
            "Query",
            json!({
                "TableName": "items", "IndexName": "by-stamp",
                "KeyConditionExpression": "pk = :p AND stamp > :s",
                "FilterExpression": "note = :note",
                "ExpressionAttributeValues": {":p": s("p"), ":s": n("96"), ":note": s("n")}
            }),
        )
        .await;
    let stamps: Vec<&str> = lsi["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["stamp"]["N"].as_str().unwrap())
        .collect();
    assert_eq!(stamps, ["97", "98", "99", "100"]);
    assert!(lsi["Items"][0].get("secret").is_none());
    assert_eq!(lsi["ScannedCount"], 4);

    // Limit counts items evaluated before the filter.
    let filtered = client
        .ok(
            "Query",
            json!({
                "TableName": "items",
                "KeyConditionExpression": "pk = :p",
                "FilterExpression": "attribute_exists(#g)",
                "ExpressionAttributeNames": {"#g": "group"},
                "ExpressionAttributeValues": {":p": s("p")},
                "Limit": 6
            }),
        )
        .await;
    assert_eq!(filtered["ScannedCount"], 6);
    assert_eq!(filtered["Count"], 5);
    assert!(filtered.get("LastEvaluatedKey").is_some());

    let (code, body) = client
        .err(
            "Query",
            json!({
                "TableName": "items",
                "KeyConditionExpression": "sk = :s",
                "ExpressionAttributeValues": {":s": s("x")}
            }),
        )
        .await;
    assert_eq!(code, "ValidationException");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("missed key schema element")
    );
    let (code, _) = client
        .err(
            "Query",
            json!({
                "TableName": "items", "IndexName": "by-group", "ConsistentRead": true,
                "KeyConditionExpression": "#g = :g",
                "ExpressionAttributeNames": {"#g": "group"},
                "ExpressionAttributeValues": {":g": s("g")}
            }),
        )
        .await;
    assert_eq!(code, "ValidationException");

    // Scan: segments partition the table; paging covers it.
    let mut total = 0;
    for segment in 0..3 {
        let page = client
            .ok(
                "Scan",
                json!({"TableName": "items", "Segment": segment, "TotalSegments": 3}),
            )
            .await;
        total += page["Count"].as_u64().unwrap();
    }
    assert_eq!(total, 6);
    let mut seen = 0;
    let mut start: Option<Value> = None;
    loop {
        let mut request =
            json!({"TableName": "items", "Limit": 4, "FilterExpression": "attribute_exists(note)"});
        if let Some(start) = &start {
            request["ExclusiveStartKey"] = start.clone();
        }
        let page = client.ok("Scan", request).await;
        seen += page["Count"].as_u64().unwrap();
        match page.get("LastEvaluatedKey") {
            Some(key) => start = Some(key.clone()),
            None => break,
        }
    }
    assert_eq!(seen, 5);
}

#[tokio::test]
async fn transactions_and_batches() {
    let (client, _running) = Client::start().await;
    items_table(&client).await;
    client
        .ok(
            "BatchWriteItem",
            json!({"RequestItems": {"items": [
                {"PutRequest": {"Item": {"pk": s("t"), "sk": s("session"), "open": n("0")}}},
                {"PutRequest": {"Item": {"pk": s("t"), "sk": s("gone")}}}
            ]}}),
        )
        .await;

    let claim = |id: &str, generation: &str| {
        json!({"TransactItems": [
            {"Put": {
                "TableName": "items",
                "Item": {"pk": s("t"), "sk": s(&format!("claim#{id}"))},
                "ConditionExpression": "attribute_not_exists(sk)"
            }},
            {"Update": {
                "TableName": "items",
                "Key": {"pk": s("t"), "sk": s("session")},
                "UpdateExpression": "SET #o = #o + :one",
                "ConditionExpression": "attribute_not_exists(generation) OR generation = :g",
                "ExpressionAttributeNames": {"#o": "open"},
                "ExpressionAttributeValues": {":one": n("1"), ":g": n(generation)},
                "ReturnValuesOnConditionCheckFailure": "ALL_OLD"
            }},
            {"Delete": {"TableName": "items", "Key": {"pk": s("t"), "sk": s("gone")}}},
            {"ConditionCheck": {
                "TableName": "items", "Key": {"pk": s("t"), "sk": s("session")},
                "ConditionExpression": "attribute_exists(sk)"
            }}
        ]})
    };
    // ConditionCheck and Update target the same item: rejected outright.
    let (code, _) = client.err("TransactWriteItems", claim("a", "1")).await;
    assert_eq!(code, "ValidationException");

    let mut request = claim("a", "1");
    request["TransactItems"].as_array_mut().unwrap().pop();
    client.ok("TransactWriteItems", request.clone()).await;
    let session = client
        .ok(
            "GetItem",
            json!({"TableName": "items", "Key": {"pk": s("t"), "sk": s("session")}}),
        )
        .await;
    assert_eq!(session["Item"]["open"], n("1"));

    // Second attempt: the Put fails, so nothing applies.
    client
        .ok("UpdateItem", json!({
            "TableName": "items", "Key": {"pk": s("t"), "sk": s("session")},
            "UpdateExpression": "SET generation = :g", "ExpressionAttributeValues": {":g": n("2")}
        }))
        .await;
    let (code, body) = client.err("TransactWriteItems", request).await;
    assert_eq!(code, "TransactionCanceledException");
    let reasons = body["CancellationReasons"].as_array().unwrap();
    assert_eq!(reasons[0]["Code"], "ConditionalCheckFailed");
    assert_eq!(reasons[1]["Code"], "ConditionalCheckFailed");
    assert_eq!(reasons[1]["Item"]["generation"], n("2"));
    assert_eq!(reasons[2]["Code"], "None");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("[ConditionalCheckFailed, ConditionalCheckFailed, None]")
    );
    let session = client
        .ok(
            "GetItem",
            json!({"TableName": "items", "Key": {"pk": s("t"), "sk": s("session")}}),
        )
        .await;
    assert_eq!(session["Item"]["open"], n("1"));

    let got = client
        .ok("TransactGetItems", json!({"TransactItems": [
            {"Get": {"TableName": "items", "Key": {"pk": s("t"), "sk": s("session")}, "ProjectionExpression": "#o", "ExpressionAttributeNames": {"#o": "open"}}},
            {"Get": {"TableName": "items", "Key": {"pk": s("t"), "sk": s("missing")}}}
        ]}))
        .await;
    assert_eq!(got["Responses"], json!([{"Item": {"open": n("1")}}, {}]));

    let batch = client
        .ok(
            "BatchGetItem",
            json!({"RequestItems": {"items": {"Keys": [
                {"pk": s("t"), "sk": s("session")},
                {"pk": s("t"), "sk": s("claim#a")},
                {"pk": s("t"), "sk": s("gone")}
            ]}}}),
        )
        .await;
    assert_eq!(batch["Responses"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(batch["UnprocessedKeys"], json!({}));
}

#[tokio::test]
async fn protocol_errors() {
    let (client, _running) = Client::start().await;
    let (code, _) = client.err("NoSuchOperation", json!({})).await;
    assert_eq!(code, "UnknownOperationException");
    let (code, _) = client
        .err(
            "CreateTable",
            json!({
                "TableName": "bad",
                "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
                "AttributeDefinitions": [
                    {"AttributeName": "id", "AttributeType": "S"},
                    {"AttributeName": "unused", "AttributeType": "S"}
                ]
            }),
        )
        .await;
    assert_eq!(code, "ValidationException");
}
