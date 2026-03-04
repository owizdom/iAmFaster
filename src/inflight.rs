use bytes::Bytes;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::watch;

/// In-flight request deduplication.
/// When a cold-miss arrives for signature S, we insert a `watch::Sender`.
/// All subsequent requests for S subscribe and wait for the result.
/// When the upstream fetch completes, we broadcast `Some(bytes)` (or `None`
/// on error) and remove the entry.
pub type InflightMap = Arc<DashMap<String, watch::Sender<Option<Bytes>>>>;

pub fn build_inflight() -> InflightMap {
    Arc::new(DashMap::new())
}
