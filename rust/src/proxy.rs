use std::{collections::HashSet, io, sync::Arc, time::Instant};

use async_stream::stream;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use futures_util::StreamExt;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    core::{Account, ApiError, AppState, Provider, Store, UsageRecord},
    oauth,
    protocol::{self, Format},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(responses))
        .route("/v1/messages", post(messages))
        .route("/v1beta/models/{*action}", post(gemini))
}

async fn models(State(state): State<AppState>) -> impl IntoResponse {
    let accounts = state.store.raw_accounts().await;
    let mut seen = HashSet::new();
    let mut data = Vec::new();
    for account in accounts.into_iter().filter(|a| a.enabled) {
        for model in account.models {
            if seen.insert(model.clone()) {
                data.push(
                    json!({"id":model,"object":"model","owned_by":account.provider.as_str()}),
                );
            }
        }
    }
    (
        StatusCode::OK,
        axum::Json(json!({"object":"list","data":data})),
    )
}

async fn chat(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    dispatch(state, headers, body, Format::Chat, None).await
}

async fn responses(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    dispatch(state, headers, body, Format::Responses, None).await
}

async fn messages(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    dispatch(state, headers, body, Format::Claude, None).await
}

async fn gemini(
    State(state): State<AppState>,
    Path(action): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((model, method)) = action.rsplit_once(':') else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "Gemini path must contain model:method",
        );
    };
    if !matches!(method, "generateContent" | "streamGenerateContent") || model.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "unsupported Gemini method");
    }
    let path_model = model.trim_start_matches('/').to_owned();
    dispatch(
        state,
        headers,
        body,
        Format::Gemini,
        Some((
            path_model,
            method.to_owned(),
            uri.query().map(str::to_owned),
        )),
    )
    .await
}

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    ApiError::new(status, message).into_response()
}

fn parse_provider(s: &str) -> Option<Provider> {
    match s {
        "openai" => Some(Provider::Openai),
        "claude" | "anthropic" => Some(Provider::Claude),
        "gemini" | "google" => Some(Provider::Gemini),
        "codex" => Some(Provider::Codex),
        "openai_compatible" | "openai-compatible" => Some(Provider::OpenaiCompatible),
        _ => None,
    }
}

fn native_format(provider: &Provider) -> Format {
    match provider {
        Provider::Openai | Provider::OpenaiCompatible => Format::Chat,
        Provider::Claude => Format::Claude,
        Provider::Gemini => Format::Gemini,
        Provider::Codex => Format::Responses,
    }
}

async fn resolve_provider(
    state: &AppState,
    requested: &str,
    source: Format,
) -> Result<(Provider, String), String> {
    if let Some((prefix, model)) = requested.split_once('/')
        && let Some(provider) = parse_provider(prefix)
    {
        if model.is_empty() {
            return Err("model name after provider prefix is empty".into());
        }
        return Ok((provider, model.to_owned()));
    }
    let accounts = state.store.raw_accounts().await;
    let matches = accounts
        .iter()
        .filter(|a| a.enabled && a.models.iter().any(|m| m == requested))
        .map(|a| a.provider.as_str())
        .collect::<HashSet<_>>();
    if matches.len() == 1 {
        return Ok((
            parse_provider(matches.into_iter().next().unwrap())
                .ok_or("unrecognized configured provider")?,
            requested.to_owned(),
        ));
    }
    if matches.len() > 1 {
        return Err(format!(
            "model {requested} is configured for multiple providers; use provider/model"
        ));
    }
    let inferred = if requested.starts_with("claude-") {
        Some(Provider::Claude)
    } else if requested.starts_with("gemini-") {
        Some(Provider::Gemini)
    } else if requested.starts_with("gpt-")
        || requested.starts_with("o1")
        || requested.starts_with("o3")
        || requested.starts_with("o4")
    {
        Some(Provider::Openai)
    } else {
        None
    };
    if let Some(provider) = inferred {
        return Ok((provider, requested.to_owned()));
    }
    let hint = match source {
        Format::Claude => Provider::Claude,
        Format::Gemini => Provider::Gemini,
        Format::Chat | Format::Responses => Provider::Openai,
    };
    if accounts
        .iter()
        .any(|a| a.enabled && a.provider == hint && a.models.is_empty())
    {
        return Ok((hint, requested.to_owned()));
    }
    Err(format!(
        "cannot identify a provider for model {requested}; use provider/model"
    ))
}

fn target_format(provider: &Provider, source: Format) -> Format {
    match provider {
        Provider::Openai | Provider::OpenaiCompatible if source == Format::Responses => {
            Format::Responses
        }
        _ => native_format(provider),
    }
}

fn endpoint(
    format: Format,
    model: &str,
    gemini_method: Option<&str>,
    account: &Account,
) -> Result<String, String> {
    let base = if account.base_url.trim().is_empty() {
        account.provider.default_base_url()
    } else {
        account.base_url.as_str()
    };
    let base = base.trim_end_matches('/');
    let suffix = match (&account.provider, format) {
        (Provider::Codex, Format::Responses) if base.ends_with("/backend-api") => {
            "/codex/responses".to_owned()
        }
        (Provider::Codex, Format::Responses) => "/responses".to_owned(),
        (_, Format::Chat) => "/v1/chat/completions".to_owned(),
        (_, Format::Responses) => "/v1/responses".to_owned(),
        (_, Format::Claude) => "/v1/messages".to_owned(),
        (_, Format::Gemini) => format!(
            "/v1beta/models/{model}:{}",
            gemini_method.unwrap_or("generateContent")
        ),
    };
    let suffix = if base.ends_with("/v1") && suffix.starts_with("/v1/") {
        &suffix[3..]
    } else if base.ends_with("/v1beta") && suffix.starts_with("/v1beta/") {
        &suffix[7..]
    } else {
        suffix.as_str()
    };
    let url = format!("{base}{suffix}");
    let parsed = reqwest::Url::parse(&url).map_err(|_| "invalid account base URL")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(
            "account base URL must be an HTTP(S) URL without credentials or fragment".into(),
        );
    }
    Ok(url)
}

fn request_headers(
    account: &Account,
    input: &HeaderMap,
    target: Format,
) -> Result<HeaderMap, String> {
    let mut out = HeaderMap::new();
    out.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    out.insert(
        header::ACCEPT,
        if account.provider == Provider::Codex || target == Format::Gemini {
            "text/event-stream"
        } else {
            "application/json"
        }
        .parse()
        .unwrap(),
    );
    let secret = account.secret.trim();
    if secret.is_empty() {
        return Err("selected account has no secret".into());
    }
    let value = if matches!(target, Format::Claude | Format::Gemini) && account.auth_kind != "oauth"
    {
        secret.to_owned()
    } else {
        format!("Bearer {secret}")
    };
    let value = value
        .parse()
        .map_err(|_| "account credential contains invalid header characters")?;
    if target == Format::Claude {
        out.insert(
            if account.auth_kind == "oauth" {
                header::AUTHORIZATION
            } else {
                header::HeaderName::from_static("x-api-key")
            },
            value,
        );
        out.insert(
            "anthropic-version",
            input
                .get("anthropic-version")
                .cloned()
                .unwrap_or_else(|| "2023-06-01".parse().unwrap()),
        );
        if let Some(v) = input.get("anthropic-beta") {
            out.insert("anthropic-beta", v.clone());
        }
    } else if target == Format::Gemini {
        out.insert(
            if account.auth_kind == "oauth" {
                header::AUTHORIZATION
            } else {
                header::HeaderName::from_static("x-goog-api-key")
            },
            value,
        );
    } else {
        out.insert(header::AUTHORIZATION, value);
        if account.provider == Provider::Codex {
            if let Some(id) = account
                .chatgpt_account_id
                .as_deref()
                .filter(|v| !v.is_empty())
            {
                out.insert(
                    "chatgpt-account-id",
                    id.parse()
                        .map_err(|_| "ChatGPT account ID contains invalid header characters")?,
                );
            }
            out.insert("originator", "codex_cli_rs".parse().unwrap());
        }
    }
    Ok(out)
}

#[derive(Default, Clone)]
struct Usage {
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    reasoning: Option<u64>,
}

impl Usage {
    fn read(&mut self, value: &Value) {
        let usage = value
            .get("usage")
            .or_else(|| value.pointer("/response/usage"))
            .or_else(|| value.pointer("/message/usage"))
            .or_else(|| value.get("usageMetadata"));
        let Some(u) = usage else {
            return;
        };
        self.input =
            number(u, &["input_tokens", "prompt_tokens", "promptTokenCount"]).or(self.input);
        self.output = number(
            u,
            &["output_tokens", "completion_tokens", "candidatesTokenCount"],
        )
        .or(self.output);
        self.cached = number(u, &["cached_tokens", "cachedContentTokenCount"])
            .or_else(|| number(u, &["cache_read_input_tokens"]))
            .or_else(|| {
                u.pointer("/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
            })
            .or_else(|| {
                u.pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
            })
            .or(self.cached);
        self.reasoning = number(u, &["reasoning_tokens", "thoughtsTokenCount"])
            .or_else(|| {
                u.pointer("/output_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64)
            })
            .or_else(|| {
                u.pointer("/completion_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64)
            })
            .or(self.reasoning);
    }
}

fn number(value: &Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_u64))
}

struct SseParser {
    line: Vec<u8>,
    data: Vec<u8>,
    event: Vec<u8>,
    terminal: bool,
    failed: Option<String>,
    usage: Usage,
    codex_response: Option<Value>,
    codex_items: Vec<Value>,
    format: Format,
    frames: usize,
}

impl SseParser {
    fn new(format: Format) -> Self {
        Self {
            line: vec![],
            data: vec![],
            event: vec![],
            terminal: false,
            failed: None,
            usage: Usage::default(),
            codex_response: None,
            codex_items: vec![],
            format,
            frames: 0,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        for &b in bytes {
            if b == b'\n' {
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.line(&line)?;
            } else {
                self.line.push(b);
                if self.line.len() > 4 * 1024 * 1024 {
                    return Err("SSE line exceeds 4 MiB".into());
                }
            }
        }
        Ok(())
    }

    fn line(&mut self, line: &[u8]) -> Result<(), String> {
        if line.is_empty() {
            return self.frame();
        }
        if line.first() == Some(&b':') {
            return Ok(());
        }
        let (key, value) = match line.iter().position(|&b| b == b':') {
            Some(i) => (&line[..i], line.get(i + 1..).unwrap_or_default()),
            None => (line, &[][..]),
        };
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match key {
            b"data" => {
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value);
                if self.data.len() > 16 * 1024 * 1024 {
                    return Err("SSE event exceeds 16 MiB".into());
                }
            }
            b"event" => self.event = value.to_vec(),
            _ => {}
        }
        Ok(())
    }

    fn frame(&mut self) -> Result<(), String> {
        let data = std::mem::take(&mut self.data);
        let event = std::mem::take(&mut self.event);
        if data.is_empty() {
            return Ok(());
        }
        self.frames += 1;
        if data == b"[DONE]" {
            self.terminal = true;
            return Ok(());
        }
        let value: Value =
            serde_json::from_slice(&data).map_err(|_| "invalid upstream SSE JSON event")?;
        self.usage.read(&value);
        let event_kind = std::str::from_utf8(&event).unwrap_or("");
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event_kind);
        if event_kind == "error"
            || kind == "error"
            || kind == "response.failed"
            || value.get("error").is_some_and(|v| !v.is_null())
        {
            self.failed = Some("upstream stream failed".to_owned());
        }
        if kind == "response.output_item.done"
            && let Some(item) = value.get("item")
        {
            self.codex_items.push(item.clone());
        }
        if matches!(kind, "response.completed" | "response.incomplete") {
            self.terminal = true;
            if let Some(response) = value.get("response") {
                self.codex_response = Some(response.clone());
            }
        }
        if kind == "message_stop" {
            self.terminal = true;
        }
        if self.format == Format::Gemini
            && value
                .pointer("/candidates/0/finishReason")
                .and_then(Value::as_str)
                .is_some()
        {
            self.terminal = true;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), String> {
        if !self.line.is_empty() {
            return Err("upstream SSE ended in a partial line".into());
        }
        if !self.data.is_empty() || !self.event.is_empty() {
            return Err("upstream SSE ended in a partial event".into());
        }
        if let Some(reason) = &self.failed {
            return Err(reason.clone());
        }
        if !self.terminal {
            return Err("upstream stream ended before a terminal event".into());
        }
        Ok(())
    }
}

struct StreamUsage {
    store: Arc<Store>,
    record: Option<UsageRecord>,
}

impl StreamUsage {
    fn update(&mut self, usage: &Usage, status: u16, error: Option<String>, started: Instant) {
        if let Some(record) = &mut self.record {
            record.input_tokens = usage.input;
            record.output_tokens = usage.output;
            record.cached_tokens = usage.cached;
            record.reasoning_tokens = usage.reasoning;
            record.status = status;
            record.error = error;
            record.duration_ms = started.elapsed().as_millis() as u64;
        }
    }

    async fn persist(&mut self) {
        if let Some(record) = self.record.take()
            && let Err(err) = self.store.record_usage(record).await
        {
            tracing::error!("failed to persist stream usage: {err:?}");
        }
    }
}

impl Drop for StreamUsage {
    fn drop(&mut self) {
        if let Some(record) = self.record.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let store = self.store.clone();
            runtime.spawn(async move {
                if let Err(err) = store.record_usage(record).await {
                    tracing::error!("failed to persist cancelled stream usage: {err:?}");
                }
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn usage_record(
    account: &Account,
    model: &str,
    endpoint: &str,
    stream: bool,
    status: u16,
    started: Instant,
    usage: &Usage,
    error: Option<String>,
) -> UsageRecord {
    UsageRecord {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        account_id: account.id.clone(),
        account_label: account.label.clone(),
        provider: account.provider,
        model: model.to_owned(),
        endpoint: endpoint.to_owned(),
        status,
        duration_ms: started.elapsed().as_millis() as u64,
        input_tokens: usage.input,
        output_tokens: usage.output,
        cached_tokens: usage.cached,
        reasoning_tokens: usage.reasoning,
        stream,
        error,
    }
}

#[allow(clippy::too_many_arguments)]
async fn record(
    state: &AppState,
    account: &Account,
    model: &str,
    endpoint: &str,
    stream: bool,
    status: u16,
    started: Instant,
    usage: &Usage,
    error: Option<String>,
) {
    let _ = state
        .store
        .record_usage(usage_record(
            account, model, endpoint, stream, status, started, usage, error,
        ))
        .await;
}

fn retry_after(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

fn retryable(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

async fn dispatch(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    source: Format,
    gemini_path: Option<(String, String, Option<String>)>,
) -> Response {
    let mut payload: Value = match serde_json::from_slice::<Value>(&body) {
        Ok(v) if v.is_object() => v,
        _ => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "request body must be a JSON object",
            );
        }
    };
    let requested = match &gemini_path {
        Some((model, _, _)) => model.as_str(),
        None => match payload.get("model").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return api_error(StatusCode::BAD_REQUEST, "model is required"),
        },
    };
    let (provider, model) = match resolve_provider(&state, requested, source).await {
        Ok(v) => v,
        Err(err) => return api_error(StatusCode::BAD_REQUEST, err),
    };
    let target = target_format(&provider, source);
    let client_stream = gemini_path
        .as_ref()
        .is_some_and(|(_, action, _)| action == "streamGenerateContent")
        || payload
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if source != target && client_stream {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "streaming cross-format conversion is not supported",
        );
    }
    if source != target && provider == Provider::Codex && source != Format::Chat {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "Codex supports Responses and basic chat requests",
        );
    }
    payload = match protocol::convert_request(source, target, &payload) {
        Ok(v) => v,
        Err(err) => return api_error(StatusCode::BAD_REQUEST, err),
    };
    if target != Format::Gemini {
        payload["model"] = json!(model);
    }
    if provider == Provider::Codex {
        payload["stream"] = json!(true);
        if payload.get("instructions").is_none() {
            payload["instructions"] = json!("");
        }
        if payload.get("store").is_none() {
            payload["store"] = json!(false);
        }
    } else if target != Format::Gemini {
        payload["stream"] = json!(client_stream);
    }
    let mut excluded = Vec::new();
    let attempt_limit = state.store.raw_accounts().await.len().max(1) * 2;
    let mut refreshed = HashSet::new();
    let mut retry_account = None;
    let mut last_error = None;
    for _ in 0..attempt_limit {
        let Some(mut account) = (match retry_account.take() {
            Some(account) => Some(account),
            None => {
                state
                    .store
                    .select_account(provider, &model, &excluded)
                    .await
            }
        }) else {
            break;
        };
        if !excluded.contains(&account.id) {
            excluded.push(account.id.clone());
        }
        if account.auth_kind == "oauth"
            && !refreshed.contains(&account.id)
            && account
                .expires_at
                .is_some_and(|expiry| expiry <= Utc::now().timestamp() + 60)
        {
            match oauth::refresh_account(&state, &account).await {
                Ok(updated) => {
                    refreshed.insert(account.id.clone());
                    account = updated;
                }
                Err(_) => {
                    last_error = Some("credential refresh failed".into());
                    continue;
                }
            }
        }
        let started = Instant::now();
        let url = match endpoint(
            target,
            &model,
            gemini_path.as_ref().map(|(_, method, _)| method.as_str()),
            &account,
        ) {
            Ok(url) => url,
            Err(err) => return api_error(StatusCode::BAD_REQUEST, err),
        };
        let mut url = match reqwest::Url::parse(&url) {
            Ok(url) => url,
            Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid upstream URL"),
        };
        if target == Format::Gemini && client_stream {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
        if let Some((_, _, Some(query))) = &gemini_path {
            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                if !matches!(key.as_ref(), "key" | "alt") {
                    url.query_pairs_mut().append_pair(&key, &value);
                }
            }
        }
        let req_headers = match request_headers(&account, &headers, target) {
            Ok(v) => v,
            Err(err) => return api_error(StatusCode::BAD_REQUEST, err),
        };
        let upstream = state
            .client
            .post(url)
            .headers(req_headers)
            .json(&payload)
            .send()
            .await;
        let upstream = match upstream {
            Ok(resp) => resp,
            Err(_) => {
                let message = "upstream connection failed".to_owned();
                record(
                    &state,
                    &account,
                    &model,
                    &format!("{source:?}"),
                    client_stream,
                    502,
                    started,
                    &Usage::default(),
                    Some(message.clone()),
                )
                .await;
                last_error = Some(message);
                continue;
            }
        };
        let status = upstream.status();
        let response_headers = upstream.headers().clone();
        if !status.is_success() {
            let retry = retryable(status);
            let retry_after = retry_after(&response_headers);
            let _ = upstream.bytes().await;
            let message = format!("upstream returned {status}");
            record(
                &state,
                &account,
                &model,
                &format!("{source:?}"),
                client_stream,
                status.as_u16(),
                started,
                &Usage::default(),
                Some(message.clone()),
            )
            .await;
            if status == StatusCode::UNAUTHORIZED
                && account.auth_kind == "oauth"
                && !refreshed.contains(&account.id)
            {
                refreshed.insert(account.id.clone());
                if let Ok(updated) = oauth::refresh_account(&state, &account).await {
                    retry_account = Some(updated);
                    continue;
                }
            }
            state
                .store
                .mark_result(&account.id, status.as_u16(), retry_after)
                .await;
            if retry {
                last_error = Some(message);
                continue;
            }
            return api_error(status, message);
        }
        state
            .store
            .mark_result(&account.id, status.as_u16(), None)
            .await;
        if client_stream {
            let mut parser = SseParser::new(target);
            let mut upstream_stream = upstream.bytes_stream();
            let mut initial = Vec::new();
            let mut initial_size = 0;
            while parser.frames == 0 {
                let Some(chunk) = upstream_stream.next().await else {
                    let err = parser
                        .finish()
                        .err()
                        .unwrap_or_else(|| "upstream stream ended before its first event".into());
                    record(
                        &state,
                        &account,
                        &model,
                        &format!("{source:?}"),
                        true,
                        502,
                        started,
                        &parser.usage,
                        Some(err.clone()),
                    )
                    .await;
                    return api_error(StatusCode::BAD_GATEWAY, err);
                };
                let bytes = match chunk {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        record(
                            &state,
                            &account,
                            &model,
                            &format!("{source:?}"),
                            true,
                            502,
                            started,
                            &parser.usage,
                            Some("upstream stream read failed".into()),
                        )
                        .await;
                        return api_error(StatusCode::BAD_GATEWAY, "upstream stream read failed");
                    }
                };
                if let Err(err) = parser.feed(&bytes) {
                    record(
                        &state,
                        &account,
                        &model,
                        &format!("{source:?}"),
                        true,
                        502,
                        started,
                        &parser.usage,
                        Some(err.clone()),
                    )
                    .await;
                    return api_error(StatusCode::BAD_GATEWAY, err);
                }
                initial_size += bytes.len();
                if initial_size > 16 * 1024 * 1024 {
                    record(
                        &state,
                        &account,
                        &model,
                        &format!("{source:?}"),
                        true,
                        502,
                        started,
                        &parser.usage,
                        Some("upstream SSE prelude exceeds 16 MiB".into()),
                    )
                    .await;
                    return api_error(
                        StatusCode::BAD_GATEWAY,
                        "upstream SSE prelude exceeds 16 MiB",
                    );
                }
                initial.push(bytes);
            }
            if let Some(err) = &parser.failed {
                record(
                    &state,
                    &account,
                    &model,
                    &format!("{source:?}"),
                    true,
                    502,
                    started,
                    &parser.usage,
                    Some(err.clone()),
                )
                .await;
                return api_error(StatusCode::BAD_GATEWAY, err.clone());
            }
            let record = usage_record(
                &account,
                &model,
                &format!("{source:?}"),
                true,
                499,
                started,
                &parser.usage,
                Some("client cancelled stream".into()),
            );
            let store = state.store.clone();
            let guard = StreamUsage {
                store,
                record: Some(record),
            };
            let body_stream = stream! {
                let mut guard = guard;
                if parser.terminal {
                    guard.update(&parser.usage, 200, None, started);
                    guard.persist().await;
                }
                for bytes in initial {
                    if !parser.terminal {
                        guard.update(&parser.usage, 499, Some("client cancelled stream".into()), started);
                    }
                    yield Ok::<Bytes, io::Error>(bytes);
                }
                if parser.terminal {
                    return;
                }
                while let Some(chunk) = upstream_stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if let Err(err) = parser.feed(&bytes) {
                                guard.update(&parser.usage, 502, Some(err.clone()), started);
                                guard.persist().await;
                                yield Err::<Bytes, io::Error>(io::Error::new(io::ErrorKind::InvalidData, err));
                                return;
                            }
                            if let Some(err) = &parser.failed {
                                let err = err.clone();
                                guard.update(&parser.usage, 502, Some(err.clone()), started);
                                guard.persist().await;
                                yield Err::<Bytes, io::Error>(io::Error::other(err));
                                return;
                            }
                            if parser.terminal {
                                guard.update(&parser.usage, 200, None, started);
                                guard.persist().await;
                            } else {
                                guard.update(&parser.usage, 499, Some("client cancelled stream".into()), started);
                            }
                            yield Ok::<Bytes, io::Error>(bytes);
                            if parser.terminal {
                                return;
                            }
                        }
                        Err(_) => {
                            let message = "upstream stream read failed".to_owned();
                            guard.update(&parser.usage, 502, Some(message.clone()), started);
                            guard.persist().await;
                            yield Err::<Bytes, io::Error>(io::Error::other(message));
                            return;
                        }
                    }
                }
                match parser.finish() {
                    Ok(()) => {
                        guard.update(&parser.usage, 200, None, started);
                        guard.persist().await;
                    },
                    Err(err) => {
                        guard.update(&parser.usage, 502, Some(err.clone()), started);
                        guard.persist().await;
                        yield Err::<Bytes, io::Error>(io::Error::new(io::ErrorKind::UnexpectedEof, err));
                    }
                }
            };
            let mut response = Response::new(Body::from_stream(body_stream));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
            return response;
        }
        if provider == Provider::Codex {
            let mut parser = SseParser::new(Format::Responses);
            let mut upstream_stream = upstream.bytes_stream();
            let mut failure = None;
            while let Some(chunk) = upstream_stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        if let Err(err) = parser.feed(&bytes) {
                            failure = Some(err);
                            break;
                        }
                        if parser.terminal {
                            break;
                        }
                    }
                    Err(_) => {
                        failure = Some("upstream stream read failed".into());
                        break;
                    }
                }
            }
            if failure.is_none() {
                failure = parser.finish().err();
            }
            if let Some(err) = failure {
                record(
                    &state,
                    &account,
                    &model,
                    &format!("{source:?}"),
                    false,
                    502,
                    started,
                    &parser.usage,
                    Some(err.clone()),
                )
                .await;
                return api_error(StatusCode::BAD_GATEWAY, err);
            }
            let Some(mut value) = parser.codex_response else {
                record(
                    &state,
                    &account,
                    &model,
                    &format!("{source:?}"),
                    false,
                    502,
                    started,
                    &parser.usage,
                    Some("Codex terminal response missing".into()),
                )
                .await;
                return api_error(StatusCode::BAD_GATEWAY, "Codex terminal response missing");
            };
            if value
                .get("output")
                .and_then(Value::as_array)
                .is_none_or(|items| items.is_empty())
                && !parser.codex_items.is_empty()
            {
                value["output"] = json!(parser.codex_items);
            }
            let value = if source != target {
                match protocol::convert_response(target, source, &value, &model) {
                    Ok(converted) => converted,
                    Err(err) => {
                        record(
                            &state,
                            &account,
                            &model,
                            &format!("{source:?}"),
                            false,
                            502,
                            started,
                            &parser.usage,
                            Some(err.clone()),
                        )
                        .await;
                        return api_error(StatusCode::BAD_GATEWAY, err);
                    }
                }
            } else {
                value
            };
            record(
                &state,
                &account,
                &model,
                &format!("{source:?}"),
                false,
                200,
                started,
                &parser.usage,
                None,
            )
            .await;
            return axum::Json(value).into_response();
        }
        let bytes = match upstream.bytes().await {
            Ok(bytes) => bytes,
            Err(_) => {
                let message = "upstream response read failed".to_owned();
                record(
                    &state,
                    &account,
                    &model,
                    &format!("{source:?}"),
                    false,
                    502,
                    started,
                    &Usage::default(),
                    Some(message.clone()),
                )
                .await;
                return api_error(StatusCode::BAD_GATEWAY, message);
            }
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                record(
                    &state,
                    &account,
                    &model,
                    &format!("{source:?}"),
                    false,
                    502,
                    started,
                    &Usage::default(),
                    Some("upstream returned invalid JSON".into()),
                )
                .await;
                return api_error(StatusCode::BAD_GATEWAY, "upstream returned invalid JSON");
            }
        };
        let mut usage = Usage::default();
        usage.read(&value);
        let out = if source != target {
            match protocol::convert_response(target, source, &value, &model) {
                Ok(out) => out,
                Err(err) => {
                    record(
                        &state,
                        &account,
                        &model,
                        &format!("{source:?}"),
                        false,
                        502,
                        started,
                        &usage,
                        Some(err.clone()),
                    )
                    .await;
                    return api_error(StatusCode::BAD_GATEWAY, err);
                }
            }
        } else {
            value
        };
        record(
            &state,
            &account,
            &model,
            &format!("{source:?}"),
            false,
            200,
            started,
            &usage,
            None,
        )
        .await;
        return axum::Json(out).into_response();
    }
    api_error(
        StatusCode::BAD_GATEWAY,
        last_error.unwrap_or_else(|| "no enabled account can serve this provider and model".into()),
    )
}
