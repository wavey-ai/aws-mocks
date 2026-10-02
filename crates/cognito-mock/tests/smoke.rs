use cognito_mock::Config;
use serde_json::{Value, json};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn call(
    address: SocketAddr,
    method: &str,
    path: &str,
    target: Option<&str>,
    body: Value,
) -> (u16, Value) {
    let body = body.to_string();
    let target = target
        .map(|target| format!("X-Amz-Target: AWSCognitoIdentityProviderService.{target}\r\n"))
        .unwrap_or_default();
    let mut stream = TcpStream::connect(address).await.unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{target}\
         Content-Type: application/x-amz-json-1.1\r\nContent-Length: {}\r\n\
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
async fn issues_a_token_that_get_user_accepts() {
    let cognito = cognito_mock::start(Config::new("us-east-1_test", "test-client"))
        .await
        .unwrap();
    let address = cognito.address();
    assert_eq!(cognito.issuer(), format!("http://{address}/us-east-1_test"));
    let (status, health) = call(address, "GET", "/health", None, json!({})).await;
    assert_eq!(status, 200);
    assert_eq!(health, json!({"status": "ok", "issuer": cognito.issuer()}));
    let (status, jwks) = call(
        address,
        "GET",
        "/us-east-1_test/.well-known/jwks.json",
        None,
        json!({}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(jwks["keys"][0]["kid"], "fixture-key-1");

    let (status, issued) = call(
        address,
        "POST",
        "/local/issue",
        None,
        json!({"sub": "user-1", "email": "user@example.test"}),
    )
    .await;
    assert_eq!(status, 200);
    let (status, user) = call(
        address,
        "POST",
        "/",
        Some("GetUser"),
        json!({"AccessToken": issued["access_token"]}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(user["Username"], "user-1");
    // Strict mode rejects the compatibility-only discovery call.
    let (status, _) = call(address, "POST", "/", Some("ListUserPools"), json!({})).await;
    assert_eq!(status, 400);
    cognito.stop();
}

#[tokio::test]
async fn compat_mode_reports_names_and_reset_code() {
    let cognito = cognito_mock::start(Config {
        compat_mode: true,
        pool_name: "pool-name".into(),
        client_name: "client-name".into(),
        reset_code: "135790".into(),
        ..Config::new("us-east-1_compat", "compat-client")
    })
    .await
    .unwrap();
    let address = cognito.address();
    let (_, health) = call(address, "GET", "/health", None, json!({})).await;
    assert_eq!(health["fixture"], "rust-cognito");
    let (_, pools) = call(
        address,
        "POST",
        "/",
        Some("ListUserPools"),
        json!({"MaxResults": 10}),
    )
    .await;
    assert_eq!(
        pools,
        json!({"UserPools": [{"Id": "us-east-1_compat", "Name": "pool-name"}]})
    );
    let (_, clients) = call(
        address,
        "POST",
        "/",
        Some("ListUserPoolClients"),
        json!({"UserPoolId": "us-east-1_compat"}),
    )
    .await;
    assert_eq!(clients["UserPoolClients"][0]["ClientName"], "client-name");

    let (status, _) = call(
        address,
        "POST",
        "/",
        Some("AdminCreateUser"),
        json!({
            "UserPoolId": "us-east-1_compat", "Username": "new@example.test",
            "MessageAction": "SUPPRESS", "TemporaryPassword": "Temporary1",
            "UserAttributes": [{"Name": "sub", "Value": "chosen-sub"}],
        }),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = call(
        address,
        "POST",
        "/",
        Some("AdminSetUserPassword"),
        json!({"UserPoolId": "us-east-1_compat", "Username": "new@example.test",
               "Password": "Permanent1", "Permanent": true}),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = call(
        address,
        "POST",
        "/",
        Some("ConfirmForgotPassword"),
        json!({"ClientId": "compat-client", "Username": "new@example.test",
               "ConfirmationCode": "135790", "Password": "Changed12"}),
    )
    .await;
    assert_eq!(status, 200);
    cognito.stop();
}

#[tokio::test]
async fn refuses_non_loopback_listeners() {
    let config = Config::new("us-east-1_test", "test-client");
    assert!(
        cognito_mock::listen("0.0.0.0:0".parse().unwrap(), config)
            .await
            .is_err()
    );
}
