# sqs-mock

Serves named SQS standard queues over the AWS JSON protocol: `GetQueueUrl`, `CreateQueue`,
`SendMessage`, `ReceiveMessage` (long polling, visibility timeouts, receive counts),
`ChangeMessageVisibility`, `DeleteMessage`, `GetQueueAttributes` and `PurgeQueue`. Messages
persist in `<directory>/state.json`.

```sh
sqs-mock --listen 127.0.0.1:8010 --directory .local/sqs --queue jobs --queue events
```

Queue URLs are `http://<listen>/000000000000/<name>`; `--queue` defaults to `local-queue`.
