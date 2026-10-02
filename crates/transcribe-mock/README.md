# transcribe-mock

A loopback endpoint that stands in for AWS Transcribe. `POST /transcribe` takes 16 kHz s16le
PCM (`Content-Type: audio/pcm; rate=16000; encoding=s16le`, up to 60 seconds) and answers
`{"text":"Local voice fixture"}`, or `{"text":""}` for silence. `GET /health` answers
`{"status":"ok","fixture":"pcm16"}`. Malformed requests get FastAPI-style 400, 413 and 415
responses, so it can replace a Python fixture with the same contract.

```sh
transcribe-mock --listen 127.0.0.1:8005
```

`--listen` defaults to `127.0.0.1:$TRANSCRIBE_PROXY_PORT`, then `127.0.0.1:8005`.
