# aws-mocks

Small, deterministic, loopback-only stand-ins for AWS services, for local stacks and
integration tests. Each mock is a Rust library you can embed in a test (start it on
`127.0.0.1:0`, point your SDK at its endpoint) and a binary you can run beside a dev stack.

They cover the calls a typical backend makes, not the whole AWS API. None of them talk to
AWS, and none of them check request signatures.

| Crate | Stands in for | Covers |
| --- | --- | --- |
| [`cognito-mock`](#cognito-mock) | Amazon Cognito user pools | AWS JSON admin, sign-in, password-reset, group and token-revocation calls; JWKS; RS256 access/ID tokens; test routes for issuing tokens and injecting faults |
| [`sqs-mock`](crates/sqs-mock/README.md) | Amazon SQS standard queues | `GetQueueUrl`, `CreateQueue` (configured names), `SendMessage`, `ReceiveMessage` with long polling, `ChangeMessageVisibility`, `DeleteMessage`, `GetQueueAttributes`, `PurgeQueue`; state persisted on disk |
| [`transcribe-mock`](crates/transcribe-mock/README.md) | AWS Transcribe (via a loopback HTTP endpoint) | `POST /transcribe` with 16 kHz PCM answers a fixed transcript, or an empty one for silence |

A `dynamodb-mock` crate is planned.

## Running

```sh
cargo run -p cognito-mock -- --listen 127.0.0.1:9229 --pool-id us-east-1_local --client-id local-client
cargo run -p sqs-mock -- --listen 127.0.0.1:8010 --directory .local/sqs --queue jobs
cargo run -p transcribe-mock -- --listen 127.0.0.1:8005
```

Every binary refuses a non-loopback listen address (`cognito-mock` allows one with
`--allow-container-network`). `--help` lists each binary's flags.

## Depending on it

Pin a git revision:

```toml
[dependencies]
sqs-mock = { git = "https://github.com/wavey-ai/aws-mocks", rev = "<sha>" }
cognito-mock = { git = "https://github.com/wavey-ai/aws-mocks", rev = "<sha>" }
transcribe-mock = { git = "https://github.com/wavey-ai/aws-mocks", rev = "<sha>" }
```

With a crate in your dependency graph, `cargo build -p cognito-mock --bin cognito-mock`
from your own workspace builds its binary into your `target/`.

```rust
let queue = sqs_mock::start(&state_dir, ["jobs"]).await?;
// AWS_ENDPOINT_URL_SQS = queue.endpoint(), queue URL = queue.queue_url()

let cognito = cognito_mock::start(cognito_mock::Config::new("us-east-1_test", "client")).await?;
// AWS_ENDPOINT_URL_COGNITO_IDENTITY_PROVIDER = cognito.endpoint(), issuer = cognito.issuer()

let speech = transcribe_mock::start().await?;
// post PCM to format!("{}/transcribe", speech.endpoint())
```

Each of these runs inside the current Tokio runtime and stops when dropped or `stop()`ped.
`serve(listener, ...)` serves on a listener you have bound yourself.

## cognito-mock

Speaks the AWS JSON protocol on `POST /` (`X-Amz-Target:
AWSCognitoIdentityProviderService.<Operation>`): `InitiateAuth` (`USER_PASSWORD_AUTH`,
`REFRESH_TOKEN_AUTH`), `RespondToAuthChallenge` (`NEW_PASSWORD_REQUIRED`), `GetUser`,
`RevokeToken`, `ListUsers` (email filter), `ForgotPassword`, `ConfirmForgotPassword`,
`AdminGetUser`, `AdminCreateUser`, `AdminSetUserPassword`, `AdminUpdateUserAttributes`,
`AdminDeleteUser`, `AdminDisableUser`, `AdminListGroupsForUser`, `AdminAddUserToGroup`,
`AdminRemoveUserFromGroup` and `CreateGroup`.

It serves the pool's keys at `GET /<pool id>/.well-known/jwks.json` and `GET /health`. Tokens
are RS256, signed by a key generated on start, with issuer `http://<listen>/<pool id>` unless
`--issuer` says otherwise.

Test routes:

| Route | Does |
| --- | --- |
| `POST /local/issue` | Creates or replaces an account (`sub`, `email`, optional `password`, `groups`, `enabled`, `user_status`, `email_verified`, `given_name`, `family_name`, `expires_in_seconds`) and returns `access_token`, `refresh_token`, `reset_code` and `kid` |
| `POST /local/revoke` | Revokes an issued `access_token` |
| `POST /local/groups` | Sets a `username`'s `groups` |
| `POST /local/rotate` | Adds a signing key; new tokens use it, the JWKS lists both |
| `POST /local/faults` | Makes the JWKS or `GetUser` unavailable, or fails named operations with a given `__type` (`forgot_error`, `confirm_error`, `auth_error`, `groups_error`, `add_group_error`, `create_error`, `resend_error`, `set_password_error`, `delete_error`, `list_user_error`) |
| `GET /local/metrics` | Call counts per operation and the number of accounts |

`--state-file` keeps accounts and groups (not keys or sessions) across restarts.

`--compat-mode` loosens the fixture for applications that bootstrap their own users
against it: `ListUserPools` and `ListUserPoolClients` discovery (reporting `--pool-name` and
`--client-name`, default `local`), `AdminEnableUser`, email aliases wherever a `Username` is
taken, a caller-chosen `sub` and `TemporaryPassword` on `AdminCreateUser` with any delivery
(such accounts confirm password resets with `--reset-code`, default `246810`), `sub` in
`AdminGetUser`, `REFRESH_TOKEN` as an alias of `REFRESH_TOKEN_AUTH`, `"fixture":"rust-cognito"`
in `/health`, and a unique key id on every start so cached JWKS never match a restarted
fixture by id alone.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
