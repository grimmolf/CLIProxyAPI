use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use cliproxy_rs::core::{AppState, Store};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn management_is_authenticated_persistent_and_secret_free() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let store = Store::open(path.clone()).await.unwrap();
    let state = AppState {
        store: store.clone(),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        api_key: Some("inference-only".into()),
        management_token: "management-only".into(),
        demo: false,
    };
    let app = cliproxy_rs::app(state);
    for token in [None, Some("inference-only")] {
        assert_eq!(
            call(&app, "GET", "/api/accounts", token, Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let mut ids = Vec::new();
    for label in ["Personal", "Team"] {
        let (status, view) = call(&app, "POST", "/api/accounts", Some("management-only"), json!({"provider":"openai","label":label,"secret":"sensitive-upstream-secret","refresh_token":"sensitive-refresh-secret","models":["test-model"]})).await;
        assert_eq!(status, StatusCode::CREATED, "{view}");
        assert!(!view.to_string().contains("sensitive"));
        assert!(view.get("secret").is_none());
        assert!(view.get("refresh_token").is_none());
        ids.push(view["id"].as_str().unwrap().to_owned());
    }
    assert_ne!(ids[0], ids[1]);
    let (status, view) = call(
        &app,
        "PATCH",
        &format!("/api/accounts/{}", ids[0]),
        Some("management-only"),
        json!({"label":"Renamed","enabled":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["enabled"], false);
    assert_eq!(
        store.account(&ids[0]).await.unwrap().secret,
        "sensitive-upstream-secret"
    );
    let reopened = Store::open(path).await.unwrap();
    assert_eq!(reopened.accounts().await.len(), 2);
    assert_eq!(reopened.account(&ids[0]).await.unwrap().label, "Renamed");
    assert_eq!(
        call(
            &app,
            "DELETE",
            &format!("/api/accounts/{}", ids[1]),
            Some("management-only"),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(store.accounts().await.len(), 1);
    assert_eq!(
        call(
            &app,
            "GET",
            "/v1/models",
            Some("management-only"),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/v1/models",
            Some("inference-only"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, settings) = call(
        &app,
        "GET",
        "/api/settings",
        Some("management-only"),
        Value::Null,
    )
    .await;
    assert_eq!(settings["inference_key_required"], true);
    assert!(!settings.to_string().contains("management-only"));
}

#[tokio::test]
async fn embedded_assets_health_and_demo_inference_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState {
        store: Store::open(directory.path().join("state.json"))
            .await
            .unwrap(),
        client: reqwest::Client::new(),
        api_key: None,
        management_token: "demo".into(),
        demo: true,
    };
    let app = cliproxy_rs::app(state);
    for (path, expected) in [
        ("/", "text/html"),
        ("/app.js", "text/javascript"),
        ("/styles.css", "text/css"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .contains(expected)
        );
        assert!(
            !to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        call(&app, "GET", "/health", None, Value::Null).await.1["runtime"],
        "rust"
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/v1/chat/completions",
            None,
            json!({"model":"openai/test"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
}
