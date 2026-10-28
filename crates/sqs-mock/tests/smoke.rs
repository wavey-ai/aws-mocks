use serde_json::{Value, json};
use std::{net::SocketAddr, path::PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "sqs-mock-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

async fn call(address: SocketAddr, action: &str, body: Value) -> (u16, Value) {
    let body = body.to_string();
    let mut stream = TcpStream::connect(address).await.unwrap();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {address}\r\nX-Amz-Target: AmazonSQS.{action}\r\n\
         Content-Type: application/x-amz-json-1.0\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let (_, payload) = response.split_once("\r\n\r\n").unwrap();
    (status, serde_json::from_str(payload).unwrap())
}

#[tokio::test]
async fn sends_receives_and_deletes_a_message() {
    let directory = scratch("roundtrip");
    let queue = sqs_mock::start(&directory, ["jobs"]).await.unwrap();
    let address = queue.address();
    let (status, found) = call(address, "GetQueueUrl", json!({"QueueName": "jobs"})).await;
    assert_eq!(status, 200);
    assert_eq!(found["QueueUrl"], queue.queue_url());
    assert!(queue.queue_url().ends_with("/000000000000/jobs"));

    let url = queue.queue_url();
    let (status, _) = call(
        address,
        "SendMessage",
        json!({"QueueUrl": url, "MessageBody": "hello"}),
    )
    .await;
    assert_eq!(status, 200);
    let (_, received) = call(
        address,
        "ReceiveMessage",
        json!({"QueueUrl": url, "WaitTimeSeconds": 1}),
    )
    .await;
    let message = &received["Messages"][0];
    assert_eq!(message["Body"], "hello");
    assert_eq!(message["Attributes"]["ApproximateReceiveCount"], "1");
    let (status, _) = call(
        address,
        "DeleteMessage",
        json!({"QueueUrl": url, "ReceiptHandle": message["ReceiptHandle"]}),
    )
    .await;
    assert_eq!(status, 200);
    let (_, attributes) = call(address, "GetQueueAttributes", json!({"QueueUrl": url})).await;
    assert_eq!(attributes["Attributes"]["ApproximateNumberOfMessages"], "0");
    queue.stop();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn serves_only_the_configured_queues() {
    let directory = scratch("queues");
    let queue = sqs_mock::start(&directory, ["first", "second"])
        .await
        .unwrap();
    let address = queue.address();
    let second = queue.queue_url_of("second").unwrap();
    assert!(queue.queue_url_of("third").is_none());
    let (status, _) = call(
        address,
        "SendMessage",
        json!({"QueueUrl": second, "MessageBody": "two"}),
    )
    .await;
    assert_eq!(status, 200);
    let (_, first) = call(
        address,
        "ReceiveMessage",
        json!({"QueueUrl": queue.queue_url()}),
    )
    .await;
    assert_eq!(first, json!({}));
    let (_, received) = call(address, "ReceiveMessage", json!({"QueueUrl": second})).await;
    assert_eq!(received["Messages"][0]["Body"], "two");
    let (status, unknown) = call(address, "GetQueueUrl", json!({"QueueName": "third"})).await;
    assert_eq!(status, 400);
    assert_eq!(
        unknown["__type"],
        "com.amazonaws.sqs#AWS.SimpleQueueService.NonExistentQueue"
    );
    queue.stop();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn create_queue_creates_unknown_queues_and_is_idempotent() {
    let directory = scratch("create");
    let queue = sqs_mock::start(&directory, ["first"]).await.unwrap();
    let address = queue.address();
    let (status, created) = call(address, "CreateQueue", json!({"QueueName": "made-later"})).await;
    assert_eq!(status, 200);
    let url = created["QueueUrl"].as_str().unwrap().to_owned();
    assert_eq!(url, sqs_mock::queue_url(address, "made-later"));
    assert_eq!(queue.queue_url_of("made-later"), Some(url.clone()));
    let (_, again) = call(address, "CreateQueue", json!({"QueueName": "made-later"})).await;
    assert_eq!(again["QueueUrl"], url);
    let (_, looked_up) = call(address, "GetQueueUrl", json!({"QueueName": "made-later"})).await;
    assert_eq!(looked_up["QueueUrl"], url);
    let (status, _) = call(
        address,
        "SendMessage",
        json!({"QueueUrl": url, "MessageBody": "late"}),
    )
    .await;
    assert_eq!(status, 200);
    let (_, received) = call(address, "ReceiveMessage", json!({"QueueUrl": url})).await;
    assert_eq!(received["Messages"][0]["Body"], "late");
    let (status, invalid) = call(address, "CreateQueue", json!({"QueueName": "no spaces"})).await;
    assert_eq!(status, 400);
    assert_eq!(invalid["__type"], "com.amazonaws.sqs#InvalidParameterValue");
    queue.stop();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn created_queues_survive_a_restart() {
    let directory = scratch("create-restart");
    let queue = sqs_mock::start(&directory, ["first"]).await.unwrap();
    let (status, _) = call(
        queue.address(),
        "CreateQueue",
        json!({"QueueName": "made-later"}),
    )
    .await;
    assert_eq!(status, 200);
    queue.stop();
    let queue = sqs_mock::start(&directory, ["first"]).await.unwrap();
    assert!(queue.queue_url_of("made-later").is_some());
    let (status, _) = call(
        queue.address(),
        "GetQueueUrl",
        json!({"QueueName": "made-later"}),
    )
    .await;
    assert_eq!(status, 200);
    queue.stop();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn single_queue_state_loads_into_the_first_queue() {
    let directory = scratch("legacy");
    std::fs::write(
        directory.join("state.json"),
        json!({"messages": [{
            "id": "m1", "body": "kept", "sent_ms": 1, "visible_ms": 1,
            "receives": 0, "receipt": null
        }]})
        .to_string(),
    )
    .unwrap();
    let queue = sqs_mock::start(&directory, ["restored"]).await.unwrap();
    let (_, received) = call(
        queue.address(),
        "ReceiveMessage",
        json!({"QueueUrl": queue.queue_url()}),
    )
    .await;
    assert_eq!(received["Messages"][0]["Body"], "kept");
    queue.stop();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn rejects_an_empty_queue_list() {
    let directory = scratch("empty");
    assert!(
        sqs_mock::start(&directory, Vec::<String>::new())
            .await
            .is_err()
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn a_failed_delete_leaves_the_message_to_be_delivered_again() {
    let directory = scratch("delete-faults");
    let queue = sqs_mock::start(&directory, ["jobs"]).await.unwrap();
    let address = queue.address();
    let url = queue.queue_url();
    let (_, sent) = call(
        address,
        "SendMessage",
        json!({"QueueUrl": url, "MessageBody": "once"}),
    )
    .await;
    let id = sent["MessageId"].as_str().unwrap().to_owned();

    queue.fail_next_deletes(1);
    let (_, first) = call(
        address,
        "ReceiveMessage",
        json!({"QueueUrl": url, "VisibilityTimeout": 1}),
    )
    .await;
    let receipt = first["Messages"][0]["ReceiptHandle"].clone();
    let (status, _) = call(
        address,
        "DeleteMessage",
        json!({"QueueUrl": url, "ReceiptHandle": receipt}),
    )
    .await;
    assert_eq!(status, 500);

    // Back once its visibility timeout passes, with the receive counted.
    let (_, again) = call(
        address,
        "ReceiveMessage",
        json!({"QueueUrl": url, "WaitTimeSeconds": 3}),
    )
    .await;
    assert_eq!(again["Messages"][0]["MessageId"], id);
    assert_eq!(
        again["Messages"][0]["Attributes"]["ApproximateReceiveCount"],
        "2"
    );
    let (status, _) = call(
        address,
        "DeleteMessage",
        json!({"QueueUrl": url, "ReceiptHandle": again["Messages"][0]["ReceiptHandle"]}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        queue.receives(&id),
        2,
        "the receive count survives the delete"
    );
    assert_eq!(queue.received().len(), 1);
    queue.stop();
    std::fs::remove_dir_all(directory).unwrap();
}
