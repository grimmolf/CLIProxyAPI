use std::{collections::HashMap, sync::Arc};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use cliproxy_rs::core::{Account, AppState, Provider, Store};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tower::ServiceExt;

#[derive(Clone)]
struct Mock(Arc<Mutex<MockData>>);

#[derive(Default)]
struct MockData {
    calls: Vec<(String, HeaderMap, Value)>,
    statuses: HashMap<String, StatusCode>,
    stream: Option<mpsc::Receiver<Bytes>>,
}

async fn upstream(
    State(mock): State<Mock>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let credential = headers
        .get("authorization")
        .or_else(|| headers.get("x-api-key"))
        .or_else(|| headers.get("x-goog-api-key"))
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let mut state = mock.0.lock().await;
    state.calls.push((
        uri.to_string(),
        headers,
        serde_json::from_slice(&body).unwrap(),
    ));
    if let Some(status) = state.statuses.get(&credential) {
        return (*status, "secret raw upstream error").into_response();
    }
    if let Some(mut receiver) = state.stream.take() {
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(async_stream::stream! {
                while let Some(bytes) = receiver.recv().await {
                    yield Ok::<_, std::io::Error>(bytes);
                }
            }))
            .unwrap();
    }
    axum::Json(json!({"id":"native","unknown":{"kept":true},"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":2},"completion_tokens_details":{"reasoning_tokens":1}}})).into_response()
}

async fn setup() -> (AppState, Mock, tempfile::TempDir, String) {
    let directory = tempfile::tempdir().unwrap();
    let mock = Mock(Arc::new(Mutex::new(MockData::default())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/{*path}", post(upstream))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = AppState {
        store: Store::open(directory.path().join("state.json"))
            .await
            .unwrap(),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        api_key: Some("client-key".into()),
        management_token: "management-key".into(),
        demo: false,
    };
    (state, mock, directory, base)
}

async fn account(
    state: &AppState,
    base: &str,
    id: &str,
    provider: Provider,
    models: &[&str],
    enabled: bool,
) {
    state
        .store
        .save_account(Account {
            id: id.into(),
            label: id.into(),
            provider,
            base_url: base.into(),
            secret: id.into(),
            refresh_token: None,
            chatgpt_account_id: None,
            expires_at: None,
            enabled,
            models: models.iter().map(|s| (*s).into()).collect(),
            created_at: "2026-01-01T00:00:00Z".into(),
            auth_kind: "api_key".into(),
        })
        .await
        .unwrap();
}

async fn call(state: &AppState, path: &str, body: Value) -> Response {
    cliproxy_rs::app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", "Bearer client-key")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn json_response(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn round_robin_filters_models_and_attributes_native_usage() {
    let (state, mock, _directory, base) = setup().await;
    account(&state, &base, "one", Provider::Openai, &["gpt-test"], true).await;
    account(&state, &base, "two", Provider::Openai, &["gpt-test"], true).await;
    account(
        &state,
        &base,
        "disabled",
        Provider::Openai,
        &["gpt-test"],
        false,
    )
    .await;
    account(
        &state,
        &base,
        "wrong-model",
        Provider::Openai,
        &["other"],
        true,
    )
    .await;
    for _ in 0..3 {
        let (status, body) = json_response(call(&state, "/v1/chat/completions", json!({"model":"gpt-test","messages":[{"role":"user","content":"hi"}],"unknown_request":{"kept":true}})).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["unknown"]["kept"], true);
    }
    let calls = &mock.0.lock().await.calls;
    assert_eq!(
        calls
            .iter()
            .map(|(_, headers, _)| headers["authorization"].to_str().unwrap())
            .collect::<Vec<_>>(),
        ["Bearer one", "Bearer two", "Bearer one"]
    );
    assert!(
        calls
            .iter()
            .all(|(path, _, body)| path == "/v1/chat/completions"
                && body["unknown_request"]["kept"] == true)
    );
    let usage = state.store.usage().await;
    assert_eq!(usage.len(), 3);
    assert_eq!(
        usage
            .iter()
            .map(|u| u.account_id.as_str())
            .collect::<Vec<_>>(),
        ["one", "two", "one"]
    );
    assert!(usage.iter().all(|u| u.input_tokens == Some(7)
        && u.output_tokens == Some(3)
        && u.cached_tokens == Some(2)
        && u.reasoning_tokens == Some(1)));
}

#[tokio::test]
async fn retryable_failures_fail_over_once_and_hide_upstream_errors() {
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let (state, mock, _directory, base) = setup().await;
        account(&state, &base, "bad", Provider::Openai, &["gpt-test"], true).await;
        account(&state, &base, "good", Provider::Openai, &["gpt-test"], true).await;
        mock.0
            .lock()
            .await
            .statuses
            .insert("Bearer bad".into(), status);
        let (actual, body) = json_response(
            call(
                &state,
                "/v1/chat/completions",
                json!({"model":"gpt-test","messages":[]}),
            )
            .await,
        )
        .await;
        assert_eq!(actual, StatusCode::OK, "{body}");
        assert_eq!(mock.0.lock().await.calls.len(), 2);
        let usage = state.store.usage().await;
        assert_eq!(usage.len(), 2);
        assert_eq!(
            usage.iter().find(|u| u.account_id == "bad").unwrap().status,
            status.as_u16()
        );
        assert!(
            usage
                .iter()
                .any(|u| u.account_id == "good" && u.status == 200)
        );
    }
    let (state, mock, _directory, base) = setup().await;
    account(&state, &base, "bad", Provider::Openai, &["gpt-test"], true).await;
    mock.0
        .lock()
        .await
        .statuses
        .insert("Bearer bad".into(), StatusCode::INTERNAL_SERVER_ERROR);
    let (status, body) = json_response(
        call(
            &state,
            "/v1/chat/completions",
            json!({"model":"gpt-test","messages":[]}),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!body.to_string().contains("secret raw upstream"));
    assert_eq!(mock.0.lock().await.calls.len(), 1);
}

#[tokio::test]
async fn provider_paths_headers_and_native_sse_usage() {
    let (state, mock, _directory, base) = setup().await;
    account(
        &state,
        &base,
        "claude-secret",
        Provider::Claude,
        &["claude-test"],
        true,
    )
    .await;
    account(
        &state,
        &base,
        "gemini-secret",
        Provider::Gemini,
        &["gemini-test"],
        true,
    )
    .await;
    account(
        &state,
        &format!("{base}/backend-api"),
        "codex-secret",
        Provider::Codex,
        &["gpt-codex"],
        true,
    )
    .await;
    let _ = json_response(
        call(
            &state,
            "/v1/messages",
            json!({"model":"claude-test","max_tokens":5,"messages":[],"unknown_request":42}),
        )
        .await,
    )
    .await;
    let _ = json_response(
        call(
            &state,
            "/v1beta/models/gemini-test:generateContent?key=leaked&foo=bar",
            json!({"contents":[],"unknown_request":42}),
        )
        .await,
    )
    .await;
    {
        let calls = &mock.0.lock().await.calls;
        assert_eq!(calls[0].0, "/v1/messages");
        assert_eq!(calls[0].1["x-api-key"], "claude-secret");
        assert_eq!(calls[0].1["anthropic-version"], "2023-06-01");
        assert_eq!(calls[0].2["unknown_request"], 42);
        assert!(
            calls[1]
                .0
                .starts_with("/v1beta/models/gemini-test:generateContent?foo=bar")
        );
        assert_eq!(calls[1].1["x-goog-api-key"], "gemini-secret");
    }
    let (tx, rx) = mpsc::channel(4);
    mock.0.lock().await.stream = Some(rx);
    tx.send(Bytes::from_static(
        b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
    ))
    .await
    .unwrap();
    let response = call(
        &state,
        "/v1/responses",
        json!({"model":"codex/gpt-codex","input":"hello","stream":true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(first.starts_with(b"event: response.created"));
    tx.send(Bytes::from_static(b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":4,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens_details\":{\"reasoning_tokens\":2}}}}\n\n")).await.unwrap();
    let second = tokio::time::timeout(std::time::Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(second.starts_with(b"event: response.completed"));
    let calls = &mock.0.lock().await.calls;
    assert_eq!(calls[2].0, "/backend-api/codex/responses");
    assert_eq!(calls[2].1["authorization"], "Bearer codex-secret");
    assert_eq!(calls[2].2["model"], "gpt-codex");
    let usage = state.store.usage().await;
    let last = usage
        .iter()
        .find(|record| record.account_id == "codex-secret")
        .unwrap();
    assert_eq!(
        (
            last.status,
            last.input_tokens,
            last.output_tokens,
            last.cached_tokens,
            last.reasoning_tokens
        ),
        (200, Some(11), Some(4), Some(3), Some(2))
    );
}

#[tokio::test]
async fn truncated_and_failed_sse_are_recorded_without_raw_errors() {
    for failed in [false, true] {
        let (state, mock, _directory, base) = setup().await;
        account(
            &state,
            &base,
            "secret",
            Provider::Openai,
            &["gpt-test"],
            true,
        )
        .await;
        let (tx, rx) = mpsc::channel(2);
        mock.0.lock().await.stream = Some(rx);
        tx.send(Bytes::from_static(
            b"data: {\"usage\":{\"prompt_tokens\":5},\"choices\":[]}\n\n",
        ))
        .await
        .unwrap();
        let response = call(
            &state,
            "/v1/chat/completions",
            json!({"model":"gpt-test","messages":[],"stream":true}),
        )
        .await;
        let mut body = response.into_body().into_data_stream();
        assert!(body.next().await.unwrap().is_ok());
        if failed {
            tx.send(Bytes::from_static(b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"secret raw upstream error\"}}\n\n")).await.unwrap();
        }
        drop(tx);
        let error = body.next().await.unwrap().unwrap_err().to_string();
        assert!(!error.contains("secret raw upstream"));
        assert!(body.next().await.is_none());
        let record = state.store.usage().await.pop().unwrap();
        assert_eq!(record.status, 502);
        assert_eq!(record.input_tokens, Some(5));
        assert!(!record.error.unwrap().contains("secret raw upstream"));
    }
}

#[tokio::test]
async fn cancelled_stream_keeps_usage_seen_before_disconnect() {
    let (state, mock, _directory, base) = setup().await;
    account(
        &state,
        &base,
        "secret",
        Provider::Openai,
        &["gpt-test"],
        true,
    )
    .await;
    let (tx, rx) = mpsc::channel(2);
    mock.0.lock().await.stream = Some(rx);
    tx.send(Bytes::from_static(
        b"data: {\"usage\":{\"prompt_tokens\":9},\"choices\":[]}\n\n",
    ))
    .await
    .unwrap();
    let response = call(
        &state,
        "/v1/chat/completions",
        json!({"model":"gpt-test","messages":[],"stream":true}),
    )
    .await;
    let mut body = response.into_body().into_data_stream();
    assert!(body.next().await.unwrap().is_ok());
    drop(body);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Some(record) = state.store.usage().await.first() {
                assert_eq!(record.status, 499);
                assert_eq!(record.input_tokens, Some(9));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(tx);
}

#[tokio::test]
async fn codex_incomplete_response_finishes_without_upstream_eof() {
    for streaming in [false, true] {
        let (state, mock, _directory, base) = setup().await;
        account(
            &state,
            &format!("{base}/backend-api"),
            "codex-secret",
            Provider::Codex,
            &["gpt-codex"],
            true,
        )
        .await;
        let (tx, rx) = mpsc::channel(2);
        mock.0.lock().await.stream = Some(rx);
        let event = b"event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"partial\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":6,\"output_tokens\":2}}}\n\n";
        tx.send(Bytes::from_static(event)).await.unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            call(
                &state,
                "/v1/responses",
                json!({"model":"codex/gpt-codex","input":"hello","stream":streaming}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        if streaming {
            let bytes = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                to_bytes(response.into_body(), 1024 * 1024),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(bytes, Bytes::from_static(event));
        } else {
            let (_, body) = json_response(response).await;
            assert_eq!(body["status"], "incomplete");
            assert_eq!(body["output"][0]["content"][0]["text"], "partial");
        }
        let usage = state.store.usage().await;
        assert_eq!(usage.len(), 1);
        assert_eq!(
            (
                usage[0].status,
                usage[0].input_tokens,
                usage[0].output_tokens
            ),
            (200, Some(6), Some(2))
        );
        drop(tx);
    }
}
