# sqs-mock

Serves named SQS standard queues over the AWS JSON protocol: `GetQueueUrl`, `CreateQueue`,
`SendMessage`, `ReceiveMessage` (long polling, visibility timeouts, receive counts),
`ChangeMessageVisibility`, `DeleteMessage`, `GetQueueAttributes` and `PurgeQueue`. Messages
persist in `<directory>/state.json`.

```sh
sqs-mock --listen 127.0.0.1:8010 --directory .local/sqs --queue jobs --queue events
```

Queue URLs are `http://<listen>/000000000000/<name>`; `--queue` defaults to `local-queue`.

`CreateQueue` creates a queue that does not exist yet and returns its URL; calling it again
returns the same URL, as SQS does. Created queues persist in `state.json` and are served again
after a restart. `GetQueueUrl` still answers `NonExistentQueue` for a name never created.
