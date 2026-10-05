use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use cliproxy_rs::{
    app,
    core::{AppState, Store},
};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn state() -> (AppState, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("state.json"))
        .await
        .unwrap();
    (
        AppState {
            store,
            client: reqwest::Client::new(),
            api_key: None,
            management_token: "management-test-token".into(),
            demo: false,
        },
        directory,
    )
}

async fn post(state: &AppState, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", "Bearer management-test-token")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn imports_multiple_codex_accounts_without_exposing_secrets() {
    let (state, _directory) = state().await;
    let one = json!({"credentials": {"type":"codex", "access_token":"first-secret", "refresh_token":"first-refresh", "account_id":"acct-1", "expired":"2030-01-01T00:00:00Z"}});
    let two = json!({"credentials": {"type":"codex", "token":{"access_token":"second-secret", "refresh_token":"second-refresh", "account_id":"acct-2"}}});
    let (status_one, view_one) = post(&state, "/api/accounts/import", one).await;
    let (status_two, view_two) = post(&state, "/api/accounts/import", two).await;
    assert_eq!(status_one, StatusCode::CREATED);
    assert_eq!(status_two, StatusCode::CREATED);
    assert_ne!(view_one["id"], view_two["id"]);
    assert!(!view_one.to_string().contains("first-secret"));
    assert!(!view_two.to_string().contains("second-refresh"));
    let accounts = state.store.raw_accounts().await;
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0].chatgpt_account_id.as_deref(), Some("acct-1"));
    assert_eq!(accounts[1].chatgpt_account_id.as_deref(), Some("acct-2"));
}

#[tokio::test]
async fn import_rejects_invalid_credentials_without_echoing_them() {
    let (state, _directory) = state().await;
    let (status, response) = post(
        &state,
        "/api/accounts/import",
        json!({"provider":"codex", "credentials":{"access_token":"bad\nsecret"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!response.to_string().contains("bad\nsecret"));
    assert!(state.store.raw_accounts().await.is_empty());
}

#[tokio::test]
async fn oauth_start_uses_registered_redirect_and_one_time_state() {
    let (state, _directory) = state().await;
    let (status, started) = post(
        &state,
        "/api/oauth/start",
        json!({"provider":"codex", "label":"Personal"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        started["redirect_uri"],
        "http://localhost:1455/auth/callback"
    );
    let url = url::Url::parse(started["authorization_url"].as_str().unwrap()).unwrap();
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .unwrap()
            .1,
        "http://localhost:1455/auth/callback"
    );
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "code_challenge_method")
            .unwrap()
            .1,
        "S256"
    );
    let state_value = started["state"].as_str().unwrap();
    let bad_callback =
        format!("http://localhost:9999/auth/callback?code=private-code&state={state_value}");
    let (bad_status, bad_response) = post(
        &state,
        "/api/oauth/complete",
        json!({"state":state_value, "callback_url":bad_callback}),
    )
    .await;
    assert_eq!(bad_status, StatusCode::BAD_REQUEST);
    assert!(!bad_response.to_string().contains("private-code"));
    let (replay_status, _) = post(
        &state,
        "/api/oauth/complete",
        json!({"state":state_value, "callback_url":bad_callback}),
    )
    .await;
    assert_eq!(replay_status, StatusCode::BAD_REQUEST);
    assert!(state.store.raw_accounts().await.is_empty());
}
