# CLIProxy Rust

An experimental Rust rewrite of CLIProxyAPI with an embedded account and usage
dashboard. The executable runs without Go, Node, or a separate frontend server.
The original Go application remains in this fork as the behavioral reference.

## Run

```sh
cargo run -p cliproxy-rs -- --demo
```

Open `http://127.0.0.1:8318` and connect with management token `demo`. Demo data is
clearly labeled and isolated in `.cliproxy-demo/`. Demo inference is disabled.

To use real accounts:

```sh
cargo build --release --locked -p cliproxy-rs
CLIPROXY_API_KEY=your-local-client-key ./target/release/cliproxy-rs
cat .cliproxy-rs/management-token
```

Paste the management token into the dashboard. Add separate account records for
each API key or login, even when the provider is the same. Credentials remain
write-only in dashboard responses. State is stored in `.cliproxy-rs/state.json`
with owner-only permissions on Unix. Back up this directory as sensitive data.

Management and inference use separate keys. `CLIPROXY_MANAGEMENT_TOKEN` overrides
the generated management token. `CLIPROXY_API_KEY` authenticates inference
requests. A loopback server may run without an inference key; an external listener
requires one. Use a TLS reverse proxy for remote access.

```sh
curl http://127.0.0.1:8318/v1/chat/completions \
  -H "Authorization: Bearer your-local-client-key" \
  -H "Content-Type: application/json" \
  -d '{"model":"openai/gpt-4.1","messages":[{"role":"user","content":"Hello"}]}'
```

Explicit `provider/model` routes choose that provider. Account model allowlists
control which credentials are eligible. Each account has a stable identity;
adding another login does not overwrite the previous one.

The proxy supports native OpenAI chat/Responses, Claude Messages, and Gemini
content routes, including native SSE. It can convert nonstreaming text and
existing function-call/result messages between these formats when the fields are
supported. Cross-format tool definitions, images, and thinking metadata are
rejected; conversion to Claude requires an explicit token limit. Unsupported
fields and content blocks return an error. Cross-format
SSE conversion and Responses WebSockets are not implemented. Codex requests are
sent upstream as Responses SSE; nonstreaming clients receive its terminal JSON
result. OAuth login is limited to Codex and Claude;
the token and callback flow has mock coverage, while live provider inference is
still unverified.

## Local options

| Flag | Environment | Default |
| --- | --- | --- |
| `--listen` | `CLIPROXY_LISTEN` | `127.0.0.1:8318` |
| `--data-dir` | `CLIPROXY_DATA_DIR` | `.cliproxy-rs` (`.cliproxy-demo` in demo) |
| `--api-key` | `CLIPROXY_API_KEY` | None on loopback |
| `--management-token` | `CLIPROXY_MANAGEMENT_TOKEN` | Generated file, or `demo` in demo |
| `--demo` | — | Disabled |

Provider API keys, OAuth tokens, token imports and account routing are configured
through the dashboard. This experimental server does not parse the Go YAML
configuration. Check the [parity matrix](../docs/rust-rewrite-parity.md) before
replacing an existing installation. OAuth requires interaction with the provider;
mock checks do not establish real account eligibility or provider compatibility.

## Build and checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked -p cliproxy-rs
```

The Rust workflow runs these checks. Integration tests use local mock upstreams
and deterministic synchronization rather than live paid-provider requests.

The dashboard shows observed request counts, failures, token counts, latency and
account attribution. Missing token metadata stays unknown. These metrics do not
represent provider billing, subscription quota, reset time, or pricing estimates.
Rows represent individual upstream account attempts. A client request that
fails over can therefore add multiple rows; a completed stream adds its row
when the stream ends. The store retains the newest 10,000 rows in local JSON,
which bounds history but is intended for local experimental use rather than
large-scale concurrent reporting.

## Container

Build from the repository root:

```sh
docker build -f rust/Dockerfile -t cliproxy-rs .
docker run --rm -p 127.0.0.1:8318:8318 \
  -e CLIPROXY_API_KEY=your-local-client-key \
  -e CLIPROXY_MANAGEMENT_TOKEN=your-management-key \
  -v cliproxy-data:/home/proxy/data cliproxy-rs
```

The container runs as an unprivileged user. The example binds the published port
to loopback and supplies both required keys.
