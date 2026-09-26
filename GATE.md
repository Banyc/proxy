# The proxy opt-in test inventory

This file records every `#[ignore]`d test in the workspace members. `cargo
test` silently skips them all, so each one's opt-in classification is named
here and machine-checked by `python3 tools/check-ignored.py`, which fails if a
test is added, removed, renamed, or reclassified without this manifest being
updated — an unnoticed `#[ignore]` skip is impossible.

Run the checker after touching any `#[ignore]`d test (including its doc and
its reason string):

```sh
python3 tools/check-ignored.py
```

## Classifications

- **perf** — an *asserting* test kept opt-in because it is an end-to-end
  throughput benchmark. `perf_bulk_rtp_mux` pushes 32 MiB through a real
  proxy chain and asserts byte-exact delivery at the receiver
  (`assert_eq!`/`debug_assert_eq!` on the read total), then prints the
  achieved MiB/s. It is a real gate — it fails on a relay corruption or a
  wedged session — but the run is wall-clock-measured and takes seconds, so
  it stays opt-in. Run it with:

  ```sh
  cargo test -p tests --lib perf_bulk_rtp_mux -- --ignored
  ```

  The byte-exactness property it checks is the same class the default-tier
  `*_relays_bytes_byte_exact` tests assert on smaller transfers; the
  benchmark adds the bulk-throughput measurement on top. The checker
  requires its body to keep an assertion token, so the integrity check
  cannot silently evaporate while staying ignored.

  `proxy_path_matched_rtt_delta` is `perf` for the same reason — an end-to-end
  run against the real `proxy` binary, too slow for the default suite — but it
  asserts **only instrument sanity and delivery integrity** (something was
  measured, nothing was left unanswered, every echo matched, every arm dialed,
  the topologies really were compared at the same end-to-end base RTT — the
  protocol-only arm against its regime's calibration — and the direct arm
  really carried bulk traffic). It asserts **no product bound**.
  See "The deployed-path diagnosis" below.
- **unrunnable** — a test whose triggering condition cannot be produced on
  this host. `basics` waits for a real OS suspend/resume notification
  (`spawn_suspend_watcher` with the system clock); that cannot be faked in a
  test process. The decision rule it exercises is pinned by the
  deterministically-runnable clock-seam tests in the same module
  (`a_gap_past_the_tolerance_notifies_the_resume_signal`,
  `the_suspend_threshold_pins_the_coefficient_and_is_strict`). The
  checker requires the `#[ignore]` reason to state the constraint, so it
  stays visible. It cannot be run on this host at all.

## Ignored-test manifest

Each line is `RELATIVE_PATH::fn = classification`. The set must equal the
`#[ignore]`d tests found in the member crates by `tools/check-ignored.py`.

```ignored-manifest
tests/src/stream.rs::perf_bulk_rtp_mux = perf
common/src/lifecycle/suspend.rs::basics = unrunnable
server/tests/proxy_path_perf.rs::proxy_path_matched_rtt_delta = perf
```

## Performance: the tri-mandate constitution (pointer)

The operator's product constitution is **three mandates** — low tail latency
of the interactive lane, reasonable goodput of that lane (`delivery = 1.000`
without inflating its own wire), and high goodput of the bulk lane — and they
apply to this workspace's product as a whole, `proxy` included. The mandates
are **asserted by `rtp_mux`**, which owns the production dual-lane topology
(`rtp_mux/GATE.md`, "Performance", states the full constitution with each
bound's derivation; `netem_test/tests/README.md` carries the pointer table).
The bounds are never restated here, or anywhere else — one authority per
mandate.

Run the asserting gates from the `rtp_mux` checkout:

```sh
# M2 (default tier — runs on every cargo test -p rtp_mux; deterministic counts):
cargo test -p rtp_mux

# M1, median-of-3 tail-latency floor (opt-in full tier):
cargo test --release -p rtp_mux --test rtp_mux_jitter -- \
    --ignored jitter_duallane_constitution_gate_p99 --nocapture --test-threads=1

# M3, bulk-goodput capacity fraction (opt-in full tier):
cargo test --release -p rtp_mux --test dual_lane_mandates -- \
    --ignored bulk_lane_goodput_stays_above_capacity_fraction --nocapture --test-threads=1
```

Proxy owns no mandate bound. The layer's per-stream/per-byte allocation and
relay freedoms are pinned by its own default-tier tests, and the constitution's
gates live with their topology owner. `proxy` does own one **diagnosis** (the
`perf`-tier scenario below); a diagnosis reports, and any threshold it asserts
is about its own instrument.

## The deployed-path diagnosis (`perf` tier)

`server/tests/proxy_path_perf.rs::proxy_path_matched_rtt_delta` measures the
assembled artifact the operator runs — the `proxy` binary from a real config
file, entered through the access server's own TCP listener, with
`access_server` → `stream.upstream` hop `rtpmux://…` → `proxy_server`, and the
hop impaired by the same seeded `netem_test` instrument the tri-mandate arms
use — against the same load shape on the same `rtp_mux` transport with no proxy
in the path, and against the same binary's `proxy_server` entered by the
harness's own protocol client (the chain minus its access-server ingress), at
**matched end-to-end client-to-echo base RTT**.

```sh
cargo test --release -p server --test proxy_path_perf -- --ignored --nocapture
```

**Tier.** `perf` — opt-in, report-only with respect to the product. It asserts
only its instrument (non-zero samples, zero unanswered requests, echo
integrity, every arm dialed, matched base RTT, bulk traffic actually carried)
and it restates no mandate bound: those live in `rtp_mux/GATE.md` §Performance
and the mandate metrics are reported, never gated here.

**Cost.** Measured 292 s wall-clock on a warm release build over 40 arms (34
pair arms plus 6 protocol-only arms), at a load average of 12; the previous
revision's 34-arm set measured 234 s on a quieter machine, and the 6 new arms
cost about 7 s each. It starts the real binary once per proxy arm (5–9 per
run, plus one per protocol-only arm), one in-process `rtp_mux` server + two
`NetemPair` instances per direct arm, and spends 4 s of interactive load (plus
a 2 s drain) or a 3 s + 5 s bulk window per arm. The controls added alongside
the client-fronting and byte-relay pair cost 3 cadence arms at 25 ms OWD
(`direct_front_relay` twice, at 25 ms and 100 ms, and its two relay-stack
siblings once each) plus one warm-cadence arm at each 25 ms-OWD scale.

**Coverage.** Baseline: the tri-mandate `clean` impairment shape, the 256 B
interactive message, the `rtpmux` hop. Arms vary one dimension from it:

| cell | covered by |
| --- | --- |
| scale: 25 ms vs 100 ms one-way (≈50 ms vs ≈190 ms RTT) | `regime=clean25`, `regime=field100` |
| impairment: 2 % iid loss vs lossless, jitter held | `clean25` vs `jitter25` |
| load shape: request/response depth 1 vs ~5 ms pipelined cadence | `shape=rr` vs `shape=cadence` |
| multiplexing: one vs four concurrent access flows on one hop | `shape=flows4` |
| lane: interactive lane under a bulk flow (the flow migrates to the bulk lane) | `shape=bulk` on `clean25_shaped` |
| layer: proxy chain vs direct transport | every pair, both topologies |
| attribution: client-side TCP fronting, server-side byte relay | `direct_tcp_front`, `direct_relay` (cadence, 25 ms OWD) |
| attribution: those two stages stacked — the chain minus the proxy protocol, its stream wrappers and its chain plumbing | `direct_front_relay` (cadence, 25 ms and 100 ms OWD) |
| attribution: the proxy's own relay implementation, one wrapper layer per arm — `TimeoutStreamShared` plus the proxy's `copy_bidirectional` fork, then that plus `async_speed_limit::Limiter::new(f64::INFINITY)` | `direct_front_relay_tout`, `direct_front_relay_timed` (cadence, 25 ms OWD) |
| window: the measured window opening on the client's first write vs after one warm round trip | `cadence` vs `cadence_steady` (25 ms OWD) |
| attribution: the chain's access-server ingress (TCP accept, chain selection, pooled connect) | `direct_proto` — the same binary with only `proxy_server` listeners, entered by the harness's own protocol client (rr + cadence, every regime) |
| clock: the connection's establishment measured on the arm's own side of `connect()` vs charged to the chain's first message | every arm's `connect_ms`; the chain's establishment appears in its `first_ms` instead |
| metric: p50/p75/p90/p95/p99/p99.9/max, over-250 ms count, the slow samples' index span / episode count / longest run, per-segment wire multiple, delivery | every arm |
| metric: the cold-connection total (`connect_ms` + first echo) and its `rtp_mux` lane-pairing and proxy-protocol parts | every arm (`connect_ms`, `mux_dial_ms`, `protocol_ms`, `first_ms`, `cold_total_ms` in `report.json`; the cold table on stdout) |

**Deliberately empty cells.** Burst (Gilbert-Elliot) impairment: the
tri-mandate `hostile` arm's model is measured at the `rtp_mux` layer, and this
scenario's job is the *level* delta, which the iid-loss and lossless arms
bracket. Residual-loss / retransmission accounting above the transport: the
per-segment wire multiple is reported, its cause is not decomposed. The bulk
arm is a **composite** (rate shaping plus flow migration) and is reported as a
range because it varied 0.28–0.99× of the direct arm across three runs; it is
not attributed. Cross-host RTT asymmetry and real NIC/scheduler paths: every
arm is loopback, so host-local CPU and loopback TCP are inside the measurement.
The relay-stack arms (`direct_front_relay_tout`, `direct_front_relay_timed`) and
the warm arm (`cadence_steady`) are not run at 100 ms OWD: they exist to
separate the relay implementation and the window's start from the delta the
25 ms-OWD cadence carries, and the 100 ms-OWD direct arm is itself a queueing
artifact of the harness (its in-process-echo arm carries 452–492 of 760–783
samples over 250 ms across two runs), so a further control there would not
attribute. The protocol-only arm's *cadence* row at 100 ms OWD has the same
shape for the same reason and is quoted, not attributed. **The split of the
`rtp_mux` lane pairing into the `rtp` session handshake and the mux pairing is
not measured**: that needs an arm against `rtp`'s own public session API, in a
crate this workspace does not own, and the pairing is reported whole. **The
stream pool as a mitigation is not measured**: the config declares no pool, so
its cold path is a keyed miss on both sides (see below), and a configured pool
would need a pool-readiness signal the harness has no way to observe.

**Matched RTT.** Because the chain has more hops than the direct arm, the
delta is only interpretable at matched end-to-end client-to-echo base RTT, and
"loopback is negligible" is measured rather than assumed: each regime is
calibrated on the round-trip arm, and the direct link's one-way delay is
corrected by half the difference. Measured correction was `0 ms` in every
regime (chain 41.5 ms vs direct 41.0 ms at 25 ms OWD; 192.0 ms vs 192.2 ms at
100 ms OWD), so the chain's extra loopback hops add no measurable baseline. The
protocol-only arm is asserted against the regime's calibrated direct base by the
same tolerance (measured 41.0 vs 41.0 ms at `clean25`, 40.7 vs 40.9 at
`jitter25`, 191.9 vs 192.2 at `field100`), because it carries the proxy server's
loopback upstream hop that the chain also carries. The
per-arm `base` column is the shape's own minimum and is **not** the matched
baseline — a queued pipelined arm has none; the delta table prints the
calibration pair's `cal_P`/`cal_D` instead.

**What the delta is, and what the establishment charge is made of.** Matching
the base RTT does not by itself make the two windows comparable, because they
open at different points in their connection's lifecycle: `Target::connect()`
establishes the direct arm's `rtp_mux` stream before its window opens, while the
chain arm's `connect()` is a TCP accept at the access server, so the chain's
window opens while the mux session, the proxy protocol preamble and header, the
flow-kind dispatch and the upstream TCP connect are still being made. Two arms
carry that reading rather than assuming it: the plain `cadence` arm's samples
above 250 ms form **one contiguous run starting at index 0** (98 of ~800 at
25 ms OWD with 2 % loss, 81 at 0 % loss, 646 at 100 ms OWD), and the `flows4`
arm shows exactly four such samples — one per concurrent flow — with the
round-trip arm showing exactly one, at index 0. A steady-state cost cannot have
that shape: it would be spread across the arm and would scale with the sample
count rather than with the number of connections. `cadence_steady` — the same
cadence with one warm round trip taken before the window opens, on both
topologies — measures that established path: its proxy-minus-direct p99
collapses to +20.3 ms at 2 % loss and +32.8 ms at 0 % loss (the previous
revision measured −5.6 and +25.1 ms, i.e. the same collapse with the two
regimes' order swapped), with zero samples above 250 ms in either topology,
against +242 and +296 ms for the unwarmed arms in the same run.

Because the windows open at different points, one arm's first-sample latency
cannot be compared across topologies on its own. Each arm records what its own
`connect()` did, and `connect_ms + first echo` is then the same clock on every
topology: the client's first act of connecting to its first echo. It attributes
the charge:

| regime | topology | connect | `rtp_mux` lane pairing | proxy protocol | first echo | total |
| --- | --- | --- | --- | --- | --- | --- |
| `clean25` (2 % loss) | `proxy_chain` | 0.2 | – | – | 447.0 | 447.2 |
| `clean25` | `direct_transport` | 341.4 | 341.3 | – | 44.7 | 386.1 |
| `clean25` | `direct_proto` | 330.7 | 330.5 | 0.3 | 62.0 | 392.8 |
| `jitter25` (0 % loss) | `proxy_chain` | 0.2 | – | – | 410.1 | 410.3 |
| `jitter25` | `direct_transport` | 299.2 | 299.2 | – | 46.7 | 346.0 |
| `jitter25` | `direct_proto` | 298.1 | 298.1 | 0.0 | 65.5 | 363.6 |
| `field100` | `proxy_chain` | 0.1 | – | – | 1286.5 | 1286.6 |
| `field100` | `direct_transport` | 1105.7 | 1105.7 | – | 193.7 | 1299.4 |
| `field100` | `direct_proto` | 1098.7 | 1098.7 | 0.0 | 222.1 | 1320.8 |

**The charge is the `rtp_mux` lane pairing.** It is 7.3–8.3 base RTTs at the
two 25 ms-OWD scales and 5.8 at 100 ms OWD, and the **proxy-free** direct
transport pays all of it before its window even opens. The proxy protocol's own
bytes (flow kind, preamble, relay header) cost at most 0.3 ms, and removing the
loss takes only 12 % of the pairing (341 → 299 ms at 25 ms OWD), so the cost is
structural rather than a loss realisation. The chain's ingress stage and its
extra relay leg are bounded by the difference from the protocol-only arm: 54 ms
at `clean25` and 47 ms at `jitter25` — about one base RTT, 12 % of the charge —
and at 100 ms OWD the chain measures 34 ms *faster* than the protocol-only arm,
inside the noise of a single-sample reading. So the `+242 ms` proxy-minus-direct
p99 the unwarmed cadence reports at `clean25` is a **clock-placement artifact**:
on one clock the two topologies agree to within one RTT, and the direct arm's
`connect()` hides the same pairing the chain charges to its first message.

**The remaining steps, each bounded or empty.** The pool is not a factor: the
config declares no pool, so `connect_with_pool`'s `pull` is a keyed miss that
returns immediately, on the access server's hop connect and on the proxy
server's upstream connect alike; both sides take the same fallback dial. The
upstream connect is a loopback TCP connect inside the proxy server, and the
protocol-only arm pays it too, so it is inside the one-RTT residue, not in the
charge. **The locus is `rtp_mux`'s dual-lane birth**, reached through
`rtp_mux::RtpMuxConnector::connect_stream_with_lane`; `proxy` does not own that
crate, so the finding is reported here with its evidence and no change is made
to it. A deployment that wants the charge off the request path can pre-pair the
hop through the stream pool (`[stream.pool]`), whose entries connect in the
background at start-up; this scenario does not measure that, because it needs a
pool-readiness signal the harness has no way to observe (see the empty cells).

**Run-to-run stability.** The table is one full run. The chain's own cold totals
reproduce the previous revision's independently measured cold connection (404–
494 ms at 41 ms base RTT, 1416 ms at 192 ms) within that measurement's own
spread, and the proxy-free direct arm reproduces the pairing in the same run. A
second attempt under a load average of 25 reproduced every calibration base
(41.4/41.6, 41.4/41.2, 192.0/191.4 ms) but failed the pre-existing
bulk-saturation instrument assertion before the cold table printed: the direct
arm carried 0.381 MiB/s against the 0.477 MiB/s floor. That is the harness, not
the change — the bulk arm's own goodput varies with host load (0.28–0.99×
between runs) — so the cold reading here has one full-run sample and the bulk
cell is quoted, never gated.

**Vacuity.** `PROXY_PATH_PERF_FAULT=zero_samples` empties an arm,
`PROXY_PATH_PERF_FAULT=unanswered` issues a request that is never answered,
`PROXY_PATH_PERF_FAULT=warm_unanswered` leaves the warm arm's pre-window round
trip unanswered — the steady arm's own instrument path, rather than its load
shape — and `PROXY_PATH_PERF_FAULT=undialed` discards an arm's dial record after
it ran, so an arm with samples reports no cold-connection reading. All four must
fail the shared guard that every arm — every topology, the control arms and the
warm arm — goes through. Demonstrated on the committed revision, each a separate
run at exit 101:

| injection | failing arm and message |
| --- | --- |
| `zero_samples` | `INSTRUMENT: arm proxy_chain/rr measured zero samples` |
| `unanswered` | `INSTRUMENT: arm proxy_chain/rr left 1 request(s) unanswered` |
| `warm_unanswered` | `INSTRUMENT: arm proxy_chain/cadence_steady measured zero samples` (a warm-up that never completed produced no sample, so the zero-sample assertion is the one that fires) |
| `undialed` | `INSTRUMENT: arm proxy_chain/rr never dialed, so it measured no connection` |

The guard that `unanswered` exercises is real in healthy runs too: it is
what caught the cadence arm's own write-half teardown truncating in-flight
echoes, which is now fixed by holding the write half open across the drain.

## Residual limitations

The checker is regex-and-brace-counting, the same tool level as the netem_test
and rtp gates: it sees a `#[ignore]` attribute followed by a `fn` in the same
file, and an assertion token only inside the fn's brace-balanced body. An
assertion hidden behind a macro alias, a trait object, or a function pointer
is invisible to it. Nothing here substitutes for running the perf benchmark
when the bulk throughput it measures is in scope — the manifest guarantees the
classification and the set, not the measurements.