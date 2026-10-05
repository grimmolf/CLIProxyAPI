use crate::core::AppState;
use axum::{
    Router,
    http::{HeaderValue, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/styles.css", get(styles))
        .route("/health", get(|| async { axum::Json(serde_json::json!({"status":"ok","runtime":"rust","version":env!("CARGO_PKG_VERSION")})) }))
}

async fn index() -> Response {
    let mut response = Html(include_str!("../web/index.html")).into_response();
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}

async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/styles.css"),
    )
}
