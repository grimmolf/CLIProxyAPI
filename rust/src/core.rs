use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

// Retain the newest 10,000 usage rows so local dashboards stay useful without unbounded disk growth.
const MAX_USAGE_RECORDS: usize = 10_000;

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Openai,
    Claude,
    Gemini,
    Codex,
    OpenaiCompatible,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::Codex => "codex",
            Self::OpenaiCompatible => "openai_compatible",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Openai => "https://api.openai.com",
            Self::Claude => "https://api.anthropic.com",
            Self::Gemini => "https://generativelanguage.googleapis.com",
            Self::Codex => "https://chatgpt.com/backend-api/codex",
            Self::OpenaiCompatible => "http://localhost:8000",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Openai => "OpenAI",
            Self::Claude => "Claude",
            Self::Gemini => "Gemini",
            Self::Codex => "Codex",
            Self::OpenaiCompatible => "OpenAI Compatible",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub provider: Provider,
    pub base_url: String,
    #[serde(default)]
    pub secret: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub chatgpt_account_id: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub models: Vec<String>,
    pub created_at: String,
    #[serde(default = "default_auth_kind")]
    pub auth_kind: String,
}

fn default_true() -> bool {
    true
}

fn default_auth_kind() -> String {
    "api_key".to_owned()
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountView {
    pub id: String,
    pub label: String,
    pub provider: Provider,
    pub base_url: String,
    pub expires_at: Option<i64>,
    pub enabled: bool,
    pub models: Vec<String>,
    pub created_at: String,
    pub auth_kind: String,
    pub has_secret: bool,
    pub status: String,
    pub cooldown_until: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageRecord {
    pub id: String,
    pub timestamp: String,
    pub account_id: String,
    pub account_label: String,
    pub provider: Provider,
    pub model: String,
    pub endpoint: String,
    pub status: u16,
    pub duration_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub stream: bool,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": {"message": self.message, "type": "proxy_error"}})),
        )
            .into_response()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub client: reqwest::Client,
    pub api_key: Option<String>,
    pub management_token: String,
    pub demo: bool,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct DiskState {
    #[serde(default)]
    accounts: Vec<Account>,
    #[serde(default)]
    usage: Vec<UsageRecord>,
}

#[derive(Default)]
struct RuntimeState {
    data: DiskState,
    cooldowns: HashMap<String, i64>,
    last_selected: HashMap<Provider, String>,
}

pub struct Store {
    path: PathBuf,
    // ponytail: one mutex serializes all account and usage access; use SQLite/WAL if local dashboard traffic contends.
    state: Mutex<RuntimeState>,
}

impl Store {
    pub async fn open(path: PathBuf) -> Result<Arc<Self>, ApiError> {
        let state = if path.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                    .map_err(storage_error)?;
            }
            let bytes = fs::read(&path).map_err(storage_error)?;
            serde_json::from_slice(&bytes).map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("invalid account store: {error}"),
                )
            })?
        } else {
            DiskState::default()
        };
        let store = Arc::new(Self {
            path,
            state: Mutex::new(RuntimeState {
                data: state,
                ..RuntimeState::default()
            }),
        });
        if !store.path.exists() {
            let guard = store.state.lock().await;
            store.persist(&guard.data)?;
        }
        Ok(store)
    }

    pub async fn accounts(&self) -> Vec<AccountView> {
        let state = self.state.lock().await;
        state
            .data
            .accounts
            .iter()
            .map(|account| account_view(account, &state.cooldowns))
            .collect()
    }

    pub async fn raw_accounts(&self) -> Vec<Account> {
        self.state.lock().await.data.accounts.clone()
    }

    pub async fn account(&self, id: &str) -> Option<Account> {
        self.state
            .lock()
            .await
            .data
            .accounts
            .iter()
            .find(|account| account.id == id)
            .cloned()
    }

    pub async fn save_account(&self, account: Account) -> Result<AccountView, ApiError> {
        validate_account(&account)?;
        let mut state = self.state.lock().await;
        let mut next = state.data.clone();
        let position = next
            .accounts
            .iter()
            .position(|existing| existing.id == account.id);
        if let Some(index) = position {
            next.accounts[index] = account.clone();
        } else {
            next.accounts.push(account.clone());
        }
        self.persist(&next)?;
        state.data = next;
        Ok(account_view(&account, &state.cooldowns))
    }

    async fn patch_account(&self, id: &str, patch: PatchAccount) -> Result<AccountView, ApiError> {
        let mut state = self.state.lock().await;
        let mut next = state.data.clone();
        let account = next
            .accounts
            .iter_mut()
            .find(|account| account.id == id)
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "account not found"))?;
        if let Some(value) = patch.label {
            account.label = value;
        }
        if let Some(value) = patch.provider {
            account.provider = value;
        }
        if let Some(value) = patch.base_url {
            account.base_url = value;
        }
        if let Some(value) = patch.secret {
            account.secret = value;
        }
        if let Some(value) = patch.refresh_token {
            account.refresh_token = value;
        }
        if let Some(value) = patch.expires_at {
            account.expires_at = value;
        }
        if let Some(value) = patch.enabled {
            account.enabled = value;
        }
        if let Some(value) = patch.models {
            account.models = value;
        }
        if let Some(value) = patch.auth_kind {
            account.auth_kind = value;
        }
        validate_account(account)?;
        let result = account.clone();
        self.persist(&next)?;
        state.data = next;
        Ok(account_view(&result, &state.cooldowns))
    }

    pub async fn commit_refresh(
        &self,
        expected: &Account,
        refreshed: Account,
    ) -> Result<Account, ApiError> {
        let mut state = self.state.lock().await;
        let mut next = state.data.clone();
        let latest = next
            .accounts
            .iter_mut()
            .find(|account| account.id == expected.id)
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "account not found"))?;
        let credentials_unchanged = latest.provider == expected.provider
            && latest.auth_kind == expected.auth_kind
            && latest.secret == expected.secret
            && latest.refresh_token == expected.refresh_token
            && latest.expires_at == expected.expires_at;
        if !credentials_unchanged {
            return Ok(latest.clone());
        }
        latest.secret = refreshed.secret;
        latest.refresh_token = refreshed.refresh_token;
        latest.expires_at = refreshed.expires_at;
        latest.chatgpt_account_id = refreshed.chatgpt_account_id;
        validate_account(latest)?;
        let result = latest.clone();
        self.persist(&next)?;
        state.data = next;
        Ok(result)
    }

    pub async fn delete_account(&self, id: &str) -> Result<(), ApiError> {
        let mut state = self.state.lock().await;
        if !state.data.accounts.iter().any(|account| account.id == id) {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "account not found"));
        }
        let mut next = state.data.clone();
        next.accounts.retain(|account| account.id != id);
        self.persist(&next)?;
        state.data = next;
        state.cooldowns.remove(id);
        Ok(())
    }

    pub async fn select_account(
        &self,
        provider: Provider,
        model: &str,
        excluded: &[String],
    ) -> Option<Account> {
        let mut state = self.state.lock().await;
        let now = Utc::now().timestamp();
        let candidates: Vec<(usize, &Account)> = state
            .data
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| {
                account.provider == provider
                    && account.enabled
                    && !excluded.iter().any(|id| id == &account.id)
                    && (account.models.is_empty()
                        || account.models.iter().any(|allowed| allowed == model))
                    && state
                        .cooldowns
                        .get(&account.id)
                        .is_none_or(|until| *until <= now)
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let previous_index = state.last_selected.get(&provider).and_then(|previous| {
            state
                .data
                .accounts
                .iter()
                .position(|account| &account.id == previous)
        });
        let selected = previous_index
            .and_then(|previous| candidates.iter().find(|(index, _)| *index > previous))
            .unwrap_or(&candidates[0])
            .1
            .clone();
        drop(candidates);
        state.last_selected.insert(provider, selected.id.clone());
        Some(selected)
    }

    pub async fn mark_result(&self, id: &str, status: u16, retry_after: Option<u64>) {
        let mut state = self.state.lock().await;
        if status == 429 || status >= 500 {
            let seconds = retry_after.unwrap_or(30).clamp(1, 3600);
            state
                .cooldowns
                .insert(id.to_owned(), Utc::now().timestamp() + seconds as i64);
        } else if (200..300).contains(&status) {
            state.cooldowns.remove(id);
        }
    }

    pub async fn record_usage(&self, mut record: UsageRecord) -> Result<(), ApiError> {
        let mut state = self.state.lock().await;
        if let Some(error) = record.error.as_mut()
            && let Some(account) = state
                .data
                .accounts
                .iter()
                .find(|account| account.id == record.account_id)
        {
            for credential in [
                account.secret.as_str(),
                account.refresh_token.as_deref().unwrap_or(""),
            ] {
                if !credential.is_empty() {
                    *error = error.replace(credential, "[redacted]");
                }
            }
        }
        let mut next = state.data.clone();
        next.usage.push(record);
        if next.usage.len() > MAX_USAGE_RECORDS {
            let remove = next.usage.len() - MAX_USAGE_RECORDS;
            next.usage.drain(..remove);
        }
        self.persist(&next)?;
        state.data = next;
        Ok(())
    }

    pub async fn usage(&self) -> Vec<UsageRecord> {
        let mut records = self.state.lock().await.data.usage.clone();
        records.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
        records
    }

    pub async fn summary(&self) -> Value {
        let state = self.state.lock().await;
        let records = &state.data.usage;
        let mut providers: BTreeMap<Provider, Stats> = BTreeMap::new();
        let mut accounts: BTreeMap<String, AccountStats> = BTreeMap::new();
        let mut daily: BTreeMap<String, DailyStats> = BTreeMap::new();
        let mut total = Stats::default();
        let mut latency = 0u128;
        for record in records {
            let success = (200..300).contains(&record.status) && record.error.is_none();
            total.add(record, success);
            providers
                .entry(record.provider)
                .or_default()
                .add(record, success);
            let account = accounts
                .entry(record.account_id.clone())
                .or_insert_with(|| AccountStats {
                    account_id: record.account_id.clone(),
                    account_label: record.account_label.clone(),
                    provider: record.provider,
                    ..AccountStats::default()
                });
            account.add(record, success);
            let date = DateTime::parse_from_rfc3339(&record.timestamp)
                .map(|timestamp| timestamp.date_naive().to_string())
                .unwrap_or_else(|_| record.timestamp.get(..10).unwrap_or("").to_owned());
            daily.entry(date).or_default().add(record);
            latency += record.duration_ms as u128;
        }
        let enabled_accounts = state
            .data
            .accounts
            .iter()
            .filter(|account| account.enabled)
            .count();
        let provider_stats: Vec<Value> = Provider::all()
            .into_iter()
            .map(|provider| {
                let stats = providers.get(&provider).cloned().unwrap_or_default();
                json!({
                    "id": provider.as_str(),
                    "requests": stats.requests,
                    "successful_requests": stats.successful_requests,
                    "failed_requests": stats.failed_requests,
                    "input_tokens": stats.input_tokens,
                    "output_tokens": stats.output_tokens
                })
            })
            .collect();
        let daily_stats: Vec<Value> = daily
            .into_iter()
            .map(|(date, stats)| {
                json!({
                    "date": date, "requests": stats.requests,
                    "input_tokens": stats.input_tokens, "output_tokens": stats.output_tokens
                })
            })
            .collect();
        let account_stats: Vec<Value> = accounts
            .into_values()
            .map(|stats| {
                json!({
                    "account_id": stats.account_id, "account_label": stats.account_label,
                    "provider": stats.provider, "requests": stats.requests,
                    "failures": stats.failures, "input_tokens": stats.input_tokens,
                    "output_tokens": stats.output_tokens
                })
            })
            .collect();
        json!({
            "requests": total.requests,
            "successful_requests": total.successful_requests,
            "failed_requests": total.failed_requests,
            "input_tokens": total.input_tokens,
            "output_tokens": total.output_tokens,
            "cached_tokens": total.cached_tokens,
            "reasoning_tokens": total.reasoning_tokens,
            "average_latency_ms": if total.requests == 0 { 0 } else { (latency / total.requests as u128) as u64 },
            "accounts": state.data.accounts.len(),
            "enabled_accounts": enabled_accounts,
            "providers": provider_stats,
            "daily": daily_stats,
            "by_account": account_stats
        })
    }

    fn persist(&self, data: &DiskState) -> Result<(), ApiError> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(storage_error)?;
        let bytes = serde_json::to_vec_pretty(data).map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("serialize account store: {error}"),
            )
        })?;
        let temp_path = parent.join(format!(
            ".{}.{}.tmp",
            self.path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("accounts"),
            Uuid::new_v4()
        ));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            #[cfg(not(unix))]
            fs::set_permissions(&temp_path, fs::Permissions::readonly())?;
            fs::rename(&temp_path, &self.path)?;
            File::open(parent)?.sync_all()?;
            Ok::<(), std::io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result.map_err(storage_error)
    }
}

impl Provider {
    fn all() -> [Self; 5] {
        [
            Self::Openai,
            Self::Claude,
            Self::Gemini,
            Self::Codex,
            Self::OpenaiCompatible,
        ]
    }
}

fn storage_error(error: std::io::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("account store: {error}"),
    )
}

fn account_view(account: &Account, cooldowns: &HashMap<String, i64>) -> AccountView {
    let cooldown_until = cooldowns
        .get(&account.id)
        .copied()
        .filter(|until| *until > Utc::now().timestamp());
    let status = if !account.enabled {
        "disabled"
    } else if cooldown_until.is_some() {
        "cooldown"
    } else if account.secret.is_empty() && account.auth_kind == "api_key" {
        "needs_auth"
    } else {
        "ready"
    };
    AccountView {
        id: account.id.clone(),
        label: account.label.clone(),
        provider: account.provider,
        base_url: account.base_url.clone(),
        expires_at: account.expires_at,
        enabled: account.enabled,
        models: account.models.clone(),
        created_at: account.created_at.clone(),
        auth_kind: account.auth_kind.clone(),
        has_secret: !account.secret.is_empty(),
        status: status.to_owned(),
        cooldown_until,
    }
}

fn validate_account(account: &Account) -> Result<(), ApiError> {
    if account.label.trim().is_empty()
        || account.label.len() > 200
        || account.label.chars().any(char::is_control)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "label must contain 1 to 200 characters",
        ));
    }
    let parsed = url::Url::parse(&account.base_url).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "base_url must be an absolute HTTP(S) URL",
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "base_url must be an absolute HTTP(S) URL without credentials or fragments",
        ));
    }
    if account.auth_kind != "api_key" && account.auth_kind != "oauth" {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "auth_kind must be api_key or oauth",
        ));
    }
    if account.secret.len() > 64 * 1024
        || account.secret.chars().any(char::is_control)
        || account
            .refresh_token
            .as_ref()
            .is_some_and(|value| value.len() > 64 * 1024 || value.chars().any(char::is_control))
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid credential value",
        ));
    }
    if account.models.len() > 1024
        || account.models.iter().any(|model| {
            model.trim().is_empty() || model.len() > 256 || model.chars().any(char::is_control)
        })
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "models must be nonempty names without control characters",
        ));
    }
    Ok(())
}

#[derive(Default, Clone)]
struct Stats {
    requests: u64,
    successful_requests: u64,
    failed_requests: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    reasoning_tokens: u64,
}

impl Stats {
    fn add(&mut self, record: &UsageRecord, success: bool) {
        self.requests += 1;
        if success {
            self.successful_requests += 1;
        } else {
            self.failed_requests += 1;
        }
        self.input_tokens = self
            .input_tokens
            .saturating_add(record.input_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(record.output_tokens.unwrap_or(0));
        self.cached_tokens = self
            .cached_tokens
            .saturating_add(record.cached_tokens.unwrap_or(0));
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(record.reasoning_tokens.unwrap_or(0));
    }
}

#[derive(Default)]
struct AccountStats {
    account_id: String,
    account_label: String,
    provider: Provider,
    requests: u64,
    failures: u64,
    input_tokens: u64,
    output_tokens: u64,
}

impl AccountStats {
    fn add(&mut self, record: &UsageRecord, success: bool) {
        self.requests += 1;
        if !success {
            self.failures += 1;
        }
        self.input_tokens = self
            .input_tokens
            .saturating_add(record.input_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(record.output_tokens.unwrap_or(0));
    }
}

#[derive(Default)]
struct DailyStats {
    requests: u64,
    input_tokens: u64,
    output_tokens: u64,
}

impl DailyStats {
    fn add(&mut self, record: &UsageRecord) {
        self.requests += 1;
        self.input_tokens = self
            .input_tokens
            .saturating_add(record.input_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(record.output_tokens.unwrap_or(0));
    }
}

#[derive(Deserialize)]
struct CreateAccount {
    label: String,
    provider: Provider,
    base_url: Option<String>,
    #[serde(default)]
    secret: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default = "default_auth_kind")]
    auth_kind: String,
}

#[derive(Default, Deserialize)]
struct PatchAccount {
    label: Option<String>,
    provider: Option<Provider>,
    base_url: Option<String>,
    secret: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    refresh_token: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    expires_at: Option<Option<i64>>,
    enabled: Option<bool>,
    models: Option<Vec<String>>,
    auth_kind: Option<String>,
}

fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/accounts", get(list_accounts).post(create_account))
        .route(
            "/api/accounts/{id}",
            patch(update_account).delete(remove_account),
        )
        .route("/api/usage", get(list_usage))
        .route("/api/summary", get(summary))
        .route("/api/providers", get(providers))
        .route("/api/settings", get(settings))
}

async fn list_accounts(State(state): State<AppState>) -> Json<Vec<AccountView>> {
    Json(state.store.accounts().await)
}

async fn create_account(
    State(state): State<AppState>,
    Json(input): Json<CreateAccount>,
) -> Result<(StatusCode, Json<AccountView>), ApiError> {
    let account = Account {
        id: Uuid::new_v4().to_string(),
        label: input.label,
        provider: input.provider,
        base_url: input
            .base_url
            .unwrap_or_else(|| input.provider.default_base_url().to_owned()),
        secret: input.secret,
        refresh_token: input.refresh_token,
        chatgpt_account_id: None,
        expires_at: input.expires_at,
        enabled: input.enabled,
        models: input.models,
        created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        auth_kind: input.auth_kind,
    };
    Ok((
        StatusCode::CREATED,
        Json(state.store.save_account(account).await?),
    ))
}

async fn update_account(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(input): Json<PatchAccount>,
) -> Result<Json<AccountView>, ApiError> {
    Ok(Json(state.store.patch_account(&id, input).await?))
}

async fn remove_account(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    state.store.delete_account(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_usage(State(state): State<AppState>) -> Json<Vec<UsageRecord>> {
    Json(state.store.usage().await)
}

async fn summary(State(state): State<AppState>) -> Json<Value> {
    Json(state.store.summary().await)
}

async fn providers() -> Json<Vec<Value>> {
    Json(Provider::all().into_iter().map(|provider| json!({
        "id": provider.as_str(), "name": provider.display_name(), "base_url": provider.default_base_url(),
        "endpoints": match provider {
            Provider::Openai | Provider::OpenaiCompatible => vec!["/v1/chat/completions", "/v1/responses", "/v1/models"],
            Provider::Claude => vec!["/v1/messages", "/v1/models"],
            Provider::Gemini => vec!["/v1beta/models/{model}:generateContent", "/v1beta/models/{model}:streamGenerateContent"],
            Provider::Codex => vec!["/v1/responses"],
        },
        "description": format!("{} compatible API", provider.display_name())
    })).collect())
}

async fn settings(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "demo": state.demo,
        "inference_key_required": state.api_key.is_some(),
        "management_auth_required": !state.management_token.is_empty()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    fn sample_account(id: &str) -> Account {
        Account {
            id: id.to_owned(),
            label: id.to_owned(),
            provider: Provider::Openai,
            base_url: Provider::Openai.default_base_url().to_owned(),
            secret: format!("secret-{id}"),
            refresh_token: Some(format!("refresh-{id}")),
            chatgpt_account_id: None,
            expires_at: None,
            enabled: true,
            models: vec!["gpt-test".to_owned()],
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            auth_kind: "api_key".to_owned(),
        }
    }

    #[tokio::test]
    async fn persists_accounts_and_never_exposes_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.json");
        let store = Store::open(path.clone()).await.unwrap();
        store.save_account(sample_account("one")).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let view = serde_json::to_string(&store.accounts().await).unwrap();
        assert!(view.contains("has_secret"));
        assert!(!view.contains("secret-one"));
        assert!(!view.contains("refresh-one"));
        drop(store);
        let reopened = Store::open(path).await.unwrap();
        assert_eq!(reopened.raw_accounts().await[0].secret, "secret-one");
    }

    #[tokio::test]
    async fn patch_preserves_absent_nullable_fields_and_clears_explicit_nulls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.json");
        let store = Store::open(path.clone()).await.unwrap();
        let mut account = sample_account("one");
        account.expires_at = Some(1_900_000_000);
        store.save_account(account.clone()).await.unwrap();

        let absent = serde_json::from_value(json!({"label":"renamed"})).unwrap();
        store.patch_account(&account.id, absent).await.unwrap();
        let preserved = store.account(&account.id).await.unwrap();
        assert_eq!(preserved.refresh_token.as_deref(), Some("refresh-one"));
        assert_eq!(preserved.expires_at, Some(1_900_000_000));

        let clear =
            serde_json::from_value(json!({"refresh_token":null,"expires_at":null})).unwrap();
        store.patch_account(&account.id, clear).await.unwrap();
        drop(store);
        let reopened = Store::open(path).await.unwrap();
        let cleared = reopened.account(&account.id).await.unwrap();
        assert_eq!(cleared.refresh_token, None);
        assert_eq!(cleared.expires_at, None);
    }

    #[tokio::test]
    async fn failed_write_keeps_previous_state_and_selection_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("not-a-directory");
        fs::write(&file_path, "occupied").unwrap();
        let store = Store::open(dir.path().join("accounts.json")).await.unwrap();
        store.save_account(sample_account("a")).await.unwrap();
        store.save_account(sample_account("b")).await.unwrap();
        store.save_account(sample_account("c")).await.unwrap();
        let first = store
            .select_account(Provider::Openai, "gpt-test", &[])
            .await
            .unwrap();
        let second = store
            .select_account(
                Provider::Openai,
                "gpt-test",
                std::slice::from_ref(&first.id),
            )
            .await
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(second.id, "b");
        let third = store
            .select_account(Provider::Openai, "gpt-test", &[])
            .await
            .unwrap();
        assert_eq!(third.id, "c");
        let fourth = store
            .select_account(Provider::Openai, "gpt-test", &[])
            .await
            .unwrap();
        assert_eq!(fourth.id, "a");

        let preserved = store.raw_accounts().await;
        let bad_store = Store {
            path: file_path.join("child.json"),
            state: Mutex::new(RuntimeState {
                data: DiskState {
                    accounts: preserved.clone(),
                    usage: vec![],
                },
                ..RuntimeState::default()
            }),
        };
        assert!(bad_store.save_account(sample_account("c")).await.is_err());
        assert_eq!(bad_store.raw_accounts().await.len(), preserved.len());
    }

    #[tokio::test]
    async fn refresh_commit_preserves_edits_and_wins_against_stale_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("accounts.json")).await.unwrap();
        let original = sample_account("one");
        store.save_account(original.clone()).await.unwrap();

        let mut edited = original.clone();
        edited.label = "renamed".to_owned();
        store.save_account(edited).await.unwrap();

        let mut refresh = original.clone();
        refresh.secret = "rotated-1".to_owned();
        refresh.refresh_token = Some("refresh-rotated-1".to_owned());
        let committed = store.commit_refresh(&original, refresh).await.unwrap();
        assert_eq!(committed.label, "renamed");
        assert_eq!(committed.secret, "rotated-1");

        let mut stale_refresh = original.clone();
        stale_refresh.secret = "rotated-2".to_owned();
        let latest = store
            .commit_refresh(&original, stale_refresh)
            .await
            .unwrap();
        assert_eq!(latest.secret, "rotated-1");

        store.delete_account(&original.id).await.unwrap();
        assert_eq!(
            store
                .commit_refresh(&original, original.clone())
                .await
                .unwrap_err()
                .status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn management_routes_create_account_without_secret_in_response() {
        let dir = tempfile::tempdir().unwrap();
        let app = routes().with_state(AppState {
            store: Store::open(dir.path().join("accounts.json")).await.unwrap(),
            client: reqwest::Client::new(),
            api_key: None,
            management_token: "token".to_owned(),
            demo: false,
        });
        let response = app.oneshot(Request::post("/api/accounts").header("content-type", "application/json")
            .body(Body::from(r#"{"label":"primary","provider":"openai","secret":"top-secret","models":["gpt-test"]}"#)).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["provider"], "openai");
        assert_eq!(value["has_secret"], true);
        assert!(value.get("secret").is_none());
    }
}
