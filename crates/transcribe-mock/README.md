# transcribe-mock

A loopback stand-in for AWS Transcribe, for applications that can send recordings to an
HTTP endpoint instead of opening a Transcribe stream in local stacks and integration runs.

It does not recognise speech. It answers with fixed text:

| Request | Response |
| --- | --- |
| `Host` other than `localhost`, `127.0.0.1`, `[::1]` or `testserver` (any port) | 400 `Invalid host header` (text/plain) |
| `GET /health` | 200 `{"status":"ok","fixture":"pcm16"}` |
| `POST /transcribe` with `Content-Type` not `audio/pcm; rate=16000; encoding=s16le` (case-insensitive, otherwise exact) | 415 `{"detail":"Unsupported audio format"}` |
| `POST /transcribe` with a body over 1,920,000 bytes (60 s of 16 kHz s16le) | 413 `{"detail":"Recording is too large"}` |
| `POST /transcribe` with an empty or odd-length body | 400 `{"detail":"Malformed PCM"}` |
| `POST /transcribe` with all-zero PCM (silence) | 200 `{"text":""}` |
| `POST /transcribe` with any non-zero byte | 200 `{"text":"Local voice fixture"}` |
| Wrong method on either route | 405 `{"detail":"Method Not Allowed"}` with `Allow: GET` or `Allow: POST` |
| A route with trailing slashes, such as `/health/` | 307 to the path without them |
| Anything else | 404 `{"detail":"Not Found"}` |

Responses follow FastAPI/Starlette conventions, so it can replace a Python fixture with the
same contract. The fixture checks the content type before it reads the body. It only listens
on loopback and never stores or logs recordings. `/transcribe` requests get a tracing span
with `http.route`; `/health` gets none.

## From Rust

```rust
let speech = transcribe_mock::start().await?; // 127.0.0.1:0
// Point the application at speech.endpoint(); dropping `speech` stops the fixture.
```

## Standalone

```sh
cargo build --locked -p transcribe-mock
target/debug/transcribe-mock --listen 127.0.0.1:8005
```

`--listen` defaults to `127.0.0.1:$TRANSCRIBE_PROXY_PORT`, or `127.0.0.1:8005` when that
variable is unset. The binary prints `Local Transcribe proxy ready: http://127.0.0.1:8005`
once it is listening. `RUST_LOG` overrides the default log filter.

To start it only when nothing healthy already answers on the port:

```sh
if ! curl -fsS http://127.0.0.1:8005/health | grep -q '"fixture":"pcm16"'; then
  mkdir -p .local
  nohup target/debug/transcribe-mock --listen 127.0.0.1:8005 \
    >> .local/transcribe-mock.log 2>&1 &
  echo $! > .local/transcribe-mock.pid
  for _ in $(seq 60); do
    curl -fsS http://127.0.0.1:8005/health >/dev/null 2>&1 && break
    sleep 0.25
  done
fi
```
