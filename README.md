# aws-mocks

Small, deterministic stand-ins for AWS services, for local stacks and integration tests. Each
mock is a Rust library you can start inside a test and a binary you can run beside a dev stack.
They listen on loopback and answer the calls a typical backend makes.

| Crate | Stands in for |
| --- | --- |
| [`cognito-mock`](crates/cognito-mock/README.md) | Cognito user pools: sign-in, admin calls, groups, JWKS and RS256 tokens |
| [`dynamodb-mock`](crates/dynamodb-mock/README.md) | DynamoDB: tables with indexes, expressions, queries, batches and transactions, in memory |
| [`sqs-mock`](crates/sqs-mock/README.md) | SQS standard queues, persisted to disk |
| [`transcribe-mock`](crates/transcribe-mock/README.md) | Transcribe over a loopback HTTP endpoint, with a fixed transcript |

## Run

```sh
cargo run -p cognito-mock -- --listen 127.0.0.1:9229 --pool-id us-east-1_local --client-id local-client
cargo run -p dynamodb-mock -- --listen 127.0.0.1:8003
cargo run -p sqs-mock -- --listen 127.0.0.1:8010 --directory .local/sqs --queue jobs
cargo run -p transcribe-mock -- --listen 127.0.0.1:8005
```

`--help` lists each binary's flags. Point your AWS SDK at the address with the usual
`AWS_ENDPOINT_URL_<SERVICE>` variables.

## Use from Cargo

```toml
[dev-dependencies]
sqs-mock = { git = "https://github.com/wavey-ai/aws-mocks", rev = "<sha>" }
```

```rust
let queue = sqs_mock::start(&state_dir, ["jobs"]).await?;
let cognito = cognito_mock::start(cognito_mock::Config::new("us-east-1_test", "client")).await?;
let dynamodb = dynamodb_mock::start().await?;
let speech = transcribe_mock::start().await?;
```

Each `start` binds `127.0.0.1:0` in the current Tokio runtime, exposes `endpoint()`, and stops
when dropped. With a mock in your dependency graph, `cargo build -p <mock> --bin <mock>` builds
its binary into your own `target/`.

## Develop

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

Apache-2.0 or MIT, at your option ([LICENSE-APACHE](LICENSE-APACHE), [LICENSE-MIT](LICENSE-MIT)).
