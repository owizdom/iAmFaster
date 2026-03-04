/// L2 disk store — redb B-tree, pre-serialized bytes.
///
/// Layout:
///   key   = Solana signature string  (88 bytes, &str)
///   value = complete JSON-RPC response bytes  (&[u8])
///
/// Hot entries live in the OS page cache → reads are effectively free.
/// Cold NVMe random reads: ~100-300μs → still beats Helius 5ms e2e.
///
/// Writes are always issued from a background task so the hot path is
/// never blocked.
use std::{path::Path, sync::Arc};

use bytes::Bytes;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use tokio::task;
use tracing::debug;

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("txs");

#[derive(Clone)]
pub struct DiskStore {
    db: Arc<Database>,
}

impl DiskStore {
    /// Open (or create) the redb database at `path`.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let db = Database::create(path)?;
        // Ensure the table exists.
        let tx = db.begin_write()?;
        { let _ = tx.open_table(TABLE)?; }
        tx.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Synchronous point read — called via `spawn_blocking` from async code.
    /// Returns `None` on miss, `Some(Bytes)` on hit.
    pub fn get_sync(&self, sig: &str) -> Option<Bytes> {
        let rtx = self.db.begin_read().ok()?;
        let table = rtx.open_table(TABLE).ok()?;
        let guard = table.get(sig).ok()??;
        let bytes = Bytes::copy_from_slice(guard.value());
        debug!("disk hit: {}", sig);
        Some(bytes)
    }

    /// Async get — offloads to blocking thread pool so tokio tasks aren't stalled.
    pub async fn get(&self, sig: String) -> Option<Bytes> {
        let store = self.clone();
        task::spawn_blocking(move || store.get_sync(&sig))
            .await
            .ok()
            .flatten()
    }

    /// Async write — fire-and-forget, never awaited on the hot path.
    pub fn write_background(&self, sig: String, bytes: Bytes) {
        let store = self.clone();
        task::spawn_blocking(move || {
            if let Err(e) = store.write_sync(&sig, &bytes) {
                debug!("disk write error for {sig}: {e}");
            }
        });
    }

    fn write_sync(&self, sig: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(TABLE)?;
            // Only write if not already present (avoid write amplification).
            if table.get(sig)?.is_none() {
                table.insert(sig, bytes)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn len(&self) -> u64 {
        let Ok(rtx) = self.db.begin_read() else { return 0 };
        let Ok(table) = rtx.open_table(TABLE) else { return 0 };
        table.len().unwrap_or(0)
    }
}
