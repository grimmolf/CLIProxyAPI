use std::{
    collections::HashMap,
    sync::OnceLock,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, Uri},
    response::Html,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use url::Url;
use uuid::Uuid;

use crate::core::{Account, AccountView, ApiError, AppState, Provider};

const CODEX_AUTH: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_TOKEN: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_REDIRECT: &str = "http://localhost:1455/auth/callback";
const CLAUDE_AUTH: &str = "https://claude.ai/oauth/authorize";
const CLAUDE_TOKEN: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_REDIRECT: &str = "http://localhost:54545/callback";
const CLAUDE_SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

struct Pending {
    provider: Provider,
    label: String,
    verifier: String,
    expires: Instant,
    callback_url: Option<String>,
}

static PENDING: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
static LISTENERS: OnceLock<Mutex<Vec<u16>>> = OnceLock::new();
// ponytail: one lock serializes refreshes for all accounts; use keyed locks if refresh volume grows.
static REFRESH: Mutex<()> = Mutex::const_new(());

fn pending() -> &'static Mutex<HashMap<String, Pending>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/accounts/import", post(import_account))
        .route("/api/oauth/start", post(start))
        .route("/api/oauth/complete", post(complete))
}

#[derive(Deserialize)]
struct ImportRequest {
    provider: Option<Provider>,
    label: Option<String>,
    credentials: Value,
}

async fn import_account(
    State(state): State<AppState>,
    Json(body): Json<ImportRequest>,
) -> Result<(StatusCode, Json<AccountView>), ApiError> {
    let account = account_from_credentials(body.provider, body.label, &body.credentials)?;
    let view = state.store.save_account(account).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

fn account_from_credentials(
    provider: Option<Provider>,
    label: Option<String>,
    credentials: &Value,
) -> Result<Account, ApiError> {
    let object = credentials
        .as_object()
        .ok_or_else(|| bad("credentials must be an object"))?;
    let nested = object.get("tokens").or_else(|| object.get("token"));
    let source = nested.and_then(Value::as_object).unwrap_or(object);
    let provider = match provider {
        Some(provider) => provider,
        None => {
            let kind = string_field(object, "type")?
                .or(string_field(object, "provider")?)
                .or(string_field(source, "type")?)
                .or(string_field(source, "provider")?)
                .ok_or_else(|| bad("provider is required"))?;
            match kind.as_str() {
                "codex" => Provider::Codex,
                "claude" | "anthropic" => Provider::Claude,
                "openai" => Provider::Openai,
                "gemini" => Provider::Gemini,
                "openai_compatible" => Provider::OpenaiCompatible,
                _ => return Err(bad("unsupported provider")),
            }
        }
    };
    if let Some(kind) = string_field(object, "type")?.or(string_field(object, "provider")?) {
        let matches =
            kind == provider.as_str() || (kind == "anthropic" && provider == Provider::Claude);
        if !matches {
            return Err(bad("provider does not match credentials"));
        }
    }
    let secret = string_field(source, "access_token")?
        .or_else(|| {
            source
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or(string_field(source, "api_key")?)
        .ok_or_else(|| bad("credentials need an access_token, token, or api_key"))?;
    valid_secret(&secret)?;
    let refresh_token =
        string_field(source, "refresh_token")?.or(string_field(object, "refresh_token")?);
    if let Some(token) = &refresh_token {
        valid_secret(token)?;
    }
    let label = label
        .or(string_field(object, "email")?)
        .or(string_field(source, "email")?)
        .unwrap_or_else(|| provider.as_str().to_owned());
    let label = valid_label(&label, &provider)?;
    let expires_at = expiration(source)?.or(expiration(object)?);
    let id_token = string_field(source, "id_token")?.or(string_field(object, "id_token")?);
    if let Some(token) = &id_token {
        valid_secret(token)?;
    }
    let chatgpt_account_id = if provider == Provider::Codex {
        string_field(source, "chatgpt_account_id")?
            .or(string_field(source, "account_id")?)
            .or(string_field(object, "chatgpt_account_id")?)
            .or(string_field(object, "account_id")?)
            .or_else(|| id_token.as_deref().and_then(jwt_account_id))
            .or_else(|| jwt_account_id(&secret))
    } else {
        None
    };
    if let Some(id) = &chatgpt_account_id {
        valid_secret(id)?;
    }
    let auth_kind = if source.contains_key("api_key")
        && !source.contains_key("access_token")
        && refresh_token.is_none()
    {
        "api_key"
    } else {
        "oauth"
    };
    Ok(Account {
        id: Uuid::new_v4().to_string(),
        label,
        provider,
        base_url: provider.default_base_url().to_owned(),
        secret,
        refresh_token,
        expires_at,
        enabled: true,
        models: vec![],
        created_at: Utc::now().to_rfc3339(),
        auth_kind: auth_kind.to_owned(),
        chatgpt_account_id,
    })
}

fn string_field(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, ApiError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
        _ => Err(bad(format!("{key} must be a nonempty string"))),
    }
}

fn valid_secret(value: &str) -> Result<(), ApiError> {
    if value.len() > 64 * 1024 || value.is_empty() || value.chars().any(char::is_control) {
        Err(bad("invalid credential value"))
    } else {
        Ok(())
    }
}

fn valid_label(value: &str, provider: &Provider) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        return Err(bad("invalid account label"));
    }
    if matches!(provider, Provider::Codex | Provider::Claude) {
        Ok(format!("{value} (experimental)"))
    } else {
        Ok(value.to_owned())
    }
}

fn expiration(object: &serde_json::Map<String, Value>) -> Result<Option<i64>, ApiError> {
    for key in ["expires_at", "expired", "expire"] {
        if let Some(value) = object.get(key) {
            return match value {
                Value::Null => Ok(None),
                Value::Number(n) => n
                    .as_i64()
                    .filter(|n| *n >= 0)
                    .map(Some)
                    .ok_or_else(|| bad("invalid expiration")),
                Value::String(s) => DateTime::parse_from_rfc3339(s)
                    .map(|dt| Some(dt.timestamp()))
                    .map_err(|_| bad("invalid expiration")),
                _ => Err(bad("invalid expiration")),
            };
        }
    }
    Ok(None)
}

fn jwt_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_owned)
}

#[derive(Deserialize)]
struct StartRequest {
    provider: Provider,
    label: Option<String>,
}

#[derive(Serialize)]
struct StartResponse {
    authorization_url: String,
    state: String,
    redirect_uri: &'static str,
    callback_listener: bool,
    manual_callback_instructions: Option<String>,
}

async fn start(Json(body): Json<StartRequest>) -> Result<Json<StartResponse>, ApiError> {
    let (auth_url, _, client_id, redirect_uri, port, scope) = config(&body.provider)?;
    let label = valid_label(
        body.label.as_deref().unwrap_or(body.provider.as_str()),
        &body.provider,
    )?;
    let state = random_urlsafe();
    let verifier = random_urlsafe();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = Url::parse(auth_url).map_err(|_| internal("invalid OAuth configuration"))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", scope)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if body.provider == Provider::Codex {
            pairs
                .append_pair("prompt", "login")
                .append_pair("id_token_add_organizations", "true")
                .append_pair("codex_cli_simplified_flow", "true");
        } else {
            pairs.append_pair("code", "true");
        }
    }
    {
        let mut sessions = pending().lock().await;
        sessions.retain(|_, session| session.expires > Instant::now());
        if sessions.len() >= 128 {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many pending logins",
            ));
        }
        sessions.insert(
            state.clone(),
            Pending {
                provider: body.provider,
                label,
                verifier,
                expires: Instant::now() + Duration::from_secs(600),
                callback_url: None,
            },
        );
    }
    let listener = ensure_listener(port).await;
    Ok(Json(StartResponse {
        authorization_url: url.into(),
        state,
        redirect_uri,
        callback_listener: listener,
        manual_callback_instructions: (!listener).then(|| format!(
            "Could not listen on {redirect_uri}. After signing in, copy the full callback URL from your browser and paste it into the app."
        )),
    }))
}

fn random_urlsafe() -> String {
    URL_SAFE_NO_PAD.encode(Uuid::new_v4().as_bytes())
        + &URL_SAFE_NO_PAD.encode(Uuid::new_v4().as_bytes())
}

async fn ensure_listener(port: u16) -> bool {
    let listeners = LISTENERS.get_or_init(|| Mutex::new(Vec::new()));
    let mut started = listeners.lock().await;
    if started.contains(&port) {
        return true;
    }
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await else {
        return false;
    };
    let route = if port == 1455 {
        "/auth/callback"
    } else {
        "/callback"
    };
    let app = Router::new().route(route, get(loopback_callback));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    started.push(port);
    true
}

async fn loopback_callback(uri: Uri) -> Html<&'static str> {
    let query = uri.query().unwrap_or("");
    let provider = if uri.path() == "/auth/callback" {
        Provider::Codex
    } else {
        Provider::Claude
    };
    let Some(state) = url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
    else {
        return Html("Invalid OAuth callback. You may paste the callback URL into the app.");
    };
    let Ok((_, _, _, redirect, _, _)) = config(&provider) else {
        return Html("Invalid OAuth callback.");
    };
    let mut sessions = pending().lock().await;
    if let Some(session) = sessions.get_mut(&state)
        && session.provider == provider
        && session.expires > Instant::now()
    {
        session.callback_url = Some(format!("{redirect}?{query}"));
        return Html("OAuth callback received. Return to the app to complete login.");
    }
    Html("Unknown or expired OAuth state. You may start login again.")
}

#[derive(Deserialize)]
struct CompleteRequest {
    state: String,
    #[serde(default)]
    callback_url: String,
}

async fn complete(
    State(state): State<AppState>,
    Json(body): Json<CompleteRequest>,
) -> Result<(StatusCode, Json<AccountView>), ApiError> {
    complete_with_endpoint(state, body, None).await
}

async fn complete_with_endpoint(
    state: AppState,
    body: CompleteRequest,
    endpoint_override: Option<&str>,
) -> Result<(StatusCode, Json<AccountView>), ApiError> {
    let session = {
        let mut sessions = pending().lock().await;
        let session = sessions
            .get(&body.state)
            .ok_or_else(|| bad("unknown or used OAuth state"))?;
        if session.expires <= Instant::now() {
            sessions.remove(&body.state);
            return Err(bad("expired OAuth state"));
        }
        if body.callback_url.is_empty() && session.callback_url.is_none() {
            return Err(bad(
                "OAuth callback has not arrived; paste its URL to complete login",
            ));
        }
        sessions
            .remove(&body.state)
            .ok_or_else(|| bad("unknown or used OAuth state"))?
    };
    let callback = if body.callback_url.is_empty() {
        session.callback_url.as_deref().unwrap_or("")
    } else {
        &body.callback_url
    };
    let (_, endpoint, client_id, redirect, _, _) = config(&session.provider)?;
    let code = callback_code(callback, redirect, &body.state)?;
    let tokens = acquire_token(
        &state.client,
        &session.provider,
        endpoint_override.unwrap_or(endpoint),
        client_id,
        TokenGrant::Code {
            code: &code,
            verifier: &session.verifier,
            redirect,
            state: &body.state,
        },
    )
    .await?;
    let mut account = account_from_tokens(session.provider, session.label, tokens)?;
    account.chatgpt_account_id = account
        .chatgpt_account_id
        .or_else(|| jwt_account_id(&account.secret));
    let view = state.store.save_account(account).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

fn callback_code(callback: &str, redirect: &str, expected_state: &str) -> Result<String, ApiError> {
    let url = Url::parse(callback).map_err(|_| bad("invalid callback URL"))?;
    let expected = Url::parse(redirect).map_err(|_| internal("invalid OAuth configuration"))?;
    if url.scheme() != expected.scheme()
        || url.host_str() != expected.host_str()
        || url.port() != expected.port()
        || url.path() != expected.path()
        || url.username() != ""
        || url.password().is_some()
    {
        return Err(bad("callback URL does not match registered redirect URI"));
    }
    let mut state = None;
    let mut code = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "state" => &mut state,
            "code" => &mut code,
            "error" => &mut error,
            _ => continue,
        };
        if slot.replace(value.into_owned()).is_some() {
            return Err(bad("duplicate OAuth callback parameter"));
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Err(bad("OAuth callback state mismatch"));
    }
    if error.is_some() {
        return Err(bad("OAuth authorization was declined"));
    }
    let code = code
        .filter(|code| !code.is_empty())
        .ok_or_else(|| bad("OAuth callback has no code"))?;
    if code.len() > 4096 || code.chars().any(char::is_control) {
        return Err(bad("invalid OAuth code"));
    }
    Ok(code)
}

fn config(
    provider: &Provider,
) -> Result<
    (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        u16,
        &'static str,
    ),
    ApiError,
> {
    match provider {
        Provider::Codex => Ok((
            CODEX_AUTH,
            CODEX_TOKEN,
            CODEX_CLIENT,
            CODEX_REDIRECT,
            1455,
            "openid email profile offline_access",
        )),
        Provider::Claude => Ok((
            CLAUDE_AUTH,
            CLAUDE_TOKEN,
            CLAUDE_CLIENT,
            CLAUDE_REDIRECT,
            54545,
            CLAUDE_SCOPE,
        )),
        _ => Err(bad("OAuth login supports Codex and Claude only")),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    id_token: Option<String>,
}

enum TokenGrant<'a> {
    Code {
        code: &'a str,
        verifier: &'a str,
        redirect: &'a str,
        state: &'a str,
    },
    Refresh(&'a str),
}

async fn acquire_token(
    client: &reqwest::Client,
    provider: &Provider,
    endpoint: &str,
    client_id: &str,
    grant: TokenGrant<'_>,
) -> Result<TokenResponse, ApiError> {
    let request = match (provider, grant) {
        (Provider::Codex, TokenGrant::Code { code, verifier, redirect, .. }) => client.post(endpoint).form(&[("grant_type", "authorization_code"), ("client_id", client_id), ("code", code), ("redirect_uri", redirect), ("code_verifier", verifier)]),
        (Provider::Codex, TokenGrant::Refresh(token)) => client.post(endpoint).form(&[("grant_type", "refresh_token"), ("client_id", client_id), ("refresh_token", token), ("scope", "openid profile email")]),
        (Provider::Claude, TokenGrant::Code { code, verifier, redirect, state }) => client.post(endpoint).json(&json!({ "grant_type": "authorization_code", "code": code, "redirect_uri": redirect, "client_id": client_id, "code_verifier": verifier, "state": state })),
        (Provider::Claude, TokenGrant::Refresh(token)) => client.post(endpoint).json(&json!({ "grant_type": "refresh_token", "client_id": client_id, "refresh_token": token, "scope": CLAUDE_SCOPE })),
        _ => return Err(bad("unsupported OAuth provider")),
    };
    let response = request
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| gateway("OAuth token request failed"))?;
    if !response.status().is_success() {
        return Err(gateway(format!(
            "OAuth token request returned HTTP {}",
            response.status().as_u16()
        )));
    }
    let tokens: TokenResponse = response
        .json()
        .await
        .map_err(|_| gateway("invalid OAuth token response"))?;
    valid_secret(&tokens.access_token)?;
    if let Some(token) = &tokens.refresh_token {
        valid_secret(token)?;
    }
    if tokens
        .expires_in
        .is_some_and(|seconds| seconds <= 0 || seconds > 365 * 24 * 3600)
    {
        return Err(gateway("invalid OAuth token expiration"));
    }
    Ok(tokens)
}

fn account_from_tokens(
    provider: Provider,
    label: String,
    tokens: TokenResponse,
) -> Result<Account, ApiError> {
    let chatgpt_account_id = tokens
        .id_token
        .as_deref()
        .and_then(jwt_account_id)
        .or_else(|| jwt_account_id(&tokens.access_token));
    let expires_at = tokens
        .expires_in
        .map(|seconds| Utc::now().timestamp() + seconds);
    Ok(Account {
        id: Uuid::new_v4().to_string(),
        label,
        provider,
        base_url: provider.default_base_url().to_owned(),
        secret: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at,
        enabled: true,
        models: vec![],
        created_at: Utc::now().to_rfc3339(),
        auth_kind: "oauth".to_owned(),
        chatgpt_account_id,
    })
}

pub async fn refresh_account(state: &AppState, account: &Account) -> Result<Account, ApiError> {
    let (_, endpoint, client_id, _, _, _) = config(&account.provider)?;
    refresh_with_endpoint(state, account, endpoint, client_id).await
}

async fn refresh_with_endpoint(
    state: &AppState,
    account: &Account,
    endpoint: &str,
    client_id: &str,
) -> Result<Account, ApiError> {
    if account.auth_kind != "oauth" {
        return Err(bad("account does not use OAuth"));
    }
    let _guard = REFRESH.lock().await;
    let current = state
        .store
        .account(&account.id)
        .await
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "account was deleted"))?;
    if current.secret != account.secret || current.refresh_token != account.refresh_token {
        return Ok(current);
    }
    let token = current
        .refresh_token
        .as_deref()
        .ok_or_else(|| bad("account has no refresh token"))?;
    let tokens = acquire_token(
        &state.client,
        &current.provider,
        endpoint,
        client_id,
        TokenGrant::Refresh(token),
    )
    .await?;
    let chatgpt_account_id = if current.provider == Provider::Codex {
        tokens
            .id_token
            .as_deref()
            .and_then(jwt_account_id)
            .or_else(|| jwt_account_id(&tokens.access_token))
    } else {
        None
    };
    let mut refreshed = current.clone();
    refreshed.secret = tokens.access_token;
    refreshed.refresh_token = tokens.refresh_token.or(refreshed.refresh_token);
    refreshed.expires_at = tokens
        .expires_in
        .map(|seconds| Utc::now().timestamp() + seconds);
    refreshed.chatgpt_account_id = chatgpt_account_id.or(refreshed.chatgpt_account_id);
    state.store.commit_refresh(&current, refreshed).await
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}
fn internal(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, message)
}
fn gateway(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Store;
    use axum::extract::Form;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    async fn test_state() -> (AppState, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("state.json"))
            .await
            .unwrap();
        (
            AppState {
                store,
                client: reqwest::Client::new(),
                api_key: None,
                management_token: "test".into(),
                demo: false,
            },
            directory,
        )
    }

    #[tokio::test]
    async fn callback_exchanges_pkce_once_and_persists_account_id() {
        let (state, _directory) = test_state().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let id_token = format!(
            "x.{}.x",
            URL_SAFE_NO_PAD
                .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-42"}}"#)
        );
        let app = Router::new().route("/token", post(move |Form(form): Form<HashMap<String, String>>| {
            let seen = seen.clone();
            let id_token = id_token.clone();
            async move {
                assert_eq!(form.get("code_verifier").map(String::as_str), Some("test-verifier"));
                assert_eq!(form.get("redirect_uri").map(String::as_str), Some(CODEX_REDIRECT));
                seen.fetch_add(1, Ordering::SeqCst);
                Json(json!({"access_token":"access-secret", "refresh_token":"refresh-secret", "expires_in":3600, "id_token":id_token}))
            }
        }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let oauth_state = random_urlsafe();
        pending().lock().await.insert(
            oauth_state.clone(),
            Pending {
                provider: Provider::Codex,
                label: "Personal (experimental)".into(),
                verifier: "test-verifier".into(),
                expires: Instant::now() + Duration::from_secs(60),
                callback_url: None,
            },
        );
        let callback_url = format!("{CODEX_REDIRECT}?code=private-code&state={oauth_state}");
        let (_, Json(view)) = complete_with_endpoint(
            state.clone(),
            CompleteRequest {
                state: oauth_state.clone(),
                callback_url: callback_url.clone(),
            },
            Some(&endpoint),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            !serde_json::to_string(&view)
                .unwrap()
                .contains("access-secret")
        );
        let saved = state.store.account(&view.id).await.unwrap();
        assert_eq!(saved.chatgpt_account_id.as_deref(), Some("acct-42"));
        assert_eq!(saved.refresh_token.as_deref(), Some("refresh-secret"));
        let replay = complete_with_endpoint(
            state,
            CompleteRequest {
                state: oauth_state,
                callback_url,
            },
            Some(&endpoint),
        )
        .await
        .unwrap_err();
        assert_eq!(replay.status, StatusCode::BAD_REQUEST);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_rotates_once_and_keeps_intervening_label_edit() {
        let (state, _directory) = test_state().await;
        let account = account_from_credentials(
            Some(Provider::Codex),
            Some("Original".into()),
            &json!({"access_token":"old-access", "refresh_token":"old-refresh"}),
        )
        .unwrap();
        state.store.save_account(account.clone()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let store = state.store.clone();
        let app = Router::new().route("/token", post(move |Form(form): Form<HashMap<String, String>>| {
            let seen = seen.clone();
            let store = store.clone();
            async move {
                assert_eq!(form.get("refresh_token").map(String::as_str), Some("old-refresh"));
                seen.fetch_add(1, Ordering::SeqCst);
                if let Some(mut edited) = store.raw_accounts().await.into_iter().next() {
                    edited.label = "Edited while refreshing".into();
                    let _ = store.save_account(edited).await;
                }
                Json(json!({"access_token":"new-access", "refresh_token":"new-refresh", "expires_in":3600}))
            }
        }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let (first, second) = tokio::join!(
            refresh_with_endpoint(&state, &account, &endpoint, CODEX_CLIENT),
            refresh_with_endpoint(&state, &account, &endpoint, CODEX_CLIENT),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.label, "Edited while refreshing");
        assert_eq!(second.secret, "new-access");
        let saved = state.store.account(&account.id).await.unwrap();
        assert_eq!(saved.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(saved.label, "Edited while refreshing");
    }

    #[tokio::test]
    async fn expired_state_is_rejected_without_network() {
        let (state, _directory) = test_state().await;
        let oauth_state = random_urlsafe();
        pending().lock().await.insert(
            oauth_state.clone(),
            Pending {
                provider: Provider::Claude,
                label: "Claude (experimental)".into(),
                verifier: "verifier".into(),
                expires: Instant::now() - Duration::from_secs(1),
                callback_url: None,
            },
        );
        let error = complete_with_endpoint(
            state,
            CompleteRequest {
                state: oauth_state.clone(),
                callback_url: format!("{CLAUDE_REDIRECT}?code=x&state={oauth_state}"),
            },
            Some("http://127.0.0.1:1/token"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.message, "expired OAuth state");
    }
}
