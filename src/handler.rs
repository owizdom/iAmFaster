use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::Response,
};
use bytes::Bytes;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::debug;

use crate::{
    cache::{patch_id, CachedResponse},
    proxy::fetch_transaction,
    AppState,
};

#[allow(unused_imports)]
use anyhow;

// ── JSON-RPC request shape ────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    pub id: Option<serde_json::Value>,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

// ── Main handler ──────────────────────────────────────────────────────────────

pub async fn handle_rpc(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response {
    // Parse with sonic-rs for SIMD speed on cold path.
    // On cache hit we never reach JSON parsing at all.
    let req: RpcRequest = match sonic_rs::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid JSON");
        }
    };

    match req.method.as_str() {
        "getTransaction" => handle_get_transaction(state, req).await,
        _ => crate::proxy::proxy_request(state, body).await,
    }
}

async fn handle_get_transaction(
    state: Arc<AppState>,
    req: RpcRequest,
) -> Response {
    // Extract signature from params[0]
    let sig = match extract_sig(&req.params) {
        Some(s) => s,
        None => return error_response(StatusCode::BAD_REQUEST, "missing signature param"),
    };

    let id_bytes = id_to_bytes(&req.id);

    // ── 1. L1: RAM cache (hot path) ──────────────────────────────────────────
    if let Some(cached) = state.cache.get(&sig).await {
        debug!("L1 hit: {}", sig);
        return json_response(patch_id(&cached, &id_bytes));
    }

    // ── 2. In-flight dedup ────────────────────────────────────────────────────
    if let Some(sender) = state.inflight.get(&sig) {
        let mut rx = sender.subscribe();
        drop(sender);
        debug!("inflight wait: {}", sig);
        let _ = rx.changed().await;
        let result = rx.borrow().clone();
        return match result {
            Some(_) => {
                if let Some(cached) = state.cache.get(&sig).await {
                    json_response(patch_id(&cached, &id_bytes))
                } else {
                    error_response(StatusCode::BAD_GATEWAY, "upstream error")
                }
            }
            None => error_response(StatusCode::BAD_GATEWAY, "upstream error"),
        };
    }

    // ── 3. L2: disk store ─────────────────────────────────────────────────────
    if let Some(raw_bytes) = state.disk.get(sig.clone()).await {
        debug!("L2 disk hit: {}", sig);
        let cached = build_cached_response(raw_bytes, &id_bytes);
        // Promote to L1 for subsequent requests
        state.cache.insert(sig.clone(), cached.clone()).await;
        return json_response(patch_id(&cached, &id_bytes));
    }

    // ── 4. L3: Helius upstream (true cold miss) ───────────────────────────────
    let (tx, _rx) = watch::channel(None::<Bytes>);
    state.inflight.insert(sig.clone(), tx);

    let result: anyhow::Result<Bytes> = fetch_transaction(
        &state.http_client,
        &state.config.helius_http_url,
        &sig,
        &req.params,
    ).await;

    match result {
        Ok(raw_bytes) => {
            let cached = build_cached_response(raw_bytes.clone(), &id_bytes);
            // Write to L1 RAM
            state.cache.insert(sig.clone(), cached.clone()).await;
            // Write to L2 disk (background, non-blocking)
            state.disk.write_background(sig.clone(), raw_bytes.clone());

            if let Some((_, sender)) = state.inflight.remove(&sig) {
                let _ = sender.send(Some(raw_bytes));
            }

            json_response(patch_id(&cached, &id_bytes))
        }
        Err(e) => {
            debug!("upstream error for {}: {}", sig, e);
            if let Some((_, sender)) = state.inflight.remove(&sig) {
                let _ = sender.send(None);
            }
            error_response(StatusCode::BAD_GATEWAY, "upstream error")
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_sig(params: &Option<serde_json::Value>) -> Option<String> {
    params
        .as_ref()?
        .as_array()?
        .first()?
        .as_str()
        .map(|s| s.to_owned())
}

/// Serialize the request `id` to bytes for splicing into cached response.
fn id_to_bytes(id: &Option<serde_json::Value>) -> Bytes {
    match id {
        None => Bytes::from_static(b"null"),
        Some(v) => Bytes::from(sonic_rs::to_string(v).unwrap_or_else(|_| "null".into())),
    }
}

/// Build a CachedResponse from upstream bytes.
/// We store the response as-is (Helius already returns valid JSON).
/// We record id_offset pointing to the id value inside the bytes so we can
/// patch it cheaply on cache hits.
///
/// The upstream response has `"id":1` or `"id":"..."` matching the request id
/// we sent. We normalise by always sending `"id":1` upstream and recording
/// the offset of `1` (1 byte).
fn build_cached_response(raw: Bytes, _id_bytes: &[u8]) -> CachedResponse {
    // Find `"id":` in the bytes and record the offset of the value that follows
    let needle = b"\"id\":";
    let (id_offset, id_len) = find_id_value(&raw, needle);
    CachedResponse {
        bytes: raw,
        id_offset,
        id_len,
    }
}

/// Scan `buf` for `needle`, then return (offset_of_value, len_of_value).
/// Value ends at the next `,` or `}` at the same depth.
fn find_id_value(buf: &[u8], needle: &[u8]) -> (usize, usize) {
    if let Some(pos) = find_subsequence(buf, needle) {
        let value_start = pos + needle.len();
        // Skip whitespace
        let mut i = value_start;
        while i < buf.len() && buf[i] == b' ' {
            i += 1;
        }
        let start = i;
        // Determine end: scan until `,` or `}` not inside a string/object
        let end = scan_value_end(buf, start);
        (start, end - start)
    } else {
        // Fallback: store at offset 0 with len 0 (patch becomes no-op)
        (0, 0)
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn scan_value_end(buf: &[u8], start: usize) -> usize {
    let mut i = start;
    if i >= buf.len() {
        return i;
    }
    match buf[i] {
        b'"' => {
            // String value
            i += 1;
            while i < buf.len() {
                if buf[i] == b'\\' {
                    i += 2;
                } else if buf[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
        }
        b'{' | b'[' => {
            // Object/array: skip to matching close
            let open = buf[i];
            let close = if open == b'{' { b'}' } else { b']' };
            let mut depth = 1usize;
            i += 1;
            while i < buf.len() && depth > 0 {
                if buf[i] == open {
                    depth += 1;
                } else if buf[i] == close {
                    depth -= 1;
                } else if buf[i] == b'"' {
                    i += 1;
                    while i < buf.len() {
                        if buf[i] == b'\\' {
                            i += 2;
                        } else if buf[i] == b'"' {
                            i += 1;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                    continue;
                }
                i += 1;
            }
        }
        _ => {
            // Number, bool, null — scan until `,` `}` `]` or whitespace
            while i < buf.len()
                && buf[i] != b','
                && buf[i] != b'}'
                && buf[i] != b']'
                && buf[i] != b' '
                && buf[i] != b'\n'
                && buf[i] != b'\r'
            {
                i += 1;
            }
        }
    }
    i
}

fn json_response(body: Bytes) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

fn error_response(status: StatusCode, msg: &'static str) -> Response {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32600,"message":"{}"}}}}"#,
        msg
    );
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

// ── Lookup-only endpoint ──────────────────────────────────────────────────────

/// POST /lookup — only does the cache hashmap check, returns {"hit":true/false}.
/// Used to benchmark pure signature lookup latency (apples-to-apples with
/// Helius's internal 473μs index lookup metric).
pub async fn handle_lookup(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response {
    let sig = match sonic_rs::from_slice::<RpcRequest>(&body)
        .ok()
        .and_then(|r| extract_sig(&r.params))
    {
        Some(s) => s,
        None => return error_response(StatusCode::BAD_REQUEST, "missing signature param"),
    };
    let hit = state.cache.get(&sig).await.is_some();
    let body = if hit {
        Bytes::from_static(b"{\"hit\":true}")
    } else {
        Bytes::from_static(b"{\"hit\":false}")
    };
    json_response(body)
}

// ── Pre-fetch entry point (called from watcher) ───────────────────────────────

/// Store a pre-fetched response into the cache (called from watcher.rs).
/// Only inserts if the key isn't already cached.
pub async fn prefetch_insert(state: &Arc<AppState>, sig: String, raw: Bytes) {
    if state.cache.get(&sig).await.is_none() {
        let cached = build_cached_response(raw.clone(), b"null");
        state.cache.insert(sig.clone(), cached).await;
        // Persist to disk so the cache survives restarts
        state.disk.write_background(sig, raw);
    }
}
