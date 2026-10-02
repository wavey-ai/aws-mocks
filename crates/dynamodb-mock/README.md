# dynamodb-mock

An in-memory DynamoDB that answers the JSON 1.0 protocol on `POST /`. It covers tables with
global and local secondary indexes, `GetItem`/`PutItem`/`UpdateItem`/`DeleteItem` with condition
and update expressions and every `ReturnValues` mode, `Query` and `Scan` with filters,
projections and paging, `BatchGetItem`/`BatchWriteItem`, and atomic
`TransactWriteItems`/`TransactGetItems`. Numbers are exact 38-digit decimals.

```sh
dynamodb-mock --listen 127.0.0.1:8003
```

`tests/boto3_compat.py` checks it against boto3:
`DYNAMODB_ENDPOINT=http://127.0.0.1:8003 python crates/dynamodb-mock/tests/boto3_compat.py`.
