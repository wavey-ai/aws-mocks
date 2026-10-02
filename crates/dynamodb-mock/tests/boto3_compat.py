"""boto3 compatibility check for dynamodb-mock.

Starts nothing: point it at a running emulator and it exercises the boto3
resource and client APIs the way an application backend does (string and
`Key`/`Attr` expressions, conditional writes, transactions, GSI paging).

    DYNAMODB_ENDPOINT=http://127.0.0.1:8003 python tests/boto3_compat.py

Environment:
    DYNAMODB_ENDPOINT     emulator URL (falls back to AWS_ENDPOINT_URL_DYNAMODB)
    COMPAT_ITEMS_TABLE    composite-key table with two GSIs (default: random name)
    COMPAT_JOBS_TABLE     hash-key table with TTL (default: random name)

Both tables are created at the start and deleted at the end.
"""

from __future__ import annotations

import os
import sys
import uuid
from decimal import Decimal

import boto3
from boto3.dynamodb.conditions import Attr, Key
from boto3.dynamodb.types import TypeSerializer
from botocore.exceptions import ClientError, EndpointConnectionError

ENDPOINT = os.environ.get("DYNAMODB_ENDPOINT") or os.environ.get("AWS_ENDPOINT_URL_DYNAMODB")
if not ENDPOINT:
    sys.exit("set DYNAMODB_ENDPOINT to the emulator URL")
SUFFIX = uuid.uuid4().hex[:8]
ITEMS = os.environ.get("COMPAT_ITEMS_TABLE") or f"compat-items-{SUFFIX}"
JOBS = os.environ.get("COMPAT_JOBS_TABLE") or f"compat-jobs-{SUFFIX}"

session = boto3.session.Session(
    aws_access_key_id="local", aws_secret_access_key="local", region_name="us-west-2"
)
client = session.client("dynamodb", endpoint_url=ENDPOINT)
resource = session.resource("dynamodb", endpoint_url=ENDPOINT)

checks = 0


def check(condition: bool, message: str) -> None:
    global checks
    checks += 1
    if not condition:
        raise AssertionError(message)


def error_code(call) -> tuple[str, dict]:
    try:
        call()
    except ClientError as error:
        return error.response["Error"]["Code"], error.response
    raise AssertionError("expected a ClientError")


def conditional_failed(call) -> None:
    code, _ = error_code(call)
    check(code == "ConditionalCheckFailedException", f"expected conditional failure, got {code}")


# ---------------------------------------------------------------------------
# Table setup, the way a local bootstrap script does it.


def setup_tables() -> None:
    try:
        client.list_tables(Limit=1)
    except EndpointConnectionError:
        sys.exit(f"no emulator at {ENDPOINT}")
    code, _ = error_code(lambda: client.describe_table(TableName=ITEMS))
    check(code == "ResourceNotFoundException", "missing table must be ResourceNotFound")
    client.create_table(
        TableName=ITEMS,
        BillingMode="PAY_PER_REQUEST",
        KeySchema=[
            {"AttributeName": "user_id", "KeyType": "HASH"},
            {"AttributeName": "item_id", "KeyType": "RANGE"},
        ],
        AttributeDefinitions=[
            {"AttributeName": "user_id", "AttributeType": "S"},
            {"AttributeName": "item_id", "AttributeType": "S"},
            {"AttributeName": "folder_id", "AttributeType": "S"},
            {"AttributeName": "type_date", "AttributeType": "S"},
        ],
        GlobalSecondaryIndexes=[
            {
                "IndexName": "folder-contents-index",
                "KeySchema": [
                    {"AttributeName": "user_id", "KeyType": "HASH"},
                    {"AttributeName": "folder_id", "KeyType": "RANGE"},
                ],
                "Projection": {"ProjectionType": "ALL"},
            },
            {
                "IndexName": "type-date-index",
                "KeySchema": [
                    {"AttributeName": "user_id", "KeyType": "HASH"},
                    {"AttributeName": "type_date", "KeyType": "RANGE"},
                ],
                "Projection": {"ProjectionType": "ALL"},
            },
        ],
    )
    client.get_waiter("table_exists").wait(TableName=ITEMS, WaiterConfig={"Delay": 1})
    code, _ = error_code(
        lambda: client.create_table(
            TableName=ITEMS,
            BillingMode="PAY_PER_REQUEST",
            KeySchema=[{"AttributeName": "user_id", "KeyType": "HASH"}],
            AttributeDefinitions=[{"AttributeName": "user_id", "AttributeType": "S"}],
        )
    )
    check(code == "ResourceInUseException", "duplicate create must be ResourceInUse")

    jobs = resource.create_table(
        TableName=JOBS,
        BillingMode="PAY_PER_REQUEST",
        KeySchema=[{"AttributeName": "job_id", "KeyType": "HASH"}],
        AttributeDefinitions=[{"AttributeName": "job_id", "AttributeType": "S"}],
    )
    jobs.wait_until_exists()
    client.update_time_to_live(
        TableName=JOBS,
        TimeToLiveSpecification={"Enabled": True, "AttributeName": "expires_at"},
    )
    ttl = client.describe_time_to_live(TableName=JOBS)["TimeToLiveDescription"]
    check(ttl["TimeToLiveStatus"] == "ENABLED", "TTL enabled")

    described = client.describe_table(TableName=ITEMS)["Table"]
    check(described["TableStatus"] == "ACTIVE", "table ACTIVE")
    indexes = {index["IndexName"] for index in described["GlobalSecondaryIndexes"]}
    check(indexes == {"folder-contents-index", "type-date-index"}, f"GSIs {indexes}")
    check(ITEMS in client.list_tables()["TableNames"], "table listed")


def query_all(table, **kwargs) -> list[dict]:
    items: list[dict] = []
    while True:
        page = table.query(**kwargs)
        items.extend(page.get("Items", []))
        start = page.get("LastEvaluatedKey")
        if not start:
            return items
        kwargs["ExclusiveStartKey"] = start


# ---------------------------------------------------------------------------
# Folder/file records and GSI queries.


def folders_and_files(table) -> None:
    owner = "workspace-1"
    for i in range(5):
        table.put_item(
            Item={
                "user_id": owner,
                "item_id": f"deal-{i}",
                "item_type": "deal",
                "folder_id": "root",
                "type_date": f"deal#2026-01-0{i + 1}T00:00:00Z",
                "name": f"Deal {i}",
                "status": "active",
            }
        )
    for i in range(3):
        table.put_item(
            Item={
                "user_id": owner,
                "item_id": f"file-{i}",
                "item_type": "file",
                "folder_id": "deal-1",
                "type_date": f"file#2026-02-0{i + 1}T00:00:00Z",
                "status": "uploading",
                "size_bytes": 0,
            }
        )
    deals = query_all(
        table,
        IndexName="type-date-index",
        KeyConditionExpression="user_id = :u AND begins_with(type_date, :p)",
        ExpressionAttributeValues={":u": owner, ":p": "deal#"},
        ScanIndexForward=False,
        Limit=2,
    )
    check([d["item_id"] for d in deals] == [f"deal-{i}" for i in range(4, -1, -1)], "deals newest first across pages")

    page = table.query(
        IndexName="type-date-index",
        KeyConditionExpression=Key("user_id").eq(owner) & Key("type_date").begins_with("file#"),
        Limit=2,
    )
    start = page["LastEvaluatedKey"]
    check(set(start) == {"user_id", "item_id", "type_date"}, f"GSI LastEvaluatedKey keys {set(start)}")

    files = query_all(
        table,
        IndexName="folder-contents-index",
        KeyConditionExpression="user_id = :u AND folder_id = :f",
        ExpressionAttributeValues={":u": owner, ":f": "deal-1"},
    )
    check(sorted(f["item_id"] for f in files) == ["file-0", "file-1", "file-2"], "files in folder")

    table.update_item(
        Key={"user_id": owner, "item_id": "deal-0"},
        UpdateExpression="SET updated_at = :now, #n = :n, #s = :s",
        ExpressionAttributeNames={"#n": "name", "#s": "status"},
        ExpressionAttributeValues={":now": "2026-03-01T00:00:00Z", ":n": "Renamed", ":s": "closed"},
    )
    table.update_item(
        Key={"user_id": owner, "item_id": "file-0"},
        UpdateExpression="SET #s = :ready, size_bytes = :sz, updated_at = :now",
        ExpressionAttributeNames={"#s": "status"},
        ExpressionAttributeValues={":ready": "ready", ":sz": 12345, ":now": "2026-03-01T00:00:00Z"},
    )
    deal = table.get_item(Key={"user_id": owner, "item_id": "deal-0"})["Item"]
    check(deal["name"] == "Renamed" and deal["status"] == "closed", "deal updated")
    ready = table.get_item(Key={"user_id": owner, "item_id": "file-0"}, ConsistentRead=False)["Item"]
    check(ready["size_bytes"] == Decimal(12345), "int size round-trips as Decimal")

    in_range = table.query(
        KeyConditionExpression=Key("user_id").eq(owner) & Key("item_id").between("deal-1", "deal-3"),
        Select="COUNT",
    )
    check(in_range["Count"] == 3, f"BETWEEN count {in_range['Count']}")
    filtered = table.query(
        KeyConditionExpression=Key("user_id").eq(owner),
        FilterExpression=Attr("item_type").eq("file") & Attr("status").is_in(["ready", "uploading"]),
        ProjectionExpression="item_id, #s",
        ExpressionAttributeNames={"#s": "status"},
    )
    check(len(filtered["Items"]) == 3 and set(filtered["Items"][0]) == {"item_id", "status"}, "filter + projection")

    table.delete_item(Key={"user_id": owner, "item_id": "file-2"})
    check("Item" not in table.get_item(Key={"user_id": owner, "item_id": "file-2"}), "deleted")


# ---------------------------------------------------------------------------
# Conversation records: guarded whole-item puts and patch updates.


def conversations(table) -> None:
    owner = "owner-1"
    key = {"user_id": owner, "item_id": "conv-1"}
    table.put_item(
        Item={
            **key,
            "item_type": "conversation",
            "type_date": "conversation#2026-01-01T00:00:00Z",
            "title": "First",
            "messages": [],
            "score": Decimal("0.5"),
        }
    )
    stored = table.get_item(Key=key)["Item"]
    table.put_item(
        Item={**stored, "messages": [{"role": "user", "text": "hi"}]},
        ConditionExpression="attribute_exists(item_id) AND #guard_title = :guard_title AND attribute_not_exists(archived_at)",
        ExpressionAttributeNames={"#guard_title": "title"},
        ExpressionAttributeValues={":guard_title": "First"},
    )
    conditional_failed(
        lambda: table.put_item(
            Item=stored,
            ConditionExpression="attribute_exists(item_id) AND #guard_title = :guard_title AND attribute_not_exists(archived_at)",
            ExpressionAttributeNames={"#guard_title": "title"},
            ExpressionAttributeValues={":guard_title": "Stale"},
        )
    )
    table.update_item(
        Key=key,
        UpdateExpression="SET #title = :title, archived_at = :archived_at",
        ConditionExpression="attribute_exists(item_id)",
        ExpressionAttributeNames={"#title": "title"},
        ExpressionAttributeValues={":title": "Second", ":archived_at": "2026-01-02T00:00:00Z"},
    )
    table.update_item(Key=key, UpdateExpression="REMOVE archived_at", ConditionExpression="attribute_exists(item_id)")
    conditional_failed(
        lambda: table.update_item(
            Key={"user_id": owner, "item_id": "missing"},
            UpdateExpression="REMOVE archived_at",
            ConditionExpression="attribute_exists(item_id)",
        )
    )
    # Transcript append and in-place replacement.
    for text in ("one", "two"):
        table.update_item(
            Key=key,
            UpdateExpression="SET messages = list_append(if_not_exists(messages, :empty), :m), updated_at = :now",
            ExpressionAttributeValues={":m": [{"role": "assistant", "text": text}], ":empty": [], ":now": "t"},
        )
    table.update_item(
        Key=key,
        UpdateExpression="SET messages[1] = :message, updated_at = :now",
        ExpressionAttributeValues={":message": {"role": "assistant", "text": "replaced"}, ":now": "t2"},
    )
    conversation = table.get_item(Key=key)["Item"]
    texts = [m["text"] for m in conversation["messages"]]
    check(texts == ["hi", "replaced", "two"], f"transcript {texts}")
    check(conversation["title"] == "Second" and "archived_at" not in conversation, "patched title, archive removed")
    check(conversation["score"] == Decimal("0.5"), "decimal fraction round-trips")
    table.update_item(
        Key=key,
        UpdateExpression="SET title = :new",
        ConditionExpression="title = :old",
        ExpressionAttributeValues={":new": "Third", ":old": "Second"},
    )
    listed = query_all(
        table,
        IndexName="type-date-index",
        KeyConditionExpression="user_id = :uid AND begins_with(type_date, :prefix)",
        ExpressionAttributeValues={":uid": owner, ":prefix": "conversation#"},
        ScanIndexForward=False,
    )
    check([c["title"] for c in listed] == ["Third"], "conversation listed via GSI")


# ---------------------------------------------------------------------------
# Run records: nested maps, NULLs, state-machine conditions, ADD counters.


def runs(table) -> None:
    owner = "owner-2"
    key = {"user_id": owner, "item_id": "run#r1"}
    record = {
        **key,
        "item_type": "agent_run",
        "type_date": "agent_run#2026-01-01T00:00:00Z",
        "status": "running",
        "runtime_dispatch": {"kind": "unassigned"},
        "progress_snapshot": {"items": [1, 2], "done": False},
        "activation_failure": None,
        "interruption": None,
        "started_at_ms": 1767225600123,
    }
    table.put_item(Item=record, ConditionExpression="attribute_not_exists(item_id)")
    conditional_failed(lambda: table.put_item(Item=record, ConditionExpression="attribute_not_exists(item_id)"))

    dispatch = {"kind": "assigned", "executor": "e1", "generation": 3}
    table.update_item(
        Key=key,
        UpdateExpression="SET runtime_dispatch = :dispatch, updated_at = :now",
        ConditionExpression="runtime_dispatch.#kind = :unassigned AND attribute_not_exists(outcome)",
        ExpressionAttributeNames={"#kind": "kind"},
        ExpressionAttributeValues={":dispatch": dispatch, ":now": "t", ":unassigned": "unassigned"},
    )
    conditional_failed(
        lambda: table.update_item(
            Key=key,
            UpdateExpression="SET outcome = :outcome",
            ConditionExpression="runtime_dispatch = :expected_dispatch AND attribute_not_exists(outcome)",
            ExpressionAttributeValues={":outcome": {"kind": "done"}, ":expected_dispatch": {**dispatch, "generation": 4}},
        )
    )
    table.update_item(
        Key=key,
        UpdateExpression=(
            "SET outcome = :outcome, runtime_dispatch = :terminal, publication_state = :pending, "
            "publication_attempt_state = :ready, publication_error = :empty, updated_at = :now"
        ),
        ConditionExpression="runtime_dispatch = :expected_dispatch AND attribute_not_exists(outcome)",
        ExpressionAttributeValues={
            ":outcome": {"kind": "completed"},
            ":terminal": {"kind": "terminal"},
            ":pending": "pending",
            ":ready": "ready",
            ":empty": "",
            ":now": "t",
            ":expected_dispatch": dict(dispatch),
        },
    )
    allocate = dict(
        Key=key,
        UpdateExpression=(
            "SET publication_attempt = :next, publication_attempt_state = :enqueued, "
            "publication_error = :empty, updated_at = :now"
        ),
        ConditionExpression=(
            "publication_state = :pending AND attribute_exists(outcome) AND "
            "(attribute_not_exists(publication_attempt) OR publication_attempt = :current) AND "
            "(attribute_not_exists(publication_attempt_state) OR "
            "publication_attempt_state IN (:ready, :failed, :cancelled, :stale))"
        ),
        ExpressionAttributeValues={
            ":next": 1,
            ":current": 0,
            ":enqueued": "enqueued",
            ":empty": "",
            ":now": "t",
            ":pending": "pending",
            ":ready": "ready",
            ":failed": "failed",
            ":cancelled": "cancelled",
            ":stale": "stale",
        },
    )
    table.update_item(**allocate)
    conditional_failed(lambda: table.update_item(**allocate))

    for _ in range(2):
        table.update_item(
            Key=key,
            UpdateExpression=(
                "SET #s = :s, phase = :p, finished_at = :t, stop_reason = :r, warnings = :w, #e = :e, "
                "updated_at = :t, retryable = :retryable, interruption = :interruption ADD revision :one"
            ),
            ExpressionAttributeNames={"#s": "status", "#e": "error"},
            ExpressionAttributeValues={
                ":s": "completed",
                ":p": "done",
                ":t": "t",
                ":r": "end_turn",
                ":w": [],
                ":e": None,
                ":retryable": False,
                ":interruption": None,
                ":one": 1,
            },
        )
    snapshot = dict(
        Key=key,
        UpdateExpression="SET revision = :v, progress_snapshot = :ps",
        ConditionExpression="attribute_not_exists(revision) OR revision < :v",
        ExpressionAttributeValues={":v": 2, ":ps": {"items": []}},
    )
    conditional_failed(lambda: table.update_item(**snapshot))
    snapshot["ExpressionAttributeValues"][":v"] = 3
    table.update_item(**snapshot)

    # A value that no expression references is a ValidationException, as in AWS.
    code, response = error_code(
        lambda: table.update_item(
            Key=key,
            UpdateExpression="SET #s = :s",
            ConditionExpression="#s = :running",
            ExpressionAttributeNames={"#s": "status"},
            ExpressionAttributeValues={":s": "x", ":running": "running", ":queued": "queued"},
        )
    )
    check(code == "ValidationException" and ":queued" in response["Error"]["Message"], "unused value rejected")

    table.update_item(Key=key, UpdateExpression="SET pause_requested = :c", ExpressionAttributeValues={":c": True})
    table.update_item(Key=key, UpdateExpression="REMOVE pause_requested")
    run = table.get_item(Key=key, ConsistentRead=True)["Item"]
    check(run["revision"] == 3 and run["error"] is None and "pause_requested" not in run, "run terminal state")
    check(run["started_at_ms"] == Decimal(1767225600123), "epoch ms exact")
    check(run["progress_snapshot"] == {"items": []}, "nested map replaced")

    runs_listed = table.query(KeyConditionExpression="user_id = :owner", ExpressionAttributeValues={":owner": owner})
    check(len(runs_listed["Items"]) == 1, "runs by owner")


# ---------------------------------------------------------------------------
# Session records: value-to-value comparisons, ALL_NEW, dynamic SET/REMOVE.


def sessions(table) -> None:
    key = {"user_id": "system#sessions", "item_id": "session#s1"}
    table.put_item(
        Item={**key, "generation": 1, "executor_id": "e1", "phase": "launching", "endpoint": "http://x"},
        ConditionExpression="attribute_not_exists(item_id)",
    )
    response = table.update_item(
        Key=key,
        UpdateExpression="SET control = :control, schema_version = :schema, open_claims = if_not_exists(open_claims, :zero)",
        ConditionExpression=(
            "generation = :generation AND executor_id = :executor AND "
            "(control.#kind = :expected_control OR (attribute_not_exists(control) AND :expected_control = :accepting)) AND "
            "(open_claims = :claims OR (attribute_not_exists(open_claims) AND :claims = :zero))"
        ),
        ExpressionAttributeNames={"#kind": "kind"},
        ExpressionAttributeValues={
            ":control": {"kind": "accepting"},
            ":schema": 2,
            ":zero": 0,
            ":generation": 1,
            ":executor": "e1",
            ":expected_control": "accepting",
            ":accepting": "accepting",
            ":claims": 0,
        },
        ReturnValues="ALL_NEW",
    )
    attributes = response["Attributes"]
    check(attributes["open_claims"] == 0 and attributes["control"] == {"kind": "accepting"}, "ALL_NEW attributes")

    response = table.update_item(
        Key=key,
        UpdateExpression="SET #f0 = :v0, #f1 = :v1 REMOVE endpoint, lease_owner",
        ConditionExpression=(
            "generation = :generation AND (#phase = :expected_phase OR attribute_not_exists(#phase)) AND "
            "attribute_not_exists(launch_attempt_id)"
        ),
        ExpressionAttributeNames={"#f0": "phase", "#f1": "launch_attempt_id", "#phase": "phase"},
        ExpressionAttributeValues={":v0": "ready", ":v1": "a1", ":generation": 1, ":expected_phase": "launching"},
        ReturnValues="ALL_NEW",
    )
    check(response["Attributes"]["phase"] == "ready" and "endpoint" not in response["Attributes"], "dynamic SET/REMOVE")

    lease = dict(
        Key=key,
        UpdateExpression="SET lease_owner = :owner, lease_expires_at = :expiry",
        ConditionExpression=(
            "generation = :generation AND (#phase = :phase OR attribute_not_exists(#phase)) AND "
            "(attribute_not_exists(lease_expires_at) OR lease_expires_at < :now OR lease_owner = :owner)"
        ),
        ExpressionAttributeNames={"#phase": "phase"},
        ExpressionAttributeValues={":owner": "w1", ":expiry": 2000, ":generation": 1, ":phase": "ready", ":now": 1000},
    )
    table.update_item(**lease)
    lease["ExpressionAttributeValues"][":owner"] = "w2"
    conditional_failed(lambda: table.update_item(**lease))
    lease["ExpressionAttributeValues"][":now"] = 3000
    table.update_item(**lease)

    old = table.update_item(
        Key=key,
        UpdateExpression="SET heartbeat_at = :heartbeat",
        ConditionExpression="generation = :generation AND executor_id = :executor",
        ExpressionAttributeValues={":heartbeat": 5, ":generation": 1, ":executor": "e1"},
        ReturnValues="UPDATED_OLD",
    )
    check("Attributes" not in old, "UPDATED_OLD omits attributes that did not exist")

    # Executor registry: paged Key() query and lease claims.
    for i in range(3):
        table.put_item(
            Item={"user_id": "system#executors", "item_id": f"executor-{i}"},
            ConditionExpression="attribute_not_exists(item_id)",
        )
    executors = query_all(table, KeyConditionExpression=Key("user_id").eq("system#executors"), Limit=1)
    check(len(executors) == 3, "paged executors")
    claim = dict(
        Key={"user_id": "system#executors", "item_id": "executor-0"},
        UpdateExpression="SET recovery_lease_until = :until",
        ConditionExpression=(
            "attribute_exists(item_id) AND attribute_not_exists(recovered_at) AND "
            "(attribute_not_exists(recovery_lease_until) OR recovery_lease_until < :now)"
        ),
        ExpressionAttributeValues={":until": 200, ":now": 100},
    )
    table.update_item(**claim)
    conditional_failed(lambda: table.update_item(**claim))
    table.update_item(
        Key=claim["Key"],
        UpdateExpression="SET recovered_at = :now REMOVE recovery_lease_until",
        ExpressionAttributeValues={":now": 300},
    )

    # Revision registration: idempotent on the same identity.
    revision = {
        "user_id": "system#revisions",
        "item_id": "revision#1.0",
        "image_identifier": "img",
        "image_version": "v1",
    }
    register = dict(
        Item=revision,
        ConditionExpression="attribute_not_exists(item_id) OR (image_identifier = :identifier AND image_version = :version)",
        ExpressionAttributeValues={":identifier": "img", ":version": "v1"},
    )
    table.put_item(**register)
    table.put_item(**register)
    register["ExpressionAttributeValues"] = {":identifier": "img", ":version": "v2"}
    conditional_failed(lambda: table.put_item(**register))


# ---------------------------------------------------------------------------
# Claims: low-level transact_write_items with TypeSerializer values.


def claims(table) -> None:
    serializer = TypeSerializer()

    def ser(value):
        return {k: serializer.serialize(v) for k, v in value.items()}

    session_key = {"user_id": "system#sessions", "item_id": "session#claims"}
    revision = {"image": "img", "version": 1}
    table.put_item(
        Item={
            **session_key,
            "generation": 1,
            "executor_id": "e1",
            "microvm_id": "vm1",
            "desired_power": "running",
            "control": {"kind": "accepting"},
            "revision": revision,
            "hook_acknowledgement": {"ok": True},
            "open_claims": 0,
        }
    )

    def acquire(claim_id: str, expected_revision: dict):
        return client.transact_write_items(
            TransactItems=[
                {
                    "Put": {
                        "TableName": ITEMS,
                        "Item": ser({"user_id": "system#claims", "item_id": f"claim#{claim_id}", "status": "open"}),
                        "ConditionExpression": "attribute_not_exists(item_id)",
                    }
                },
                {
                    "Update": {
                        "TableName": ITEMS,
                        "Key": ser(session_key),
                        "UpdateExpression": "SET open_claims = open_claims + :one",
                        "ConditionExpression": (
                            "generation = :generation AND executor_id = :executor AND microvm_id = :microvm AND "
                            "desired_power = :running AND control.#kind = :accepting AND revision = :revision AND "
                            "hook_acknowledgement = :ack"
                        ),
                        "ExpressionAttributeNames": {"#kind": "kind"},
                        "ExpressionAttributeValues": ser(
                            {
                                ":one": 1,
                                ":generation": 1,
                                ":executor": "e1",
                                ":microvm": "vm1",
                                ":running": "running",
                                ":accepting": "accepting",
                                ":revision": expected_revision,
                                ":ack": {"ok": True},
                            }
                        ),
                    }
                },
            ]
        )

    acquire("c1", revision)
    acquire("c2", revision)
    code, response = error_code(lambda: acquire("c3", {"image": "img", "version": 2}))
    check(code == "TransactionCanceledException", f"cancelled transaction, got {code}")
    reasons = [reason["Code"] for reason in response["CancellationReasons"]]
    check(reasons == ["None", "ConditionalCheckFailed"], f"cancellation reasons {reasons}")
    check(
        "Item" not in table.get_item(Key={"user_id": "system#claims", "item_id": "claim#c3"}),
        "cancelled transaction wrote nothing",
    )
    code, _ = error_code(lambda: acquire("c1", revision))
    check(code == "TransactionCanceledException", "duplicate claim cancelled")

    client.transact_write_items(
        TransactItems=[
            {
                "Update": {
                    "TableName": ITEMS,
                    "Key": ser({"user_id": "system#claims", "item_id": "claim#c1"}),
                    "UpdateExpression": "SET #status = :released, released_at = :now",
                    "ConditionExpression": "#status = :open",
                    "ExpressionAttributeNames": {"#status": "status"},
                    "ExpressionAttributeValues": ser({":released": "released", ":now": 1, ":open": "open"}),
                }
            },
            {
                "Update": {
                    "TableName": ITEMS,
                    "Key": ser(session_key),
                    "UpdateExpression": (
                        "SET open_claims = open_claims - :one, control = :control, #phase = :terminating, "
                        "phase_updated_at = :now REMOVE endpoint"
                    ),
                    "ConditionExpression": "generation = :generation AND executor_id = :executor AND open_claims >= :one",
                    "ExpressionAttributeNames": {"#phase": "phase"},
                    "ExpressionAttributeValues": ser(
                        {
                            ":one": 1,
                            ":control": {"kind": "draining"},
                            ":terminating": "terminating",
                            ":now": 2,
                            ":generation": 1,
                            ":executor": "e1",
                        }
                    ),
                }
            },
        ]
    )
    session = table.get_item(Key=session_key, ConsistentRead=True)["Item"]
    check(session["open_claims"] == 1 and session["phase"] == "terminating", f"released claim {session}")

    got = client.transact_get_items(
        TransactItems=[
            {"Get": {"TableName": ITEMS, "Key": ser(session_key), "ProjectionExpression": "open_claims"}},
            {"Get": {"TableName": ITEMS, "Key": ser({"user_id": "none", "item_id": "none"})}},
        ]
    )
    check(got["Responses"][0]["Item"] == {"open_claims": {"N": "1"}} and got["Responses"][1] == {}, "transact get")


# ---------------------------------------------------------------------------
# Hash-key job records.


def jobs(table) -> None:
    table.put_item(
        Item={"job_id": "j1", "status": "queued", "expires_at": 1767225600, "created_at": 1767225500},
        ConditionExpression="attribute_not_exists(job_id)",
    )
    claim = dict(
        Key={"job_id": "j1"},
        UpdateExpression="SET #s = :running, lambda_request_id = :rid, started_at = :now, heartbeat_at = :now",
        ConditionExpression="(#s = :queued OR lambda_request_id = :rid) AND attribute_not_exists(cancel_requested)",
        ExpressionAttributeNames={"#s": "status"},
        ExpressionAttributeValues={":running": "running", ":rid": "r1", ":now": 10, ":queued": "queued"},
    )
    table.update_item(**claim)
    table.update_item(**claim)  # retried by the same request id
    claim["ExpressionAttributeValues"][":rid"] = "r2"
    try:
        table.update_item(**claim)
        raise AssertionError("second claimant must fail")
    except Exception as exc:  # callers match on the type name / message
        check("ConditionalCheckFailed" in type(exc).__name__ + str(exc), "conditional failure visible in exception")
    touched = table.update_item(
        Key={"job_id": "j1"},
        UpdateExpression="SET heartbeat_at = :now",
        ExpressionAttributeValues={":now": 11},
        ReturnValues="ALL_NEW",
    )["Attributes"]
    check(touched["heartbeat_at"] == 11 and touched["status"] == "running", "heartbeat ALL_NEW")
    table.update_item(
        Key={"job_id": "j1"},
        UpdateExpression="SET #f0 = :v0, #f1 = :v1",
        ExpressionAttributeNames={"#f0": "duration_seconds", "#f1": "results"},
        ExpressionAttributeValues={":v0": Decimal("1.25"), ":v1": ["a", {"b": 1}]},
    )
    job = table.get_item(Key={"job_id": "j1"}, ConsistentRead=True)["Item"]
    check(job["duration_seconds"] == Decimal("1.25") and job["results"] == ["a", {"b": 1}], "result write")


# ---------------------------------------------------------------------------
# Scans, batches, sets.


def scans_and_batches(table) -> None:
    with table.batch_writer() as batch:
        for i in range(30):
            batch.put_item(Item={"user_id": "batch", "item_id": f"b{i:02d}", "item_type": "batch_row", "n": i})
    got = resource.batch_get_item(
        RequestItems={ITEMS: {"Keys": [{"user_id": "batch", "item_id": f"b{i:02d}"} for i in range(5)]}}
    )
    check(len(got["Responses"][ITEMS]) == 5, "batch get")

    rows: list[dict] = []
    kwargs = {"FilterExpression": "item_type = :kind", "ExpressionAttributeValues": {":kind": "batch_row"}, "Limit": 7}
    while True:
        page = table.scan(**kwargs)
        rows.extend(page["Items"])
        if "LastEvaluatedKey" not in page:
            break
        kwargs["ExclusiveStartKey"] = page["LastEvaluatedKey"]
    check(len(rows) == 30, f"paged filtered scan saw {len(rows)}")
    segments = sum(
        table.scan(Segment=s, TotalSegments=4, FilterExpression=Attr("item_type").eq("batch_row"))["Count"]
        for s in range(4)
    )
    check(segments == 30, f"segmented scan saw {segments}")

    key = {"user_id": "batch", "item_id": "b00"}
    table.update_item(Key=key, UpdateExpression="ADD tags :t", ExpressionAttributeValues={":t": {"a", "b"}})
    table.update_item(Key=key, UpdateExpression="ADD tags :t", ExpressionAttributeValues={":t": {"c"}})
    table.update_item(Key=key, UpdateExpression="DELETE tags :t", ExpressionAttributeValues={":t": {"a"}})
    row = table.get_item(Key=key)["Item"]
    check(row["tags"] == {"b", "c"}, f"set ADD/DELETE {row['tags']}")
    check(
        table.scan(FilterExpression=Attr("tags").contains("c") & Attr("n").lt(1))["Count"] == 1,
        "contains on set",
    )
    with table.batch_writer() as batch:
        for i in range(30):
            batch.delete_item(Key={"user_id": "batch", "item_id": f"b{i:02d}"})
    check(table.query(KeyConditionExpression=Key("user_id").eq("batch"))["Count"] == 0, "batch delete")


def main() -> None:
    setup_tables()
    items = resource.Table(ITEMS)
    try:
        folders_and_files(items)
        conversations(items)
        runs(items)
        sessions(items)
        claims(items)
        jobs(resource.Table(JOBS))
        scans_and_batches(items)
    finally:
        for name in (ITEMS, JOBS):
            try:
                client.delete_table(TableName=name)
            except ClientError:
                pass
    code, _ = error_code(lambda: client.describe_table(TableName=ITEMS))
    check(code == "ResourceNotFoundException", "table deleted")
    print(f"boto3 compatibility: {checks} checks passed against {ENDPOINT}")


if __name__ == "__main__":
    main()
