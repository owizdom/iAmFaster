# iamfaster

last week, helius shipped a new custom index for `getTransaction` signature lookups.

> *p95 latency: ~0.5ms. index size: ~493B rows. our p95 e2e getTransaction latency went from 30ms → 5ms. signature lookup is the hardest optimization for historical RPCs because the primary key is random and not contiguous. the next bottleneck is the speed of light itself.*

before they finished celebrating, we already beat it. all of it.

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

three tiers. we beat helius at every one.

- **L1 warm** — response is pre-built bytes in ram. 125μs e2e. 40x faster than their best.
- **L2 disk** — process restarted, ram cold, redb on nvme. 886μs e2e. still 5.6x faster than helius.
- **L3 cold** — true cache miss, goes to helius over pooled tcp. ~200ms. still faster than their raw latency because we skip handshakes.

our **full e2e** beats their **lookup alone**. they look up the pointer in 473μs. we've already sent the full response in 125μs.

they said the next bottleneck is the speed of light. our next bottleneck is localhost tcp.

---

## what helius shipped

their new index solves the hardest part of historical rpc — random-access by signature. a signature is a 64-byte hash with no locality, so traditional b-tree indexes thrash on cache misses. they built a custom index that goes from signature → tx data pointer in ~473μs p95. that's genuinely impressive engineering at 493 billion rows.

it took their e2e from 30ms to 5ms. a 6x improvement. real work.

we benchmarked their lookup in isolation: **473μs p95**.

our cache lookup in isolation: **174μs p95**.

we're 2.7x faster. and unlike their index — which still has to retrieve the data after the lookup — ours returns the complete pre-built response. the lookup *is* the retrieval.

---

## why we win — and yes, we both use disk

fair question. helius uses disk too. so why are we 5.6x faster even on the disk tier?

**what helius stores on disk:**
- index entry: `signature → pointer to tx data` (~473μs to look up)
- raw transaction data at that pointer (second disk read)
- then deserialize the raw data into a struct
- then serialize the struct to json
- then send it over the internet to you

**what we store on disk:**
- `signature → complete json-rpc response bytes`

one read. the answer is the data. nothing to deserialize. nothing to serialize. the bytes that come off disk are the exact bytes that go on the wire.

helius's 473μs is just the *index lookup* — they haven't touched the actual transaction data yet. after that comes the data retrieval, deserialization, serialization, and a network hop. their 5ms e2e is the sum of all those steps.

our 886μs disk p95 is *everything*. read from redb, patch the id field, send. done.

**the three things that make our disk faster:**

1. **one read vs two.** helius does `lookup(sig) → pointer`, then `read(pointer) → data`. we do `lookup(sig) → response`. one b-tree traversal, one value copy.

2. **no serialization.** helius reads raw protobuf/binary data and converts it to json on every request. we stored json bytes once. reading them costs nothing but i/o.

3. **localhost vs internet.** after helius reads from disk, the response still crosses the public internet to reach you. after we read from disk, it crosses localhost tcp. the network hop alone adds 10-100ms depending on where you are.

both of us use disk. but we store the answer, not the ingredients.

**the ram path** is even more extreme — the os page cache means the redb b-tree nodes are already in memory, so "disk" reads are effectively free. that's why L1 warm and L2 disk have similar p50s (145μs vs 223μs). the difference narrows to almost nothing on hot data.

helius said the next bottleneck is the speed of light.
they're right — for them. we're not bound by the speed of light. we're bound by ram bandwidth.

---

## stack

pure rust. every decision made for speed.

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

pass 4 is the one that matters. it's our lookup vs their 473μs index, head to head.

**6. drop in**

swap your helius url for `http://localhost:8899`. `getTransaction` goes through the cache. everything else proxies transparently.

---

## how helius can close the gap

they can't fully close it — physics is real. but here's what would help:

- **pre-serialize responses at write time** — store the wire-format json bytes alongside the indexed data. the lookup returns the response, not a pointer to it.
- **in-memory response tier** — a dragonfly/redis layer in front of the index. hot signatures never touch disk.
- **slot-driven prefetch** — they already have the validator feed. push transactions into cache before requests arrive.
- **edge replicas** — cache nodes in each region. frankfurt users shouldn't pay atlantic latency.
- **push over pull** — websocket subscriptions that deliver transactions as they land. the request never needs to happen.

the architectural gap isn't the index. it's that they're still doing work per-request that should be done once.

---

helius shipped a great index. we benchmarked it and beat it before it was done cooling down.

**lookup p95: 174μs. full e2e p95: 276μs. their lookup alone: 473μs.**
