//! Loopback Amazon Cognito user pool fixture.
//!
//! It speaks the AWS JSON protocol (`X-Amz-Target: AWSCognitoIdentityProviderService.*`) for
//! the admin, sign-in, password-reset and group calls a typical backend makes, serves the
//! pool's JWKS at `/<pool id>/.well-known/jwks.json`, and signs RS256 access and ID tokens with
//! a key generated on start. Test-only `/local/*` routes issue tokens, revoke them, set
//! groups, rotate the signing key, inject faults and report call counts.
//!
//! The `cognito-mock` binary serves it on a given address; tests embed it with [`start`] on an
//! ephemeral loopback port, or hand a bound listener to [`serve`].

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, body::Incoming, header};
use hyper_util::rt::TokioIo;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rand::{RngCore, rngs::OsRng};
use rsa::{
    RsaPrivateKey,
    pkcs8::{EncodePrivateKey, LineEnding},
    traits::PublicKeyParts,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    io::Write,
    net::{Ipv4Addr, SocketAddr},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, task::JoinHandle};
use tracing::warn;
use uuid::Uuid;

/// The pool and client name compatibility mode reports from `ListUserPools` and
/// `ListUserPoolClients` unless [`Config::pool_name`] / [`Config::client_name`] say otherwise.
pub const DEFAULT_NAME: &str = "local";
/// The confirmation code accounts created by `AdminCreateUser` in compatibility mode accept for
/// `ConfirmForgotPassword`, unless [`Config::reset_code`] says otherwise.
pub const DEFAULT_RESET_CODE: &str = "246810";
/// The password an `AdminCreateUser` in compatibility mode sets when the caller sends no
/// `TemporaryPassword`.
pub const DEFAULT_TEMPORARY_PASSWORD: &str = "LocalDev123!";

/// How the fixture behaves.
#[derive(Clone, Debug)]
pub struct Config {
    /// The user pool id; requests must name it and the JWKS is served under it.
    pub pool_id: String,
    /// The app client id; sign-in requests must name it and tokens carry it.
    pub client_id: String,
    /// The token issuer. Defaults to `http://<listen address>/<pool id>`.
    pub issuer: Option<String>,
    /// Keep accounts and groups (not keys or sessions) in this file across restarts.
    pub state_file: Option<PathBuf>,
    /// Compatibility mode, closer to `cognito-local`-style emulators and to what an
    /// application bootstrapping its own users expects:
    /// `ListUserPools`/`ListUserPoolClients` discovery (reporting [`Config::pool_name`] and
    /// [`Config::client_name`]), `AdminEnableUser`, email aliases wherever a `Username` is
    /// taken, a caller-chosen `sub` and `TemporaryPassword` on `AdminCreateUser` with any
    /// delivery (the account accepts [`Config::reset_code`]), `sub` in `AdminGetUser`, the
    /// `REFRESH_TOKEN` auth flow alias, a `fixture` name in `/health`, and a fresh key id on
    /// every start. Off, the fixture is stricter and only accepts the calls a backend's
    /// production code paths make.
    pub compat_mode: bool,
    /// The pool name compatibility mode lists.
    pub pool_name: String,
    /// The client name compatibility mode lists.
    pub client_name: String,
    /// The `ConfirmForgotPassword` code for accounts created in compatibility mode.
    pub reset_code: String,
}

impl Config {
    /// A strict-mode fixture for `pool_id` and `client_id`, with the default names and code.
    pub fn new(pool_id: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            pool_id: pool_id.into(),
            client_id: client_id.into(),
            issuer: None,
            state_file: None,
            compat_mode: false,
            pool_name: DEFAULT_NAME.into(),
            client_name: DEFAULT_NAME.into(),
            reset_code: DEFAULT_RESET_CODE.into(),
        }
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.pool_id.is_empty() && !self.pool_id.contains('/'),
            "invalid pool id"
        );
        anyhow::ensure!(!self.client_id.is_empty(), "invalid client id");
        Ok(())
    }
}

struct Key {
    kid: String,
    encoding: EncodingKey,
    n: String,
    e: String,
}

impl Key {
    /// `unique` (compatibility mode) makes the key id differ on every start, so a client's cached JWKS
    /// never matches a restarted fixture's key by id alone.
    fn generate(number: usize, unique: bool) -> Result<Self> {
        let private = RsaPrivateKey::new(&mut OsRng, 2048).context("generate fixture RSA key")?;
        let public = private.to_public_key();
        let pem = private
            .to_pkcs8_pem(LineEnding::LF)
            .context("encode fixture RSA key")?;
        Ok(Self {
            kid: if unique {
                format!("fixture-key-{number}-{}", Uuid::now_v7())
            } else {
                format!("fixture-key-{number}")
            },
            encoding: EncodingKey::from_rsa_pem(pem.as_bytes()).context("load fixture RSA key")?,
            n: URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            e: URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
        })
    }

    fn public_jwk(&self) -> Value {
        json!({"kid":self.kid,"kty":"RSA","alg":"RS256","use":"sig","n":self.n,"e":self.e})
    }
}

#[derive(Clone)]
struct Identity {
    sub: String,
    email: String,
    email_verified: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Account {
    email: String,
    enabled: bool,
    status: String,
    reset_code: String,
    password: String,
    groups: Vec<String>,
    email_verified: bool,
    given_name: Option<String>,
    family_name: Option<String>,
}

struct State {
    keys: Vec<Key>,
    issued: HashMap<String, Identity>,
    accounts: HashMap<String, Account>,
    groups: HashSet<String>,
    revoked: HashSet<String>,
    issued_refresh: HashSet<String>,
    refresh_users: HashMap<String, String>,
    revoked_refresh: HashSet<String>,
    challenges: HashMap<String, String>,
    jwks_requests: u64,
    get_user_requests: u64,
    revoke_requests: u64,
    forgot_requests: u64,
    confirm_requests: u64,
    list_user_requests: u64,
    jwks_fault: bool,
    get_user_fault: bool,
    forgot_error: Option<String>,
    confirm_error: Option<String>,
    auth_error: Option<String>,
    groups_error: Option<String>,
    add_group_error: Option<String>,
    create_error: Option<String>,
    resend_error: Option<String>,
    set_password_error: Option<String>,
    delete_error: Option<String>,
    list_user_error: Option<String>,
    create_requests: u64,
    resend_requests: u64,
    delete_requests: u64,
}

/// What `--state-file` keeps across restarts.
#[derive(Default, Serialize, Deserialize)]
struct SavedAccounts {
    accounts: HashMap<String, Account>,
    groups: HashSet<String>,
}

struct App {
    state_file: Option<PathBuf>,
    compat_mode: bool,
    issuer: String,
    pool_id: String,
    client_id: String,
    pool_name: String,
    client_name: String,
    reset_code: String,
    state: Mutex<State>,
}

type Reply = Response<Full<Bytes>>;

fn reply(status: StatusCode, body: Value) -> Reply {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
        .unwrap()
}

fn bad_request(detail: &str) -> Reply {
    reply(StatusCode::BAD_REQUEST, json!({"detail":detail}))
}

async fn body_json(request: Request<Incoming>) -> Result<Value> {
    let body = request.into_body().collect().await?.to_bytes();
    anyhow::ensure!(body.len() <= 32 * 1024, "fixture request exceeds 32 KiB");
    Ok(serde_json::from_slice(&body)?)
}

#[derive(Deserialize)]
struct Issue {
    sub: String,
    email: String,
    #[serde(default)]
    expires_in_seconds: Option<i64>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    user_status: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    given_name: Option<String>,
    #[serde(default)]
    family_name: Option<String>,
}

fn issue_session(app: &App, state: &mut State, username: &str, rotate_refresh: bool) -> Value {
    let Some(account) = state.accounts.get(username) else {
        return json!({});
    };
    let email = account.email.clone();
    let email_verified = account.email_verified;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let key = state.keys.last().unwrap();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(key.kid.clone());
    let mut nonce = [0_u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let jti = URL_SAFE_NO_PAD.encode(nonce);
    let access = encode(
        &header,
        &json!({"iss":app.issuer,"sub":username,"client_id":app.client_id,
                 "token_use":"access","iat":now,"exp":now + 3600,"jti":jti}),
        &key.encoding,
    )
    .unwrap();
    let id = encode(
        &header,
        &json!({"iss":app.issuer,"sub":username,"aud":app.client_id,
                 "token_use":"id","email":email,"iat":now,"exp":now + 3600}),
        &key.encoding,
    )
    .unwrap();
    state.issued.insert(
        access.clone(),
        Identity {
            sub: username.to_owned(),
            email,
            email_verified,
        },
    );
    let mut result =
        json!({"AccessToken":access,"IdToken":id,"ExpiresIn":3600,"TokenType":"Bearer"});
    if rotate_refresh {
        let mut bytes = [0_u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let refresh = URL_SAFE_NO_PAD.encode(bytes);
        state.issued_refresh.insert(refresh.clone());
        state
            .refresh_users
            .insert(refresh.clone(), username.to_owned());
        result["RefreshToken"] = json!(refresh);
    }
    result
}

/// Handle one request, then persist accounts and groups after a successful POST when
/// `--state-file` is set.
async fn handle(app: Arc<App>, request: Request<Incoming>) -> Reply {
    // Reads (GetUser runs on every authenticated backend request) change no account.
    let read_only = request
        .headers()
        .get("x-amz-target")
        .and_then(|target| target.to_str().ok())
        .and_then(|target| target.rsplit('.').next())
        .is_some_and(|operation| matches!(operation, "GetUser" | "AdminGetUser" | "ListUsers"));
    let changed = request.method() == Method::POST && !read_only;
    let response = handle_request(app.clone(), request).await;
    if changed
        && response.status().is_success()
        && let Some(path) = &app.state_file
        && let Err(error) = save_accounts(&app, path)
    {
        warn!(%error, "Fixture account persistence failed");
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"detail":"Account persistence failed"}),
        );
    }
    response
}

fn save_accounts(app: &App, path: &std::path::Path) -> Result<()> {
    // Held through the rename: concurrent saves share one temporary file.
    let state = app.state.lock().unwrap();
    let saved = SavedAccounts {
        accounts: state.accounts.clone(),
        groups: state.groups.clone(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(&saved)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

/// Compatibility-mode `AdminCreateUser`: any delivery (owner, invitation or `RESEND`), the
/// caller's `sub` as the username, and `TemporaryPassword` (default
/// [`DEFAULT_TEMPORARY_PASSWORD`]) as the password.
fn compat_create_user(app: &App, body: &Value) -> Reply {
    let email = body["Username"].as_str().unwrap_or("");
    if body["UserPoolId"].as_str() != Some(app.pool_id.as_str()) || !email.contains('@') {
        return bad_request("Invalid local user creation request");
    }
    let mut state = app.state.lock().unwrap();
    state.create_requests += 1;
    if let Some(error) = &state.create_error {
        return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
    }
    let password = body["TemporaryPassword"]
        .as_str()
        .unwrap_or(DEFAULT_TEMPORARY_PASSWORD);
    let existing = state
        .accounts
        .iter()
        .find(|(_, account)| account.email.eq_ignore_ascii_case(email))
        .map(|(sub, _)| sub.clone());
    if body["MessageAction"].as_str() == Some("RESEND") {
        let Some(username) = existing else {
            return reply(
                StatusCode::BAD_REQUEST,
                json!({"__type":"UserNotFoundException"}),
            );
        };
        let account = state.accounts.get_mut(&username).unwrap();
        if account.status != "FORCE_CHANGE_PASSWORD" {
            return reply(
                StatusCode::BAD_REQUEST,
                json!({"__type":"UnsupportedUserStateException"}),
            );
        }
        account.password = password.to_owned();
        return reply(
            StatusCode::OK,
            json!({"User":{"Username":username,"UserStatus":account.status}}),
        );
    }
    if existing.is_some() {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"__type":"UsernameExistsException"}),
        );
    }
    let attribute = |name: &str| {
        body["UserAttributes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|attribute| attribute["Name"].as_str() == Some(name))
            .and_then(|attribute| attribute["Value"].as_str())
    };
    let username = attribute("sub")
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    if state.accounts.contains_key(&username) {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"__type":"UsernameExistsException"}),
        );
    }
    state.accounts.insert(
        username.clone(),
        Account {
            email: email.to_owned(),
            enabled: true,
            status: "FORCE_CHANGE_PASSWORD".into(),
            reset_code: app.reset_code.clone(),
            password: password.to_owned(),
            groups: Vec::new(),
            email_verified: attribute("email_verified") == Some("true"),
            given_name: attribute("given_name").map(str::to_owned),
            family_name: attribute("family_name").map(str::to_owned),
        },
    );
    reply(
        StatusCode::OK,
        json!({"User":{"Username":username,"Attributes":[
            {"Name":"sub","Value":username},{"Name":"email","Value":email}
        ],"Enabled":true,"UserStatus":"FORCE_CHANGE_PASSWORD"}}),
    )
}

/// Compatibility-mode handling that runs before the common targets. `Some` answers the request;
/// otherwise `body`'s `Username` may have been rewritten from an email to its account's sub.
fn compat_request(app: &App, target: &str, body: &mut Value) -> Option<Reply> {
    if target.ends_with(".ListUserPools") {
        return Some(reply(
            StatusCode::OK,
            json!({"UserPools":[{"Id":app.pool_id,"Name":app.pool_name}]}),
        ));
    }
    if target.ends_with(".ListUserPoolClients") {
        return Some(reply(
            StatusCode::OK,
            json!({"UserPoolClients":[{"ClientId":app.client_id,"ClientName":app.client_name,"UserPoolId":app.pool_id}]}),
        ));
    }
    if target.ends_with(".AdminCreateUser") {
        return Some(compat_create_user(app, body));
    }
    {
        // Cognito accepts an email alias wherever it takes a username.
        let state = app.state.lock().unwrap();
        for pointer in [
            "/Username",
            "/AuthParameters/USERNAME",
            "/ChallengeResponses/USERNAME",
        ] {
            let subject = body
                .pointer(pointer)
                .and_then(Value::as_str)
                .and_then(|value| {
                    state.accounts.iter().find(|(sub, account)| {
                        sub.as_str() == value || account.email.eq_ignore_ascii_case(value)
                    })
                })
                .map(|(sub, _)| sub.clone());
            if let Some(subject) = subject {
                *body.pointer_mut(pointer).unwrap() = json!(subject);
            }
        }
    }
    if target.ends_with(".AdminDisableUser") || target.ends_with(".AdminEnableUser") {
        let mut state = app.state.lock().unwrap();
        let Some(account) = state
            .accounts
            .get_mut(body["Username"].as_str().unwrap_or(""))
        else {
            return Some(reply(
                StatusCode::BAD_REQUEST,
                json!({"__type":"UserNotFoundException"}),
            ));
        };
        account.enabled = target.ends_with(".AdminEnableUser");
        return Some(reply(StatusCode::OK, json!({})));
    }
    None
}

async fn handle_request(app: Arc<App>, request: Request<Incoming>) -> Reply {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    if method == Method::GET && path == "/health" {
        if app.compat_mode {
            return reply(
                StatusCode::OK,
                json!({"status":"ok","fixture":"rust-cognito","issuer":app.issuer}),
            );
        }
        return reply(StatusCode::OK, json!({"status":"ok","issuer":app.issuer}));
    }
    if method == Method::GET && path == format!("/{}/.well-known/jwks.json", app.pool_id) {
        let mut state = app.state.lock().unwrap();
        state.jwks_requests += 1;
        if state.jwks_fault {
            return reply(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"__type":"InternalErrorException"}),
            );
        }
        return reply(
            StatusCode::OK,
            json!({"keys":state.keys.iter().map(Key::public_jwk).collect::<Vec<_>>() }),
        );
    }
    if method == Method::POST && path == "/" {
        let target = request
            .headers()
            .get("X-Amz-Target")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let compat_target = app.compat_mode
            && matches!(
                target.as_str(),
                "AWSCognitoIdentityProviderService.ListUserPools"
                    | "AWSCognitoIdentityProviderService.ListUserPoolClients"
                    | "AWSCognitoIdentityProviderService.AdminEnableUser"
            );
        if !compat_target
            && !matches!(
                target.as_str(),
                "AWSCognitoIdentityProviderService.GetUser"
                    | "AWSCognitoIdentityProviderService.RevokeToken"
                    | "AWSCognitoIdentityProviderService.ListUsers"
                    | "AWSCognitoIdentityProviderService.ForgotPassword"
                    | "AWSCognitoIdentityProviderService.ConfirmForgotPassword"
                    | "AWSCognitoIdentityProviderService.AdminGetUser"
                    | "AWSCognitoIdentityProviderService.InitiateAuth"
                    | "AWSCognitoIdentityProviderService.RespondToAuthChallenge"
                    | "AWSCognitoIdentityProviderService.AdminUpdateUserAttributes"
                    | "AWSCognitoIdentityProviderService.AdminListGroupsForUser"
                    | "AWSCognitoIdentityProviderService.AdminAddUserToGroup"
                    | "AWSCognitoIdentityProviderService.AdminRemoveUserFromGroup"
                    | "AWSCognitoIdentityProviderService.CreateGroup"
                    | "AWSCognitoIdentityProviderService.AdminCreateUser"
                    | "AWSCognitoIdentityProviderService.AdminSetUserPassword"
                    | "AWSCognitoIdentityProviderService.AdminDeleteUser"
                    | "AWSCognitoIdentityProviderService.AdminDisableUser"
            )
        {
            return bad_request("Unexpected Cognito target");
        }
        let Ok(mut body) = body_json(request).await else {
            return bad_request("Invalid Cognito body");
        };
        if app.compat_mode
            && let Some(response) = compat_request(&app, &target, &mut body)
        {
            return response;
        }
        if target == "AWSCognitoIdentityProviderService.CreateGroup" {
            let group = body["GroupName"].as_str().unwrap_or("");
            if body["UserPoolId"].as_str() != Some(app.pool_id.as_str()) || group.is_empty() {
                return bad_request("Invalid group request");
            }
            let mut state = app.state.lock().unwrap();
            if let Some(error) = &state.create_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            if !state.groups.insert(group.to_owned()) {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"GroupExistsException"}),
                );
            }
            return reply(StatusCode::OK, json!({"Group":{"GroupName":group}}));
        }
        if target == "AWSCognitoIdentityProviderService.AdminCreateUser" {
            let email = body["Username"].as_str().unwrap_or("");
            let owner_create = body["MessageAction"].as_str() == Some("SUPPRESS");
            let invited_create = body.get("MessageAction").is_none()
                && body["DesiredDeliveryMediums"] == json!(["EMAIL"]);
            let resend = body["MessageAction"].as_str() == Some("RESEND")
                && body["DesiredDeliveryMediums"] == json!(["EMAIL"]);
            if body["UserPoolId"].as_str() != Some(app.pool_id.as_str())
                || !(owner_create || invited_create || resend)
                || !email.contains('@')
            {
                return bad_request("Invalid owner creation request");
            }
            let mut state = app.state.lock().unwrap();
            if resend {
                state.resend_requests += 1;
                if let Some(error) = &state.resend_error {
                    return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
                }
                let Some((username, account)) = state
                    .accounts
                    .iter()
                    .find(|(_, account)| account.email.eq_ignore_ascii_case(email))
                else {
                    return reply(
                        StatusCode::BAD_REQUEST,
                        json!({"__type":"UserNotFoundException"}),
                    );
                };
                if account.status != "FORCE_CHANGE_PASSWORD" {
                    return reply(
                        StatusCode::BAD_REQUEST,
                        json!({"__type":"UnsupportedUserStateException"}),
                    );
                }
                return reply(StatusCode::OK, json!({"User":{"Username":username}}));
            }
            state.create_requests += 1;
            if let Some(error) = &state.create_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            if state
                .accounts
                .values()
                .any(|account| account.email.eq_ignore_ascii_case(email))
            {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"UsernameExistsException"}),
                );
            }
            let username = Uuid::now_v7().to_string();
            state.accounts.insert(
                username.clone(),
                Account {
                    email: email.to_owned(),
                    enabled: true,
                    status: "FORCE_CHANGE_PASSWORD".into(),
                    reset_code: String::new(),
                    password: String::new(),
                    groups: Vec::new(),
                    email_verified: true,
                    given_name: None,
                    family_name: None,
                },
            );
            return reply(
                StatusCode::OK,
                json!({"User":{"Username":username,"Attributes":[
                    {"Name":"sub","Value":username},{"Name":"email","Value":email}
                ]}}),
            );
        }
        if target == "AWSCognitoIdentityProviderService.AdminSetUserPassword" {
            let username = body["Username"].as_str().unwrap_or("");
            let password = body["Password"].as_str().unwrap_or("");
            let mut state = app.state.lock().unwrap();
            if let Some(error) = &state.set_password_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            let Some(account) = state.accounts.get_mut(username) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"UserNotFoundException"}),
                );
            };
            if password.len() < 8
                || !password.chars().any(|c| c.is_ascii_uppercase())
                || !password.chars().any(|c| c.is_ascii_lowercase())
                || !password.chars().any(|c| c.is_ascii_digit())
            {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"InvalidPasswordException"}),
                );
            }
            account.password = password.to_owned();
            account.status = "CONFIRMED".into();
            return reply(StatusCode::OK, json!({}));
        }
        if target == "AWSCognitoIdentityProviderService.AdminDeleteUser" {
            let username = body["Username"].as_str().unwrap_or("");
            let mut state = app.state.lock().unwrap();
            state.delete_requests += 1;
            if let Some(error) = &state.delete_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            if state.accounts.remove(username).is_none() {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"UserNotFoundException"}),
                );
            }
            return reply(StatusCode::OK, json!({}));
        }
        if target == "AWSCognitoIdentityProviderService.AdminDisableUser" {
            let username = body["Username"].as_str().unwrap_or("");
            let mut state = app.state.lock().unwrap();
            let Some(account) = state.accounts.get_mut(username) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"UserNotFoundException"}),
                );
            };
            account.enabled = false;
            return reply(StatusCode::OK, json!({}));
        }
        if target == "AWSCognitoIdentityProviderService.AdminListGroupsForUser" {
            let username = body["Username"].as_str().unwrap_or("");
            let state = app.state.lock().unwrap();
            if let Some(error) = &state.groups_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            let Some(account) = state.accounts.get(username) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"UserNotFoundException"}),
                );
            };
            return reply(
                StatusCode::OK,
                json!({"Groups":account.groups.iter().map(|name| json!({"GroupName":name})).collect::<Vec<_>>()}),
            );
        }
        if matches!(
            target.as_str(),
            "AWSCognitoIdentityProviderService.AdminAddUserToGroup"
                | "AWSCognitoIdentityProviderService.AdminRemoveUserFromGroup"
        ) {
            let username = body["Username"].as_str().unwrap_or("");
            let group = body["GroupName"].as_str().unwrap_or("");
            let mut state = app.state.lock().unwrap();
            if target == "AWSCognitoIdentityProviderService.AdminAddUserToGroup"
                && let Some(error) = &state.add_group_error
            {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            if let Some(error) = &state.groups_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            let Some(account) = state.accounts.get_mut(username) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"UserNotFoundException"}),
                );
            };
            if group.is_empty() {
                return bad_request("Expected group name");
            }
            if target == "AWSCognitoIdentityProviderService.AdminAddUserToGroup" {
                if !account.groups.iter().any(|name| name == group) {
                    account.groups.push(group.to_owned());
                }
            } else {
                account.groups.retain(|name| name != group);
            }
            return reply(StatusCode::OK, json!({}));
        }
        if target == "AWSCognitoIdentityProviderService.InitiateAuth" {
            let mut state = app.state.lock().unwrap();
            if let Some(error) = &state.auth_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            if body["ClientId"].as_str() != Some(app.client_id.as_str()) {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"ResourceNotFoundException"}),
                );
            }
            match body["AuthFlow"].as_str() {
                Some("USER_PASSWORD_AUTH") => {
                    let username = body["AuthParameters"]["USERNAME"].as_str().unwrap_or("");
                    let password = body["AuthParameters"]["PASSWORD"].as_str().unwrap_or("");
                    let Some(account) = state.accounts.get(username) else {
                        return reply(
                            StatusCode::BAD_REQUEST,
                            json!({"__type":"UserNotFoundException"}),
                        );
                    };
                    if !account.enabled || account.password != password {
                        return reply(
                            StatusCode::BAD_REQUEST,
                            json!({"__type":"NotAuthorizedException"}),
                        );
                    }
                    if account.status == "RESET_REQUIRED" {
                        return reply(
                            StatusCode::BAD_REQUEST,
                            json!({"__type":"PasswordResetRequiredException"}),
                        );
                    }
                    if account.status == "FORCE_CHANGE_PASSWORD" {
                        let mut bytes = [0_u8; 24];
                        OsRng.fill_bytes(&mut bytes);
                        let session = URL_SAFE_NO_PAD.encode(bytes);
                        state
                            .challenges
                            .insert(session.clone(), username.to_owned());
                        return reply(
                            StatusCode::OK,
                            json!({"ChallengeName":"NEW_PASSWORD_REQUIRED","Session":session}),
                        );
                    }
                    return reply(
                        StatusCode::OK,
                        json!({"AuthenticationResult":issue_session(&app, &mut state, username, true)}),
                    );
                }
                Some(flow)
                    if flow == "REFRESH_TOKEN_AUTH"
                        || (app.compat_mode && flow == "REFRESH_TOKEN") =>
                {
                    let refresh = body["AuthParameters"]["REFRESH_TOKEN"]
                        .as_str()
                        .unwrap_or("");
                    if state.revoked_refresh.contains(refresh) {
                        return reply(
                            StatusCode::BAD_REQUEST,
                            json!({"__type":"NotAuthorizedException"}),
                        );
                    }
                    let Some(username) = state.refresh_users.get(refresh).cloned() else {
                        return reply(
                            StatusCode::BAD_REQUEST,
                            json!({"__type":"NotAuthorizedException"}),
                        );
                    };
                    if !state
                        .accounts
                        .get(&username)
                        .is_some_and(|account| account.enabled)
                    {
                        return reply(
                            StatusCode::BAD_REQUEST,
                            json!({"__type":"NotAuthorizedException"}),
                        );
                    }
                    return reply(
                        StatusCode::OK,
                        json!({"AuthenticationResult":issue_session(&app, &mut state, &username, false)}),
                    );
                }
                _ => return bad_request("Unexpected auth flow"),
            }
        }
        if target == "AWSCognitoIdentityProviderService.RespondToAuthChallenge" {
            let mut state = app.state.lock().unwrap();
            if let Some(error) = &state.auth_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            let session = body["Session"].as_str().unwrap_or("");
            let username = body["ChallengeResponses"]["USERNAME"]
                .as_str()
                .unwrap_or("");
            let password = body["ChallengeResponses"]["NEW_PASSWORD"]
                .as_str()
                .unwrap_or("");
            if body["ClientId"].as_str() != Some(app.client_id.as_str())
                || body["ChallengeName"].as_str() != Some("NEW_PASSWORD_REQUIRED")
                || state
                    .challenges
                    .get(session)
                    .is_none_or(|expected| expected != username)
            {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"NotAuthorizedException"}),
                );
            }
            if password.len() < 8
                || !password.chars().any(|c| c.is_ascii_uppercase())
                || !password.chars().any(|c| c.is_ascii_lowercase())
                || !password.chars().any(|c| c.is_ascii_digit())
            {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"InvalidPasswordException"}),
                );
            }
            state.challenges.remove(session);
            if let Some(account) = state.accounts.get_mut(username) {
                account.password = password.to_owned();
                account.status = "CONFIRMED".into();
            }
            return reply(
                StatusCode::OK,
                json!({"AuthenticationResult":issue_session(&app, &mut state, username, true)}),
            );
        }
        if target == "AWSCognitoIdentityProviderService.AdminUpdateUserAttributes" {
            let username = body["Username"].as_str().unwrap_or("");
            let mut state = app.state.lock().unwrap();
            let Some(account) = state.accounts.get_mut(username) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"UserNotFoundException"}),
                );
            };
            if body["UserPoolId"].as_str() != Some(app.pool_id.as_str()) {
                return bad_request("Unexpected pool");
            }
            for attribute in body["UserAttributes"].as_array().into_iter().flatten() {
                match attribute["Name"].as_str() {
                    Some("given_name") => {
                        account.given_name = attribute["Value"].as_str().map(str::to_owned)
                    }
                    Some("family_name") => {
                        account.family_name = attribute["Value"].as_str().map(str::to_owned)
                    }
                    _ => {}
                }
            }
            return reply(StatusCode::OK, json!({}));
        }
        if target == "AWSCognitoIdentityProviderService.ListUsers" {
            let filter = body["Filter"].as_str().unwrap_or("");
            let Some(email) = filter
                .strip_prefix("email = \"")
                .and_then(|s| s.strip_suffix('"'))
            else {
                return bad_request("Expected email filter");
            };
            let mut state = app.state.lock().unwrap();
            state.list_user_requests += 1;
            if let Some(error) = &state.list_user_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            let users: Vec<Value> = state
                .accounts
                .iter()
                .filter(|(_, account)| account.email.eq_ignore_ascii_case(email))
                .map(|(username, account)| {
                    json!({
                        "Username":username,
                        "Attributes":[{"Name":"sub","Value":username},{"Name":"email","Value":account.email}],
                        "Enabled":account.enabled,
                        "UserStatus":account.status,
                    })
                })
                .collect();
            return reply(StatusCode::OK, json!({"Users":users}));
        }
        if target == "AWSCognitoIdentityProviderService.AdminGetUser" {
            let username = body["Username"].as_str().unwrap_or("");
            let state = app.state.lock().unwrap();
            let Some(account) = state.accounts.get(username) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"UserNotFoundException"}),
                );
            };
            let attributes = if app.compat_mode {
                json!([{"Name":"sub","Value":username},{"Name":"email","Value":account.email}])
            } else {
                json!([{"Name":"email","Value":account.email}])
            };
            return reply(
                StatusCode::OK,
                json!({
                    "Username":username,"Enabled":account.enabled,"UserStatus":account.status,
                    "UserAttributes":attributes
                }),
            );
        }
        if target == "AWSCognitoIdentityProviderService.ForgotPassword" {
            let username = body["Username"].as_str().unwrap_or("");
            let client_id = body["ClientId"].as_str().unwrap_or("");
            let mut state = app.state.lock().unwrap();
            state.forgot_requests += 1;
            if let Some(error) = &state.forgot_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            if client_id != app.client_id {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"InvalidParameterException"}),
                );
            }
            let Some(account) = state.accounts.get(username) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"UserNotFoundException"}),
                );
            };
            if !account.enabled || account.status != "CONFIRMED" {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"NotAuthorizedException"}),
                );
            }
            return reply(
                StatusCode::OK,
                json!({"CodeDeliveryDetails":{"Destination":"masked@example.test","DeliveryMedium":"EMAIL","AttributeName":"email"}}),
            );
        }
        if target == "AWSCognitoIdentityProviderService.ConfirmForgotPassword" {
            let username = body["Username"].as_str().unwrap_or("");
            let code = body["ConfirmationCode"].as_str().unwrap_or("");
            let password = body["Password"].as_str().unwrap_or("");
            let client_id = body["ClientId"].as_str().unwrap_or("");
            let mut state = app.state.lock().unwrap();
            state.confirm_requests += 1;
            if let Some(error) = &state.confirm_error {
                return reply(StatusCode::BAD_REQUEST, json!({"__type":error}));
            }
            if client_id != app.client_id {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"InvalidParameterException"}),
                );
            }
            let Some(account) = state.accounts.get_mut(username) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"UserNotFoundException"}),
                );
            };
            if !account.enabled || account.status != "CONFIRMED" {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"NotAuthorizedException"}),
                );
            }
            if code != account.reset_code {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"CodeMismatchException"}),
                );
            }
            if password.len() < 8
                || !password.chars().any(|c| c.is_ascii_uppercase())
                || !password.chars().any(|c| c.is_ascii_lowercase())
                || !password.chars().any(|c| c.is_ascii_digit())
            {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"InvalidPasswordException"}),
                );
            }
            account.password = password.to_owned();
            return reply(StatusCode::OK, json!({}));
        }
        if target == "AWSCognitoIdentityProviderService.RevokeToken" {
            let token = body["Token"].as_str().unwrap_or("");
            let client_id = body["ClientId"].as_str().unwrap_or("");
            let mut state = app.state.lock().unwrap();
            state.revoke_requests += 1;
            if client_id != app.client_id || !state.issued_refresh.contains(token) {
                return reply(
                    StatusCode::BAD_REQUEST,
                    json!({"__type":"NotAuthorizedException"}),
                );
            }
            state.revoked_refresh.insert(token.to_owned());
            return reply(StatusCode::OK, json!({}));
        }
        let token = body["AccessToken"].as_str().unwrap_or("");
        let mut state = app.state.lock().unwrap();
        state.get_user_requests += 1;
        if state.get_user_fault {
            return reply(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"__type":"InternalErrorException"}),
            );
        }
        if state.revoked.contains(token) || !state.issued.contains_key(token) {
            return reply(
                StatusCode::BAD_REQUEST,
                json!({"__type":"NotAuthorizedException"}),
            );
        }
        let identity = state.issued.get(token).unwrap();
        let account = state.accounts.get(&identity.sub);
        if account.is_none() {
            return reply(
                StatusCode::BAD_REQUEST,
                json!({"__type":"UserNotFoundException"}),
            );
        }
        let mut attributes = vec![
            json!({"Name":"sub","Value":identity.sub}),
            json!({"Name":"email","Value":identity.email}),
            json!({"Name":"email_verified","Value":if identity.email_verified {"true"} else {"false"}}),
        ];
        if let Some(given) = account.and_then(|account| account.given_name.as_deref()) {
            attributes.push(json!({"Name":"given_name","Value":given}));
        }
        if let Some(family) = account.and_then(|account| account.family_name.as_deref()) {
            attributes.push(json!({"Name":"family_name","Value":family}));
        }
        return reply(
            StatusCode::OK,
            json!({"Username":identity.sub,"UserAttributes":attributes}),
        );
    }
    if method == Method::POST && path == "/local/issue" {
        let Ok(body) = body_json(request).await else {
            return bad_request("Invalid issue body");
        };
        let Ok(input) = serde_json::from_value::<Issue>(body) else {
            return bad_request("Expected sub and email");
        };
        if input.sub.is_empty() || input.email.is_empty() || input.sub.len() > 255 {
            return bad_request("Invalid fixture identity");
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let expires = now + input.expires_in_seconds.unwrap_or(3600);
        let mut state = app.state.lock().unwrap();
        let key = state.keys.last().unwrap();
        let kid = key.kid.clone();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.clone());
        let token = match encode(
            &header,
            &json!({
                "iss":app.issuer,"sub":input.sub,"client_id":app.client_id,
                "token_use":"access","iat":now,"exp":expires
            }),
            &key.encoding,
        ) {
            Ok(token) => token,
            Err(error) => {
                warn!(%error, "Fixture token encoding failed");
                return reply(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"detail":"Token encoding failed"}),
                );
            }
        };
        state.issued.insert(
            token.clone(),
            Identity {
                sub: input.sub.clone(),
                email: input.email.clone(),
                email_verified: input.email_verified.unwrap_or(true),
            },
        );
        let mut code_bytes = [0_u8; 12];
        OsRng.fill_bytes(&mut code_bytes);
        let reset_code = URL_SAFE_NO_PAD.encode(code_bytes);
        state.accounts.insert(
            input.sub.clone(),
            Account {
                email: input.email,
                enabled: input.enabled.unwrap_or(true),
                status: input.user_status.unwrap_or_else(|| "CONFIRMED".into()),
                reset_code: reset_code.clone(),
                password: input.password.unwrap_or_else(|| "Password123".into()),
                groups: input.groups,
                email_verified: input.email_verified.unwrap_or(true),
                given_name: input.given_name,
                family_name: input.family_name,
            },
        );
        let mut refresh_bytes = [0_u8; 32];
        OsRng.fill_bytes(&mut refresh_bytes);
        let refresh_token = URL_SAFE_NO_PAD.encode(refresh_bytes);
        state.issued_refresh.insert(refresh_token.clone());
        state
            .refresh_users
            .insert(refresh_token.clone(), input.sub.clone());
        return reply(
            StatusCode::OK,
            json!({"access_token":token,"refresh_token":refresh_token,"reset_code":reset_code,"kid":kid}),
        );
    }
    if method == Method::POST && path == "/local/revoke" {
        let Ok(body) = body_json(request).await else {
            return bad_request("Invalid revoke body");
        };
        let Some(token) = body["access_token"].as_str() else {
            return bad_request("Expected access_token");
        };
        let mut state = app.state.lock().unwrap();
        if !state.issued.contains_key(token) {
            return bad_request("Unknown fixture token");
        }
        state.revoked.insert(token.to_owned());
        return reply(StatusCode::OK, json!({"revoked":true}));
    }
    if method == Method::POST && path == "/local/groups" {
        let Ok(body) = body_json(request).await else {
            return bad_request("Invalid groups body");
        };
        let Some(username) = body["username"].as_str() else {
            return bad_request("Expected username");
        };
        let Ok(groups) = serde_json::from_value::<Vec<String>>(body["groups"].clone()) else {
            return bad_request("Expected groups");
        };
        let mut state = app.state.lock().unwrap();
        let Some(account) = state.accounts.get_mut(username) else {
            return bad_request("Unknown fixture account");
        };
        account.groups = groups;
        return reply(StatusCode::OK, json!({"updated":true}));
    }
    if method == Method::POST && path == "/local/rotate" {
        let next = { app.state.lock().unwrap().keys.len() + 1 };
        let Ok(key) = Key::generate(next, app.compat_mode) else {
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"detail":"Key generation failed"}),
            );
        };
        let kid = key.kid.clone();
        app.state.lock().unwrap().keys.push(key);
        return reply(StatusCode::OK, json!({"kid":kid}));
    }
    if method == Method::POST && path == "/local/faults" {
        let Ok(body) = body_json(request).await else {
            return bad_request("Invalid faults body");
        };
        let mut state = app.state.lock().unwrap();
        state.jwks_fault = body["jwks_unavailable"].as_bool().unwrap_or(false);
        state.get_user_fault = body["get_user_unavailable"].as_bool().unwrap_or(false);
        state.forgot_error = body["forgot_error"].as_str().map(str::to_owned);
        state.confirm_error = body["confirm_error"].as_str().map(str::to_owned);
        state.auth_error = body["auth_error"].as_str().map(str::to_owned);
        state.groups_error = body["groups_error"].as_str().map(str::to_owned);
        state.add_group_error = body["add_group_error"].as_str().map(str::to_owned);
        state.create_error = body["create_error"].as_str().map(str::to_owned);
        state.resend_error = body["resend_error"].as_str().map(str::to_owned);
        state.set_password_error = body["set_password_error"].as_str().map(str::to_owned);
        state.delete_error = body["delete_error"].as_str().map(str::to_owned);
        state.list_user_error = body["list_user_error"].as_str().map(str::to_owned);
        return reply(StatusCode::OK, json!({"updated":true}));
    }
    if method == Method::GET && path == "/local/metrics" {
        let state = app.state.lock().unwrap();
        return reply(
            StatusCode::OK,
            json!({
                "jwks_requests":state.jwks_requests,
                "get_user_requests":state.get_user_requests,
                "revoke_requests":state.revoke_requests,
                "revoked_refresh":state.revoked_refresh.len(),
                "forgot_requests":state.forgot_requests,
                "confirm_requests":state.confirm_requests,
                "list_user_requests":state.list_user_requests,
                "create_requests":state.create_requests,
                "resend_requests":state.resend_requests,
                "delete_requests":state.delete_requests,
                "accounts":state.accounts.len(),
            }),
        );
    }
    reply(StatusCode::NOT_FOUND, json!({"detail":"Not Found"}))
}

fn load(config: Config, address: SocketAddr) -> Result<Arc<App>> {
    config.validate()?;
    let saved: SavedAccounts = match config.state_file.as_ref().filter(|path| path.exists()) {
        Some(path) => serde_json::from_slice(
            &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("decode {}", path.display()))?,
        None => SavedAccounts::default(),
    };
    let key = Key::generate(1, config.compat_mode)?;
    Ok(Arc::new(App {
        state_file: config.state_file,
        compat_mode: config.compat_mode,
        issuer: config
            .issuer
            .unwrap_or_else(|| format!("http://{address}/{pool}", pool = config.pool_id)),
        pool_id: config.pool_id,
        client_id: config.client_id,
        pool_name: config.pool_name,
        client_name: config.client_name,
        reset_code: config.reset_code,
        state: Mutex::new(State {
            keys: vec![key],
            issued: HashMap::new(),
            accounts: saved.accounts,
            groups: saved.groups,
            revoked: HashSet::new(),
            issued_refresh: HashSet::new(),
            refresh_users: HashMap::new(),
            revoked_refresh: HashSet::new(),
            challenges: HashMap::new(),
            jwks_requests: 0,
            get_user_requests: 0,
            revoke_requests: 0,
            forgot_requests: 0,
            confirm_requests: 0,
            list_user_requests: 0,
            jwks_fault: false,
            get_user_fault: false,
            forgot_error: None,
            confirm_error: None,
            auth_error: None,
            groups_error: None,
            add_group_error: None,
            create_error: None,
            resend_error: None,
            set_password_error: None,
            delete_error: None,
            list_user_error: None,
            create_requests: 0,
            resend_requests: 0,
            delete_requests: 0,
        }),
    }))
}

async fn accept(listener: TcpListener, app: Arc<App>) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let app = app.clone();
                async move { Ok::<_, Infallible>(handle(app, request).await) }
            });
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                warn!(%error, "Fixture connection failed");
            }
        });
    }
}

/// Serve the fixture on a bound listener until the task is dropped. The default issuer uses
/// the listener's address. The caller decides which interfaces the listener is bound to.
pub async fn serve(listener: TcpListener, config: Config) -> Result<()> {
    let address = listener
        .local_addr()
        .context("read Cognito listener address")?;
    let app = load(config, address)?;
    accept(listener, app).await
}

/// A fixture running inside the current Tokio runtime; dropping it stops the listener.
pub struct RunningCognito {
    address: SocketAddr,
    issuer: String,
    task: JoinHandle<Result<()>>,
}

impl RunningCognito {
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The endpoint to configure as `AWS_ENDPOINT_URL_COGNITO_IDENTITY_PROVIDER`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The `iss` claim of tokens this fixture signs.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn stop(self) {}
}

impl Drop for RunningCognito {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start the fixture on `127.0.0.1:0`.
pub async fn start(config: Config) -> Result<RunningCognito> {
    listen(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), config).await
}

/// Start the fixture on `address`, which must be loopback.
pub async fn listen(address: SocketAddr, config: Config) -> Result<RunningCognito> {
    anyhow::ensure!(address.ip().is_loopback(), "fixture must bind loopback");
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("bind Cognito listener on {address}"))?;
    let address = listener
        .local_addr()
        .context("read Cognito listener address")?;
    // Load up front so a bad config or state file fails the caller, not the task.
    let app = load(config, address)?;
    let issuer = app.issuer.clone();
    let task = tokio::spawn(accept(listener, app));
    Ok(RunningCognito {
        address,
        issuer,
        task,
    })
}
