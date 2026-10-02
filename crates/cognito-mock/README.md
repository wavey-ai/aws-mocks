# cognito-mock

Answers the Cognito Identity Provider JSON protocol on `POST /`: `InitiateAuth`
(`USER_PASSWORD_AUTH`, `REFRESH_TOKEN_AUTH`), `RespondToAuthChallenge`, `GetUser`, `RevokeToken`,
`ListUsers`, `ForgotPassword`, `ConfirmForgotPassword`, `CreateGroup` and the `Admin*` user and
group calls. Tokens are RS256, signed by a key generated at start; the pool's keys are at
`GET /<pool id>/.well-known/jwks.json`.

```sh
cognito-mock --listen 127.0.0.1:9229 --pool-id us-east-1_local --client-id local-client
```

Test routes: `POST /local/issue` (create an account and return its tokens), `/local/revoke`,
`/local/groups`, `/local/rotate` (add a signing key), `/local/faults` (fail named operations) and
`GET /local/metrics`.

Options:

- `--state-file` keeps accounts and groups across restarts.
- `--compat-mode` suits applications that bootstrap their own users: pool and client discovery
  (named by `--pool-name` and `--client-name`), `AdminEnableUser`, email aliases for usernames,
  a caller-chosen `sub` and temporary password on `AdminCreateUser`, password resets confirmed
  with `--reset-code` (default `246810`), and a fresh key id on every start.
- `--issuer` sets the token issuer (default `http://<listen>/<pool id>`).
