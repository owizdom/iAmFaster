/// WebSocket slot subscriber + background prefetch loop.
///
/// On each new slot:
///   1. Fetch the block's transaction signatures (one HTTP call).
///   2. Push each signature onto a LIFO channel (bounded).
///   3. Up to PREFETCH_CONCURRENCY tasks race to call `getTransaction`
///      and insert results into the cache.
///
/// LIFO ordering means the most-recent signatures (most likely to be
/// requested next) are fetched first.
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::Semaphore;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::{handler::prefetch_insert, proxy::fetch_transaction, AppState};

const PREFETCH_CONCURRENCY: usize = 32;

pub async fn run_watcher(state: Arc<AppState>) {
    loop {
        if let Err(e) = watcher_inner(state.clone()).await {
            warn!("watcher disconnected: {e}. Reconnecting in 5s…");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn watcher_inner(state: Arc<AppState>) -> anyhow::Result<()> {
    let url = &state.config.helius_ws_url;
    info!("watcher: connecting to {url}");

    let (mut ws, _) = connect_async(url).await?;
    info!("watcher: connected");

    // Subscribe to slot notifications
    ws.send(Message::Text(
        r#"{"jsonrpc":"2.0","id":1,"method":"slotSubscribe"}"#.into(),
    ))
    .await?;

    let sem = Arc::new(Semaphore::new(PREFETCH_CONCURRENCY));

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let text = match msg {
            Message::Text(t) => t,
            Message::Ping(p) => {
                ws.send(Message::Pong(p)).await?;
                continue;
            }
            _ => continue,
        };

        // Parse slot notification
        let slot = match parse_slot_notification(text.as_str()) {
            Some(s) => s,
            None => continue,
        };

        debug!("watcher: new slot {slot}");

        // Fetch signatures for this slot in the background
        let state2 = state.clone();
        let sem2 = sem.clone();
        tokio::spawn(async move {
            if let Err(e) = prefetch_slot(state2, sem2, slot).await {
                debug!("prefetch slot {slot} error: {e}");
            }
        });
    }

    Ok(())
}

async fn prefetch_slot(
    state: Arc<AppState>,
    sem: Arc<Semaphore>,
    slot: u64,
) -> anyhow::Result<()> {
    let sigs: Vec<String> = fetch_block_signatures(&state, slot).await?;
    debug!("watcher: slot {slot} → {} sigs", sigs.len());

    // LIFO: iterate in reverse so newest (last appended) go first.
    // We spawn with semaphore so at most PREFETCH_CONCURRENCY run at once.
    for sig in sigs.into_iter().rev() {
        // Skip if already cached
        if state.cache.get(&sig).await.is_some() {
            continue;
        }

        let permit = sem.clone().acquire_owned().await?;
        let state2 = state.clone();
        tokio::spawn(async move {
            let _permit = permit; // released on drop
            match fetch_transaction(
                &state2.http_client,
                &state2.config.helius_http_url,
                &sig,
                &None,
            )
            .await
            {
                Ok(bytes) => {
                    prefetch_insert(&state2, sig.to_owned(), bytes).await;
                }
                Err(e) => {
                    debug!("prefetch fetch error: {e}");
                }
            }
        });
    }

    Ok(())
}

/// Call `getBlock` with `transactionDetails:"signatures"` to get just the sigs.
async fn fetch_block_signatures(state: &AppState, slot: u64) -> anyhow::Result<Vec<String>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getBlock",
        "params": [slot, {
            "transactionDetails": "signatures",
            "commitment": "finalized",
            "maxSupportedTransactionVersion": 0,
            "rewards": false
        }]
    });

    let resp: serde_json::Value = state
        .http_client
        .post(&state.config.helius_http_url)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let sigs = resp
        .get("result")
        .and_then(|r| r.get("signatures"))
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(sigs)
}

/// Parse a `slotSubscribe` notification and return the slot number.
/// Notification shape: `{"jsonrpc":"2.0","method":"slotNotification","params":{"result":{"slot":N,...},"subscription":M}}`
fn parse_slot_notification(text: &str) -> Option<u64> {
    // Fast path: look for `"slot":` using a simple scan before touching serde
    let slot_key = b"\"slot\":";
    let bytes = text.as_bytes();
    let pos = bytes.windows(slot_key.len()).position(|w| w == slot_key)?;
    let after = &bytes[pos + slot_key.len()..];
    // Read digits
    let digits: Vec<u8> = after
        .iter()
        .copied()
        .take_while(|b| b.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    std::str::from_utf8(&digits).ok()?.parse().ok()
}
