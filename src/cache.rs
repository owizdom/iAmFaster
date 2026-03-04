use bytes::Bytes;
use moka::future::Cache;
use std::sync::Arc;

/// A pre-serialized JSON-RPC response envelope stored in cache.
/// `id_offset` and `id_len` point to the `null` literal after `"id":` so we
/// can patch just those bytes on a cache hit without a full re-parse.
#[derive(Clone, Debug)]
pub struct CachedResponse {
    /// Full response bytes: `{"jsonrpc":"2.0","id":null,"result":...}`
    pub bytes: Bytes,
    /// Byte offset of the `null` value after `"id":`
    pub id_offset: usize,
    /// Length of the stored id value (typically 4 for `null`)
    pub id_len: usize,
}

pub type AppCache = Arc<Cache<String, CachedResponse>>;

pub fn build_cache(capacity: u64) -> AppCache {
    Arc::new(
        Cache::builder()
            .max_capacity(capacity)
            .build(),
    )
}

/// Patch the `id` field in a cached response.
/// If the requested id is the same length as the stored id, do an in-place
/// copy. Otherwise fall back to a heap allocation (rare).
pub fn patch_id(cached: &CachedResponse, id_bytes: &[u8]) -> Bytes {
    if id_bytes.len() == cached.id_len {
        let mut buf = cached.bytes.to_vec();
        buf[cached.id_offset..cached.id_offset + cached.id_len]
            .copy_from_slice(id_bytes);
        Bytes::from(buf)
    } else {
        // Different length: splice
        let mut buf = Vec::with_capacity(
            cached.bytes.len() - cached.id_len + id_bytes.len(),
        );
        buf.extend_from_slice(&cached.bytes[..cached.id_offset]);
        buf.extend_from_slice(id_bytes);
        buf.extend_from_slice(&cached.bytes[cached.id_offset + cached.id_len..]);
        Bytes::from(buf)
    }
}
