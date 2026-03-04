use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use bytes::Bytes;
use std::sync::Arc;
use tracing::debug;

use crate::AppState;

/// Forward any non-getTransaction RPC call straight to Helius and stream
/// the response back to the caller.
pub async fn proxy_request(state: Arc<AppState>, body: Bytes) -> Response {
    debug!("proxy passthrough");
    match state
        .http_client
        .post(&state.config.helius_http_url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            match resp.bytes().await {
                Ok(b) => Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(b))
                    .unwrap(),
                Err(_) => error_502(),
            }
        }
        Err(_) => error_502(),
    }
}

/// Fetch a single `getTransaction` from Helius and return raw response bytes.
/// We always send `"id":1` upstream; the caller patches the real id on the
/// returned bytes before sending to the client.
pub async fn fetch_transaction(
    client: &reqwest::Client,
    url: &str,
    sig: &str,
    params: &Option<serde_json::Value>,
) -> anyhow::Result<Bytes> {
    // Build the minimal params array: [sig, options]
    let options = params
        .as_ref()
        .and_then(|p| p.as_array())
        .and_then(|a| a.get(1))
        .cloned()
        .unwrap_or_else(|| {
            serde_json::json!({"encoding": "jsonParsed", "maxSupportedTransactionVersion": 0})
        });

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getTransaction",
        "params": [sig, options]
    });

    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?
        .error_for_status()?;

    Ok(resp.bytes().await?)
}

fn error_502() -> Response {
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"upstream unavailable"}}"#,
        ))
        .unwrap()
}
