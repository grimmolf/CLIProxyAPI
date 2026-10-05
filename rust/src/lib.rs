pub mod core;
pub mod oauth;
pub mod protocol;
pub mod proxy;
pub mod web;

use axum::{
    Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::Response,
};
use core::{ApiError, AppState};

pub fn app(state: AppState) -> Router {
    let management =
        core::routes()
            .merge(oauth::routes())
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                management_auth,
            ));
    let inference = proxy::routes().route_layer(middleware::from_fn_with_state(
        state.clone(),
        inference_auth,
    ));
    web::routes()
        .merge(management)
        .merge(inference)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .with_state(state)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

async fn management_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if bearer(request.headers()) != Some(state.management_token.as_str()) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "Enter the management token to connect.",
        ));
    }
    Ok(next.run(request).await)
}

async fn inference_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.demo {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Demo mode does not send requests to providers. Restart without --demo to use inference.",
        ));
    }
    if let Some(expected) = &state.api_key {
        let supplied = bearer(request.headers())
            .or_else(|| {
                request
                    .headers()
                    .get("x-api-key")
                    .and_then(|h| h.to_str().ok())
            })
            .or_else(|| {
                request
                    .headers()
                    .get("x-goog-api-key")
                    .and_then(|h| h.to_str().ok())
            });
        let query_key = request.uri().query().and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find(|(name, _)| name == "key")
                .map(|(_, value)| value.into_owned())
        });
        if supplied != Some(expected.as_str()) && query_key.as_deref() != Some(expected.as_str()) {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "A valid inference API key is required.",
            ));
        }
    }
    Ok(next.run(request).await)
}
