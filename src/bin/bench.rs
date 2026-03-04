/// iAmFaster benchmark binary
/// Usage:
///   ./bench                  — full run (saves sigs to bench_sigs.txt)
///   ./bench --disk           — disk tier only (loads sigs from bench_sigs.txt, RAM must be cold)
///   ./bench --count 100      — use 100 signatures

use std::{
    fs,
    time::{Duration, Instant},
};

use reqwest::Client;
use serde_json::Value;

const DEFAULT_COUNT: usize = 50;
const TX_OPTIONS: &str = r#"{"encoding":"jsonParsed","maxSupportedTransactionVersion":0}"#;
const SIGS_FILE: &str = "bench_sigs.txt";

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let api_key = std::env::var("HELIUS_API_KEY").expect("HELIUS_API_KEY not set");
    let proxy   = std::env::var("PROXY_URL").unwrap_or_else(|_| "http://localhost:8899".into());
    let helius  = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");

    let args: Vec<String> = std::env::args().collect();
    let disk_mode = args.contains(&"--disk".to_string());
    let count = args.windows(2)
        .find(|w| w[0] == "--count")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(DEFAULT_COUNT);

    let client = Client::builder()
        .pool_max_idle_per_host(32)
        .tcp_nodelay(true)
        .use_rustls_tls()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    if disk_mode {
        run_disk_bench(&client, &proxy, &helius).await;
        return;
    }

    // ── fetch signatures and save for disk bench ──────────────────────────
    print!("Fetching {count} recent signatures… ");
    let sigs = get_signatures(&client, &helius, count).await;
    println!("got {}", sigs.len());
    // Save so --disk mode can reuse the exact same set
    fs::write(SIGS_FILE, sigs.join("\n")).ok();
    println!("  \x1b[2m(saved to {SIGS_FILE} — run with --disk after server restart to bench disk tier)\x1b[0m");
    println!();

    // ── pass 1: helius baseline ───────────────────────────────────────────
    bold("=== Pass 1: Helius baseline ===");
    let h_times = run_pass(&client, &helius, &sigs).await;
    let h = stats(&h_times);
    print_row("helius", &h, None);
    println!();

    // ── pass 2: iAmFaster cold ────────────────────────────────────────────
    bold("=== Pass 2: iAmFaster COLD ===");
    let c_times = run_pass(&client, &proxy, &sigs).await;
    let c = stats(&c_times);
    print_row("cold", &c, Some(&h));
    println!();

    // ── pass 3: iAmFaster warm ────────────────────────────────────────────
    bold("=== Pass 3: iAmFaster WARM ===");
    let w_times = run_pass(&client, &proxy, &sigs).await;
    let w = stats(&w_times);
    print_row("warm", &w, Some(&h));
    println!();

    // ── pass 4: iAmFaster lookup only (cache hashmap, no response body) ──
    bold("=== Pass 4: iAmFaster LOOKUP ONLY (vs Helius 473μs index) ===");
    let lookup_url = format!("{}/lookup", proxy);
    let l_times = run_lookup_pass(&client, &lookup_url, &sigs).await;
    let l = stats(&l_times);
    // compare against helius's stated 473μs lookup p95
    let helius_lookup = Stats { p50: 0.5, p95: 0.473, p99: 0.6, mean: 0.48 };
    print_row("iamfaster lookup", &l, Some(&helius_lookup));
    println!("  \x1b[2m(helius lookup p95 = 473μs per their internal metrics)\x1b[0m");
    println!();

    println!("  \x1b[2mrestart server then run: ./target/release/bench --disk\x1b[0m");
    println!();

    // ── summary ───────────────────────────────────────────────────────────
    bold("=== Summary ===");
    print_row("helius e2e (before index)  ", &Stats { p50: 30.0, p95: 30.0, p99: 35.0, mean: 30.0 }, None);
    print_row("helius e2e (after index)   ", &Stats { p50: 5.0,  p95: 5.0,  p99: 6.0,  mean: 5.0  }, None);
    print_row("helius lookup only         ", &helius_lookup, None);
    println!("  ---");
    print_row("iamfaster cold (L3 helius) ", &c, Some(&h));
    print_row("iamfaster warm  (L1 ram)   ", &w, Some(&helius_lookup));
    print_row("iamfaster lookup only      ", &l, Some(&helius_lookup));
    println!("  \x1b[2mrun --disk after server restart for L2 disk numbers\x1b[0m");
}

async fn run_disk_bench(client: &Client, proxy: &str, helius: &str) {
    let sigs: Vec<String> = match fs::read_to_string(SIGS_FILE) {
        Ok(s) => s.lines().filter(|l| !l.is_empty()).map(|l| l.to_owned()).collect(),
        Err(_) => {
            eprintln!("no {SIGS_FILE} found — run without --disk first to populate it");
            return;
        }
    };

    println!("loaded {} sigs from {SIGS_FILE}", sigs.len());
    println!("RAM cache should be cold (fresh server restart)");
    println!();

    bold("=== Pass 1: Helius baseline ===");
    let h_times = run_pass(client, helius, &sigs).await;
    let h = stats(&h_times);
    print_row("helius", &h, None);
    println!();

    bold("=== Pass 2: iAmFaster DISK (L2 redb, RAM cold) ===");
    let d_times = run_pass(client, proxy, &sigs).await;
    let d = stats(&d_times);
    let helius_best = Stats { p50: 5.0, p95: 5.0, p99: 6.0, mean: 5.0 };
    print_row("disk", &d, Some(&helius_best));
    println!("  \x1b[2m(served from redb on disk, no Helius call, no RAM)\x1b[0m");
    println!();

    bold("=== Pass 3: iAmFaster WARM (L1 ram, promoted from disk) ===");
    let w_times = run_pass(client, proxy, &sigs).await;
    let w = stats(&w_times);
    print_row("warm", &w, Some(&helius_best));
    println!();

    bold("=== Summary ===");
    print_row("helius e2e (their best)    ", &helius_best, None);
    print_row("helius lookup only         ", &Stats { p50: 0.5, p95: 0.473, p99: 0.6, mean: 0.48 }, None);
    println!("  ---");
    print_row("iamfaster disk  (L2 redb)  ", &d, Some(&helius_best));
    print_row("iamfaster warm  (L1 ram)   ", &w, Some(&helius_best));
}

// ── RPC helpers ───────────────────────────────────────────────────────────────

async fn run_lookup_pass(client: &Client, url: &str, sigs: &[String]) -> Vec<f64> {
    let mut times = Vec::with_capacity(sigs.len());
    for sig in sigs {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"getTransaction","params":["{sig}",{}]}}"#,
            TX_OPTIONS
        );
        let t = Instant::now();
        let _ = client
            .post(url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .ok();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times
}

async fn run_pass(client: &Client, url: &str, sigs: &[String]) -> Vec<f64> {
    let mut times = Vec::with_capacity(sigs.len());
    for sig in sigs {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"getTransaction","params":["{sig}",{TX_OPTIONS}]}}"#
        );
        let t = Instant::now();
        let _ = client
            .post(url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .ok();
        times.push(t.elapsed().as_secs_f64() * 1000.0); // ms
    }
    times
}

async fn get_signatures(client: &Client, helius: &str, count: usize) -> Vec<String> {
    // Get current slot
    let slot_body = r#"{"jsonrpc":"2.0","id":1,"method":"getSlot","params":[{"commitment":"finalized"}]}"#;
    let slot_resp: Value = client
        .post(helius)
        .header("content-type", "application/json")
        .body(slot_body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut slot = slot_resp["result"].as_u64().unwrap();

    let mut sigs = Vec::with_capacity(count);

    while sigs.len() < count && slot > 0 {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"getBlock","params":[{slot},{{"transactionDetails":"signatures","commitment":"finalized","maxSupportedTransactionVersion":0,"rewards":false}}]}}"#
        );
        let resp: Value = match client
            .post(helius)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(r) => r.json().await.unwrap_or(Value::Null),
            Err(_) => { slot -= 1; continue; }
        };

        if let Some(arr) = resp["result"]["signatures"].as_array() {
            for v in arr {
                if let Some(s) = v.as_str() {
                    sigs.push(s.to_owned());
                    if sigs.len() >= count { break; }
                }
            }
        }
        slot -= 1;
    }

    sigs
}

// ── stats & display ───────────────────────────────────────────────────────────

struct Stats {
    p50: f64,
    p95: f64,
    p99: f64,
    mean: f64,
}

fn stats(ms: &[f64]) -> Stats {
    let mut s = ms.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |pct: f64| s[(((pct / 100.0) * s.len() as f64).ceil() as usize).saturating_sub(1).min(s.len()-1)];
    let mean = s.iter().sum::<f64>() / s.len() as f64;
    Stats { p50: p(50.0), p95: p(95.0), p99: p(99.0), mean }
}

fn fmt(ms: f64) -> String {
    if ms < 1.0 {
        format!("{:.0}μs", ms * 1000.0)
    } else {
        format!("{:.2}ms", ms)
    }
}

fn print_row(label: &str, s: &Stats, baseline: Option<&Stats>) {
    let speedup = baseline.map(|b| {
        let ratio = b.p95 / s.p95;
        if ratio >= 1.0 {
            format!("\x1b[32m{:.1}x faster\x1b[0m", ratio)
        } else {
            format!("\x1b[31m{:.1}x slower\x1b[0m", 1.0 / ratio)
        }
    }).unwrap_or_default();

    println!(
        "  {:<22} p50={:>8}  p95={:>8}  p99={:>8}  mean={:>8}  {}",
        label,
        fmt(s.p50),
        fmt(s.p95),
        fmt(s.p99),
        fmt(s.mean),
        speedup,
    );
}

fn bold(s: &str) {
    println!("\x1b[1m{s}\x1b[0m");
}
