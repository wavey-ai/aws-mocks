# sqs-mock

A loopback Amazon SQS fixture serving a fixed set of named standard queues over the AWS JSON
protocol (`X-Amz-Target: AmazonSQS.*`, as used by current AWS SDKs and boto3). It implements
`GetQueueUrl`, `SendMessage`, `ReceiveMessage` (long polling, visibility timeouts, receive
counts), `ChangeMessageVisibility` and `DeleteMessage`, plus `CreateQueue` (returning a
configured queue's URL), `GetQueueAttributes` (`ApproximateNumberOfMessages`) and
`PurgeQueue` for inspection. `GET /health` answers `{"fixture":"rust-sqs"}`.

Messages persist in `<directory>/state.json` across restarts.

```sh
cargo build --locked -p sqs-mock
target/debug/sqs-mock --listen 127.0.0.1:8010 --directory .local/sqs --queue jobs --queue events
```

Queue URLs are `http://<listen>/000000000000/<name>`. `--queue` defaults to `local-queue`.
Only loopback listeners are accepted. It is not a general SQS emulator: no FIFO queues,
batches, dead-letter queues or message attributes.

From Rust:

```rust
let queue = sqs_mock::start(&state_dir, ["jobs"]).await?; // 127.0.0.1:0
let endpoint = queue.endpoint(); // AWS_ENDPOINT_URL_SQS
let url = queue.queue_url();     // the first queue's URL
```
