# Rust rewrite implementation contract

The experimental executable is `cliproxy-rs`, compiled from the `rust/` crate.
The Go application remains a reference. No dependency on the Go process is allowed.
The public fork is based on public upstream history; private origin history is never pushed.

## Shared types (owned by core worker)

`core::Provider`: serde snake_case enum `Openai`, `Claude`, `Gemini`, `Codex`, `OpenaiCompatible`.
Provide `as_str() -> &'static str` and `default_base_url() -> &'static str`.

`core::Account`: public fields `id: String`, `label: String`, `provider: Provider`,
`base_url: String`, `secret: String`, `refresh_token: Option<String>`,
`expires_at: Option<i64>` (Unix seconds), `enabled: bool`, `models: Vec<String>`,
`created_at: String`, `auth_kind: String` (`api_key` or `oauth`). Derive Clone, Serialize,
Deserialize; use serde defaults for optional fields. Separate `AccountView` has all
nonsecret fields plus `has_secret: bool`, `status: String`, `cooldown_until: Option<i64>`.
Never return secret or refresh_token in management JSON.
Raw Account also carries `chatgpt_account_id: Option<String>` with serde default,
kept out of AccountView, for Codex upstream account headers.

`core::UsageRecord`: public fields `id: String`, `timestamp: String`, `account_id: String`,
`account_label: String`, `provider: Provider`, `model: String`, `endpoint: String`,
`status: u16`, `duration_ms: u64`, `input_tokens: Option<u64>`,
`output_tokens: Option<u64>`, `cached_tokens: Option<u64>`,
`reasoning_tokens: Option<u64>`, `stream: bool`, `error: Option<String>`.
Do not infer token counts or fabricate quota or cost data.

`core::Store::open(path: PathBuf) -> Result<Arc<Store>, ApiError>` async.
Async methods: `accounts() -> Vec<AccountView>`, `raw_accounts() -> Vec<Account>`,
`account(id: &str) -> Option<Account>`, `save_account(account: Account) -> Result<AccountView, ApiError>`,
`delete_account(id: &str) -> Result<(), ApiError>`,
`select_account(provider: Provider, model: &str, excluded: &[String]) -> Option<Account>`,
`mark_result(id: &str, status: u16, retry_after: Option<u64>)`,
`record_usage(record: UsageRecord) -> Result<(), ApiError>`,
`usage() -> Vec<UsageRecord>`, `summary() -> serde_json::Value`.
`commit_refresh(expected: &Account, refreshed: Account) -> Result<Account, ApiError>`
atomically updates credential fields while preserving newer noncredential edits;
deleted accounts cannot be recreated and independently changed credentials win.
Selection is atomic round-robin among enabled compatible accounts, ignores cooling
accounts, and tracks attempts per request. Empty model lists mean any model.
Persistence serializes mutations, writes owner-only temp file, syncs then atomically renames,
and commits memory only when the disk operation succeeds. Usage retention is bounded
and documented. Secrets are stored locally, never checked in.

`core::ApiError`: `new(status: axum::http::StatusCode, message: impl Into<String>)`,
IntoResponse yields `{"error":{"message":...,"type":"proxy_error"}}`.
`core::AppState`: Clone, public fields `store: Arc<Store>`, `client: reqwest::Client`,
`api_key: Option<String>`, `management_token: String`, `demo: bool`.
`core::routes() -> axum::Router<AppState>` management account + usage routes.

## Route and UI contract

Management routes live under `/api/`, protected centrally by main wiring using a
Bearer management token. Frontend stores token in sessionStorage only, prompts to
connect and displays API errors. Static `/`, `/app.js`, `/styles.css` are public.
`GET /api/accounts` => array of AccountView.
`POST /api/accounts` => AccountView (201), body accepts label/provider/base_url/secret/models/enabled/auth_kind,
optional refresh_token/expires_at. Server creates stable UUID, never overwrites another account by provider.
`PATCH /api/accounts/{id}` => AccountView; absent secret preserves prior secret.
`DELETE /api/accounts/{id}` => 204.
`GET /api/usage` => array of UsageRecord, newest first.
`GET /api/summary` => object: `requests`, `successful_requests`, `failed_requests`,
`input_tokens`, `output_tokens`, `cached_tokens`, `reasoning_tokens`, `average_latency_ms`,
`accounts`, `enabled_accounts`, `providers` array with per-provider stats,
`daily` array (`date`, `requests`, `input_tokens`, `output_tokens`),
`by_account` array (account_id/account_label/provider/requests/failures/input_tokens/output_tokens).
`GET /api/providers` => metadata array (id, name, base_url, endpoints, description).
`GET /api/settings` => redacted runtime info including demo flag and inference key required.

`proxy::routes() -> Router<AppState>`: `/v1/models`, `/v1/chat/completions`,
`/v1/responses`, `/v1/messages`, `/v1beta/models/{*action}`.
Inference API auth centrally wrapped separately, key optional only on loopback.
Explicit provider/model syntax `provider/model` chooses provider and strips prefix upstream.
Otherwise select by configured model membership; infer default provider only for known model prefixes.
Native forwarding preserves unknown fields, no redirects, no read/response timeouts.
Cross-format conversions live in `protocol.rs` (owned by proxy worker) with a
canonical message representation; reject unsupported blocks instead of silently losing data.
Stream bytes incrementally; bounded attempt failover for 401/429/5xx before output,
never replay after output. Usage includes stream completion, errors and per-account attribution.

`oauth::routes() -> Router<AppState>`:
`POST /api/oauth/start` body `{provider, label}` => `{authorization_url, state, redirect_uri}`.
`POST /api/oauth/complete` body `{state, callback_url}` => AccountView.
`POST /api/accounts/import` body `{provider?, label?, credentials: object}` => AccountView.
Import CLIProxyAPI/Codex credential JSON without exporting it.
`oauth::refresh_account(state: &AppState, account: &Account) -> Result<Account, ApiError>`.
OAuth modules store pending sessions internally; localhost callback listener or manual callback
paste must match provider registered redirect URI. State/PKCE, one-time/expiring sessions.
Secrets never occur in a frontend URL. Codex/Claude OAuth are labeled experimental
until live inference is tested. Use existing Go auth source for endpoint constants.

## Ownership and verification

Core worker owns only `rust/src/core.rs`.
Proxy worker owns only `rust/src/proxy.rs`, `rust/src/protocol.rs`, proxy integration tests.
OAuth worker owns only `rust/src/oauth.rs`, OAuth tests.
Frontend worker owns only `rust/web/`.
Explorer owns only `docs/rust-rewrite-parity.md`.
Root owns Cargo files, lib/main/web wiring, CI/docs, integration and publication.
Agents share a checkout, never revert others or commit others' work. Do not spawn more agents.

Done means compiled executable serves dashboard without Go/Node, mock upstream tests
prove multi-account routing/failover/native SSE and real usage, management is authenticated,
frontend account CRUD and filters work, cargo fmt/test/clippy pass, and code is
published to a public fork. Feature exclusions are recorded honestly in the parity document.
