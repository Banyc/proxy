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
  measured, nothing was left unanswered, every echo matched, the two
  topologies really were compared at the same end-to-end base RTT, and the
  direct arm really carried bulk traffic). It asserts **no product bound**.
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
in the path, at **matched end-to-end client-to-echo base RTT**.

```sh
cargo test --release -p server --test proxy_path_perf -- --ignored --nocapture
```

**Tier.** `perf` — opt-in, report-only with respect to the product. It asserts
only its instrument (non-zero samples, zero unanswered requests, echo
integrity, matched base RTT, bulk traffic actually carried) and it restates no
mandate bound: those live in `rtp_mux/GATE.md` §Performance and the mandate
metrics are reported, never gated here.

**Cost.** Measured 234 s wall-clock on a warm release build over 34 arms. It starts
the real binary once per proxy arm (5–9 per run), one in-process `rtp_mux`
server + two `NetemPair` instances per direct arm, and spends 4 s of interactive
load (plus a 2 s drain) or a 3 s + 5 s bulk window per arm. The controls added
alongside the client-fronting and byte-relay pair cost 3 cadence arms at 25 ms
OWD (`direct_front_relay` twice, at 25 ms and 100 ms, and its two relay-stack
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
| metric: p50/p75/p90/p95/p99/p99.9/max, over-250 ms count, the slow samples' index span / episode count / longest run, per-segment wire multiple, delivery | every arm |

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
artifact of the harness (its in-process-echo arm carries the largest tail of any
arm in that regime), so a further control there would not attribute.

**Matched RTT.** Because the chain has more hops than the direct arm, the
delta is only interpretable at matched end-to-end client-to-echo base RTT, and
"loopback is negligible" is measured rather than assumed: each regime is
calibrated on the round-trip arm, and the direct link's one-way delay is
corrected by half the difference. Measured correction was `0 ms` in every
regime (chain 41.2 ms vs direct 41.1 ms at 25 ms OWD; 191.5 ms vs 193.1 ms at
100 ms OWD), so the chain's extra loopback hops add no measurable baseline. The
per-arm `base` column is the shape's own minimum and is **not** the matched
baseline — a queued pipelined arm has none; the delta table prints the
calibration pair's `cal_P`/`cal_D` instead.

**What the delta is.** Matching the base RTT does not by itself make the two
windows comparable, because they open at different points in their connection's
lifecycle: `Target::connect()` establishes the direct arm's `rtp_mux` stream
before its window opens, while the chain arm's `connect()` is a TCP accept at
the access server, so the chain's window opens while the mux session, the proxy
protocol preamble and header, the flow-kind dispatch and the upstream TCP
connect are still being made. Two arms carry that reading rather than assuming
it: the plain `cadence` arm's samples above 250 ms form **one contiguous run
starting at index 0** (104–111 of ~800 at 25 ms OWD, 99 at 0 % loss, 596–716 at
100 ms OWD), and the `flows4` arm shows exactly four such samples — one per
concurrent flow — with the round-trip arm showing exactly one, at index 0. A
steady-state cost cannot have that shape: it would be spread across the arm and
would scale with the sample count rather than with the number of connections.
`cadence_steady` — the same cadence with one warm round trip taken before the
window opens, on both topologies — measures that established path: its
proxy-minus-direct p99 is −5.6 ms at 2 % loss and +25.1 ms at 0 % loss, with
zero samples above 250 ms in either topology, against +289 ms and +374 ms for
the unwarmed arms in the same run. So the delta the unwarmed arms report is the
chain's **connection establishment**, charged to the first messages of a window
that was opened before the chain was up.

**Vacuity.** `PROXY_PATH_PERF_FAULT=zero_samples` empties an arm,
`PROXY_PATH_PERF_FAULT=unanswered` issues a request that is never answered, and
`PROXY_PATH_PERF_FAULT=warm_unanswered` leaves the warm arm's pre-window round
trip unanswered — the steady arm's own instrument path, rather than its load
shape. All three must fail the shared guard that every arm — the two topologies,
the control arms and the warm arm — goes through. Demonstrated on the committed
revision: exit 101 with `INSTRUMENT: arm proxy_chain/rr measured zero samples`,
`INSTRUMENT: arm proxy_chain/rr left 1 request(s) unanswered`, and `INSTRUMENT:
arm proxy_chain/cadence_steady measured zero samples` (a warm-up that never
completed produced no sample, so the zero-sample assertion is the one that
fires). The guard that `unanswered` exercises is real in healthy runs too: it is
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