// ── Global allocator: mimalloc (2-3x faster alloc in hot paths) ───────────────
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{routing::{get, post}, Router};
use reqwest::Client;
use socket2::{Domain, Socket, Type};
use tokio::net::TcpListener;
use tracing::info;

mod cache;
mod config;
mod handler;
mod inflight;
mod proxy;
mod store;
mod watcher;

use cache::AppCache;
use config::Config;
use inflight::InflightMap;
use store::DiskStore;

// ── Shared application state ──────────────────────────────────────────────────

pub struct AppState {
    pub cache: AppCache,
    pub disk: DiskStore,
    pub inflight: InflightMap,
    pub http_client: Client,
    pub config: Config,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iamfaster=info".parse().unwrap()),
        )
        .init();

    let cfg = Config::from_env();

    let capacity = auto_cache_capacity(cfg.cache_capacity);
    info!(
        "cache capacity: {} entries (~{}GB at 10KB/tx)",
        capacity,
        capacity * 10 / 1024 / 1024
    );

    let disk = DiskStore::open("txs.redb").expect("failed to open disk store");
    info!("disk store: {} transactions persisted", disk.len());

    let http_client = Client::builder()
        .pool_max_idle_per_host(128)
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true)
        .use_rustls_tls()
        .timeout(Duration::from_millis(4500))
        .build()
        .expect("failed to build reqwest client");

    let state = Arc::new(AppState {
        cache: cache::build_cache(capacity),
        disk,
        inflight: inflight::build_inflight(),
        http_client,
        config: cfg.clone(),
    });

    // Spawn WebSocket prefetch watcher
    {
        let s = state.clone();
        tokio::spawn(async move { watcher::run_watcher(s).await });
    }

    let app = Router::new()
        .route("/", post(handler::handle_rpc))
        .route("/lookup", post(handler::handle_lookup))
        .route("/health", get(health))
        .with_state(state.clone());

    let num_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let addr: SocketAddr = format!("0.0.0.0:{}", cfg.port).parse().unwrap();

    info!("listening on {} ({} SO_REUSEPORT workers)", addr, num_cpus);

    let mut handles = Vec::with_capacity(num_cpus);
    for _ in 0..num_cpus {
        let app2 = app.clone();
        let listener = make_reuseport_listener(addr)
            .expect("failed to create SO_REUSEPORT listener");
        handles.push(tokio::spawn(async move {
            axum::serve(listener, app2).await.unwrap();
        }));
    }

    for h in handles {
        let _ = h.await;
    }
}

fn make_reuseport_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_nodelay(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    let std_listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(std_listener)
}

fn auto_cache_capacity(max: u64) -> u64 {
    let available = available_ram_bytes();
    if available > 0 {
        let entries = (available * 40 / 100) / (10 * 1024);
        entries.min(max).max(10_000)
    } else {
        max
    }
}

#[cfg(target_os = "macos")]
fn available_ram_bytes() -> u64 {
    use std::process::Command;
    Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(not(target_os = "macos"))]
fn available_ram_bytes() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("MemAvailable:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

async fn health() -> &'static str {
    "ok"
}
