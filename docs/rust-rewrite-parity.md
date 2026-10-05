# Experimental Rust rewrite: current parity

`cliproxy-rs` is an experimental, independent Rust proxy. The Go server remains the behavioral reference. This inventory describes the checked-in Rust implementation and its current limits; it is not a claim of Go parity or live provider compatibility.

## Implemented surface

| Capability | Rust status | Go reference pointers |
|---|---|---|
| Native inference routes | OpenAI model listing, chat completions, Responses, Claude Messages, Gemini `generateContent` and `streamGenerateContent`; native JSON and SSE forwarding. Codex uses Responses and requests upstream SSE, then buffers its terminal result into a JSON response. | `internal/api/server_routes.go`; `internal/runtime/executor/openai_compat_executor.go`, `codex_executor*.go`, `claude_executor*.go`, `gemini_executor.go` |
| Accounts and routing | Stable independent account records allow multiple same-provider API keys or OAuth accounts. Model allowlists, enabled status, round-robin selection, account-wide cooldown, and bounded failover on connection failures, 401, 429, and 5xx before client output. Streaming is never replayed after output starts. | `sdk/cliproxy/auth/types.go`, `selector.go`, `conductor_selection.go`, `cooldown*.go`; `sdk/api/handlers/handlers_routing.go` |
| Protocol conversion | Nonstreaming text and existing function-call/result messages among OpenAI chat, Responses, Claude, and Gemini formats, within the supported field set. Tool definitions are rejected for cross-format requests; Gemini tool results without a function name are rejected. Generation controls and token-limit finish reasons are translated. Unsupported fields or content are rejected instead of silently dropped. | `internal/translator/`; `internal/thinking/` |
| Management and credentials | Bearer-protected management API and embedded dashboard; account secrets are write-only in management responses. OAuth login uses PKCE and Codex/Claude registered loopback callbacks with manual callback URL fallback, plus token refresh with atomic credential commit. Credential import is available. Live OAuth inference remains unverified. | `sdk/cliproxy/auth/manager.go`, `sdk/auth/codex.go`, `internal/auth/codex/`, `internal/runtime/executor/codex_executor_auth.go` |
| Persistence | Local JSON state with owner-only files on Unix, serialized atomic replacement, and in-memory commit only after persistence succeeds. This is a single local-file store, not a scalable database backend. | `sdk/auth/filestore.go`; optional Go backends in `internal/store/` |
| Usage and dashboard | Observed status, latency, account, provider, model, endpoint, stream flag, and reported input/output/cached/reasoning token counts; filterable usage dashboard. Unknown token counts remain unknown. Retains the newest 10,000 usage rows. | `sdk/cliproxy/usage/manager.go`, `accounting.go` |

Usage rows describe upstream account attempts, not deduplicated logical client requests. A failed attempt followed by failover can create multiple rows for one client request. A successful stream is recorded when it terminates; an interrupted or failed stream is recorded with its outcome. Dashboard request and failure totals aggregate these observed rows. Token counts are provider-reported only: the Rust rewrite does not estimate billing, cost, subscription quota, or quota reset times.

## Explicit omissions and limits

- Cross-format SSE conversion returns HTTP 501. Native-format streaming is supported; Codex itself is requested as upstream SSE even for a nonstreaming client request.
- Cross-format requests reject tool definitions, nonempty text annotations, images, and thinking/signature metadata. Conversion to Claude requires an explicit token limit (`max_tokens`, `max_output_tokens`, or Gemini `maxOutputTokens`). Native requests preserve provider-specific fields.
- Responses WebSockets and connection-pinned auth/session replay are not implemented.
- The Go canonical thinking configuration and per-provider thinking translation pipeline are not implemented.
- Cooldown is account-wide. Model-level quota state, session affinity, and Go's advanced selector strategies are absent.
- Advanced Go provider integrations and compatibility behaviors are not claimed. The listed native routes and limited nonstream conversions define the supported surface.
- Go YAML configuration, cloud/Postgres/Git/object storage, hot reload, plugins, CLIProxyAPIHome, SDK embedding, and TUI are absent. Rust settings and accounts are configured locally through its dashboard and command-line options.
- OAuth token exchanges, refresh, and callback handling have local mock coverage, but live Codex or Claude login and inference compatibility have not been established.
- JSON persistence and dashboard aggregation are intended for local experimental use. The 10,000-row retention bound limits history but does not give JSON the concurrency or query scalability of a database.

## Go source map

- Route registration and middleware: `internal/api/server_routes.go`, `internal/api/server.go`.
- Request execution and routing: `sdk/api/handlers/handlers_execution.go`, `handlers_routing.go`, `handlers_model_router.go`; `sdk/cliproxy/auth/conductor_selection.go`.
- Credentials and storage: `sdk/cliproxy/auth/types.go`, `manager.go`, `sdk/auth/filestore.go`, `internal/store/`, `internal/watcher/clients.go`, `internal/watcher/synthesizer/`.
- Selectors and cooldowns: `sdk/cliproxy/auth/selector.go`, `cooldown_state.go`, `cooldown_view.go`, `scheduler.go`.
- Provider executors: `internal/runtime/executor/openai_compat_executor.go`, `codex_executor*.go`, `claude_executor*.go`, `gemini_executor.go`.
- Translation, thinking, and usage: `internal/translator/`, `internal/thinking/`, `sdk/cliproxy/usage/`.
