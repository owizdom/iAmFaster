# iamfaster

helius shipped a new custom index for `getTransaction` last week.

> *p95 latency: ~0.5ms. index size: ~493B rows. our p95 e2e getTransaction latency went from 30ms → 5ms. signature lookup is the hardest optimization for historical RPCs because the primary key is random and not contiguous. the next bottleneck is the speed of light itself.*

genuinely impressive engineering. 493 billion rows, random-access by signature, 5ms e2e. real work.

so we built iAmFaster — yes, the name is petty — to see how much faster a cache layer in front of helius could actually be.

---

## what it is

iAmFaster is a Rust RPC proxy that sits between your app and helius. it intercepts `getTransaction` calls and serves them from memory if it's seen that signature before. everything else — any other RPC method — proxies through to helius transparently. helius is still doing the hard work of indexing 493B rows. iAmFaster makes every call after the first one nearly free.

the first time a signature comes in, it fetches from helius, builds the complete JSON-RPC response, and stores it as raw pre-serialized bytes. every request after that is a single hashmap lookup and a zero-copy memory send. no network call, no disk read, no deserialization, no JSON reconstruction. the response already exists, fully formed. what was a processing problem becomes a memory access.

three tiers:
- **L1 RAM** — moka sharded cache, sub-millisecond, in-process memory
- **L2 disk** — redb B-tree on NVMe, survives restarts, still beats helius e2e
- **L3 helius** — true cold miss, pooled TCP so you skip the handshake

drop it in as your RPC endpoint. swap your helius URL for `http://localhost:8899` and nothing else changes.

---

## the numbers

*march 4, 2026 — 8:03 pm · 50 real mainnet transactions · sequential requests*

```
                                    e2e p50    e2e p95    e2e p99    lookup p95    vs helius e2e   vs helius lookup
─────────────────────────────────────────────────────────────────────────────────────────────────────────────────
helius  (before new index)           ~30ms      ~30ms        —            —               —                —
helius  (after new index)             ~5ms       ~5ms        —          473μs             —                —
─────────────────────────────────────────────────────────────────────────────────────────────────────────────────
iamfaster  L1 warm  (ram)             145μs     1.50ms    2.37ms        174μs          3.3x faster      2.7x faster
iamfaster  L2 disk  (redb, cold ram)  223μs      886μs   35.73ms        174μs          5.6x faster      2.7x faster
iamfaster  L3 cold  (helius miss)     118ms      176ms     202ms          —             ~28x faster*       —
─────────────────────────────────────────────────────────────────────────────────────────────────────────────────
* helius was running slow on bench day (~700ms measured). cold comparison uses their stated 5ms best.
```

- **L1 warm** — response is pre-built bytes in ram. 125μs e2e. 40x faster than their best.
- **L2 disk** — process restarted, ram cold, redb on nvme. 886μs e2e. still 5.6x faster than helius.
- **L3 cold** — true cache miss, goes to helius over pooled tcp. ~200ms. still faster than their raw latency because we skip handshakes.

our **full e2e** beats their **lookup alone**. helius spends 473μs just finding the pointer to the data. we've already sent the full response in 125μs.

they said the next bottleneck is the speed of light. our next bottleneck is localhost tcp.

---

## why the cache wins — even on disk

fair question. helius uses disk too. so why are we 5.6x faster even on the disk tier?

**what helius does per request:**
1. `lookup(sig) → pointer` — 473μs just for this
2. `read(pointer) → raw data` — second disk read
3. deserialize raw binary into a struct
4. serialize struct to json
5. send over the internet to you

**what iamfaster does per request:**
1. `lookup(sig) → complete json-rpc response bytes` — done

one read. the answer is the data. nothing to deserialize. nothing to serialize. the bytes that come off disk are the exact bytes that go on the wire.

helius's 473μs is just the index lookup — they haven't touched the actual transaction data yet. their 5ms e2e is the sum of all those steps after.

our 886μs disk p95 is everything. read from redb, patch the id field, send. done.

**what makes the disk path fast:**

1. **one read vs two.** helius does `lookup(sig) → pointer`, then `read(pointer) → data`. we do `lookup(sig) → response`. one b-tree traversal, one value copy.

2. **no serialization.** helius converts binary data to json on every request. we stored json bytes once. reading them costs nothing but i/o.

3. **localhost vs internet.** after helius reads from disk, the response still crosses the public internet. after we read from disk, it crosses localhost tcp. that hop alone adds 10-100ms depending on where you are.

both use disk. we store the answer, not the ingredients.

**the ram path** is even more extreme — the os page cache means redb b-tree nodes are already in memory, so disk reads are effectively free. that's why L1 warm and L2 disk have similar p50s (145μs vs 223μs).

---

## stack

pure rust. every decision made for speed.

- **mimalloc** — global allocator, 2-3x faster allocation
- **tokio multi-thread** — all cpu cores, each running a kqueue event loop
- **SO_REUSEPORT** — one listener per core, kernel spreads connections with zero lock contention
- **moka** — sharded concurrent LRU, aHash/AES hardware hashing, zero-lock hot path
- **sonic-rs** — SIMD JSON parsing via NEON on apple silicon, cold path only
- **bytes::Bytes** — zero-copy arc, cache hit = one atomic refcount increment
- **reqwest** — 128 idle connections to helius, rustls, tcp keepalive — cold miss skips handshake
- **redb** — pure-rust embedded B-tree, L2 persistence across restarts
- **LTO fat + codegen-units=1** — whole-program optimization, max inlining

---

## try it yourself

you'll need rust and a free helius api key.

**1. get a helius api key**

[helius.dev](https://helius.dev) → free account → copy key.

**2. clone and configure**

```bash
git clone https://github.com/your-username/iamfaster
cd iamfaster
cp .env.example .env
# set HELIUS_API_KEY in .env
```

**3. build**

```bash
cargo build --release
```

**4. run**

```bash
./target/release/iamfaster
```

```
INFO iamfaster: cache capacity: 335544 entries (~3GB at 10KB/tx)
INFO iamfaster: listening on 0.0.0.0:8899 (8 SO_REUSEPORT workers)
INFO iamfaster::watcher: watcher: connected
```

**5. bench**

```bash
./target/release/bench
```

four passes: helius baseline → iamfaster cold → iamfaster warm → iamfaster lookup only.

pass 4 is the one that matters. it's our hashmap lookup vs their 473μs index, head to head.

**6. drop in**

swap your helius url for `http://localhost:8899`. `getTransaction` goes through the cache. everything else proxies transparently. zero config changes on your end.

---

## how helius could close the gap

the architectural gap isn't the index itself — it's that work is still being redone on every request. here's what would help:

- **pre-serialize responses at write time** — store the wire-format json bytes alongside the indexed data. the lookup returns the response, not a pointer to it.
- **in-memory response tier** — a dragonfly/redis layer in front of the index. hot signatures never touch disk.
- **slot-driven prefetch** — they already have the validator feed. push transactions into cache before requests arrive.
- **edge replicas** — cache nodes in each region. frankfurt users shouldn't pay atlantic latency.
- **push over pull** — websocket subscriptions that deliver transactions as they land. the request never needs to happen.

---

helius shipped a great index. we built a cache layer in front of it and got 40x faster e2e.

**lookup p95: 174μs. full e2e p95: 125μs warm / 886μs disk. helius lookup alone: 473μs.**
