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
  protocol-only arm against its regime's calibration — the direct arm really
  carried bulk traffic, and every stream-pool arm really observed a ready pool
  entry before its window opened). It asserts **no product bound**. See "The
  deployed-path diagnosis" below.
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

**Cost.** Measured 414 s wall-clock on a warm release build over 67 arms (34
pair arms, 6 protocol-only arms, 27 stream-pool arms), on a host also running
three concurrent agent builds; earlier figures were 292 s and 260 s over the
40-arm set on a quieter machine, and 234 s over the 34-arm set. It starts the
real binary once per proxy arm (5–9 per run, plus one per protocol-only arm and
three per stream-pool replication — control, pooled-at-readiness, pooled with
settle), one in-process `rtp_mux` server + two `NetemPair` instances per direct
arm, and spends 4 s of interactive load (plus a 2 s drain) or a 3 s + 5 s bulk
window per arm. The pool arms add 27 binary starts (3 replications × 3 arms ×
3 regimes), each 4 s of load, plus the pool's own readiness wait (0.3 s at
25 ms OWD, 1.1 s at 100 ms) and a 3 s settle on the settled arm. The controls
added alongside the client-fronting and byte-relay pair cost 3 cadence arms at
25 ms OWD (`direct_front_relay` twice, at 25 ms and 100 ms, and its two
relay-stack siblings once each) plus one warm-cadence arm at each 25 ms-OWD
scale.

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
| mitigation: a configured, background-pre-paired `[stream.pool]` vs no pool | `proxy_chain_pool` vs `proxy_chain_nopool`, paired per replication, at all three regimes |
| warm-up: the window opening the moment the pool reports ready vs 3 s later | `proxy_chain_pool_ready` vs `proxy_chain_pool`, one dimension apart |
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
crate this workspace does not own, and the pairing is reported whole. **Why a
freshly born mux session is unusable for about a second** — the settle the pool
arms need (see "The `[stream.pool]` mitigation" below) — is not attributed: the
instrument shows the effect, not its cause, and the candidates (the session's
own path exploration settling, its send-window ramp, the second lane's lazy
birth) are listed as candidates only. **The pool's behaviour above its 16-entry
queue depth is not measured**: every pool arm dials one flow, so what a burst of
more than 16 concurrent new flows pays, and whether the queue drains in time for
a 17th, is outside this scenario. **The pool's lifetime across a config reload
or a system resume is not measured either**; the reload path replaces the pool
wholesale and no arm reloads.

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
topology: the client's first act of connecting to its first echo. Two full runs
(A, then B) attribute the charge:

| regime | topology | run | connect | of which lane pairing | of which protocol | first echo | total |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `clean25` (2 % loss) | `proxy_chain` | A / B | 0.2 / 0.1 | – | – | 447.0 / 531.3 | 447.2 / 531.4 |
| `clean25` | `direct_transport` | A / B | 341.4 / 339.4 | 341.3 / 339.4 | – | 44.7 / 47.8 | 386.1 / 387.2 |
| `clean25` | `direct_proto` | A / B | 330.7 / 336.2 | 330.5 / 336.2 | 0.3 / 0.0 | 62.0 / 69.1 | 392.8 / 405.3 |
| `jitter25` (0 % loss) | `proxy_chain` | A / B | 0.2 / 0.1 | – | – | 410.1 / 415.4 | 410.3 / 415.4 |
| `jitter25` | `direct_transport` | A / B | 299.2 / 327.7 | 299.2 / 327.7 | – | 46.7 / 49.2 | 346.0 / 376.9 |
| `jitter25` | `direct_proto` | A / B | 298.1 / 351.2 | 298.1 / 351.2 | 0.0 / 0.0 | 65.5 / 67.5 | 363.6 / 418.7 |
| `field100` | `proxy_chain` | A / B | 0.1 / 0.1 | – | – | 1286.5 / 1299.1 | 1286.6 / 1299.2 |
| `field100` | `direct_transport` | A / B | 1105.7 / 1127.0 | 1105.7 / 1127.0 | – | 193.7 / 193.2 | 1299.4 / 1320.2 |
| `field100` | `direct_proto` | A / B | 1098.7 / 1099.3 | 1098.7 / 1099.3 | 0.0 / 0.0 | 222.1 / 224.5 | 1320.8 / 1323.8 |

**The charge is the `rtp_mux` lane pairing.** It is 7.3-8.3 base RTTs at the
two 25 ms-OWD scales and 5.8 at 100 ms OWD, and the **proxy-free** direct
transport pays all of it before its window even opens. Across the two runs the
pairing reproduces within 12 % (339-341 ms at `clean25`, 299-328 at `jitter25`,
1099-1127 at `field100`), and the lossless regime removes only 12-16 % of it, so
the cost is structural rather than a loss realisation. The proxy protocol's own
bytes (flow kind, preamble, relay header) cost at most 0.3 ms. So the `+242 ms`
proxy-minus-direct p99 the unwarmed cadence reports at `clean25` is a
**clock-placement artifact**: on one clock the direct transport pays the same
pairing the chain charges to its first message.

**The chain's ingress stage and its extra relay leg are bounded, not
resolved.** Their contribution is the difference from the protocol-only arm,
which is a single sample per regime per run: run A gives +54 ms at `clean25`,
+47 at `jitter25` and −34 at `field100`, run B +126, −3 and −25 ms. The spread
(two runs, same revision, same seeds; ~1.3 base RTT at the 25 ms scales, and the
sign flips at 100 ms) is the run-to-run spread of the unwarmed first sample, so
the honest bound is: the chain's ingress plus its extra relay leg is at most a
two-base-RTT effect and never a large share of the charge, and this instrument
cannot resolve it further without more samples of the same arm.

**The remaining steps, each bounded or empty.** The pool is not a factor for
any arm *above*: those configs declare no pool, so `connect_with_pool`'s `pull`
is a keyed miss that returns immediately, on the access server's hop connect and
on the proxy server's upstream connect alike; both sides take the same fallback
dial. The upstream connect is a loopback TCP connect inside the proxy server,
and the protocol-only arm pays it too, so it is inside the one-RTT residue, not
in the charge. **The locus is `rtp_mux`'s dual-lane birth**, reached through
`rtp_mux::RtpMuxConnector::connect_stream_with_lane`; `proxy` does not own that
crate, so the finding is reported here with its evidence and no change is made
to it.

**The `[stream.pool]` mitigation, measured.** A deployment can move the charge
off the request path by pre-pairing the hop through the stream pool
(`[stream.pool]`), whose entries connect in the background at start-up. Three
arms per replication measure that, all on one clock: `proxy_chain_nopool` (the
same chain with no pool), `proxy_chain_pool_ready` (pooled, window opened the
moment the pool reports a ready entry) and `proxy_chain_pool` (pooled, window
opened 3 s after that readiness). Warm-ness is observed from *outside* the
binary, which is what the previous revision could not do: the pool now exports
`stream.pool.ready{key}` on the monitor listener — established-and-unpulled
connections per key, moved by the three events that push a connection into or
out of the queue — and each pooled arm asserts it observed at least one such
entry for its own hop key before opening its window, so an arm that merely
configured a pool cannot race its own first dial. The pool banked its full
queue (16 of 16) every time, after a wait that is **one pairing's worth**, not
dial×16: 314–393 ms at 25 ms OWD and 1088–1102 ms at 100 ms OWD. That is what
per-address session reuse would predict — the first entry pairs the session and
the rest open streams on it — but the instrument sees the queue depth, not which
entry paid for it, so the sharing is an inference and the wait is the
observation.

| regime | replication | control (no pool) | pooled, window at readiness | pooled, +3 s settle |
| --- | --- | --- | --- | --- |
| `clean25` (41.2 ms base) | A / B / C | 425.5 / 518.7 / 438.9 | 388.4 / 397.3 / 356.3 | **66.6 / 70.9 / 61.2** |
| `jitter25` (41.2 ms base) | A / B / C | 456.8 / 402.7 / 467.9 | 440.6 / 348.3 / 326.7 | **57.9 / 113.4 / 58.1** |
| `field100` (191.8 ms base) | A / B / C | 1321.6 / 1291.7 / 1420.8 | 386.6 / 481.0 / 702.2 | **206.8 / 206.6 / 201.9** |
| `clean25` median | | 438.9 | 388.4 | **66.6 (0.139×)** |
| `jitter25` median | | 456.8 | 348.3 | **58.1 (0.127×)** |
| `field100` median | | 1321.6 | 481.0 | **206.6 (0.156×)** |

**The pool removes the charge almost entirely — but only once the session has
settled, and that is not observable.** The settled arm's cold-connection total
is 0.13–0.16× the unpooled control's at every regime: 438.9 → 66.6 ms at
`clean25`, 456.8 → 58.1 at `jitter25`, 1321.6 → 206.6 at `field100`: from
10.7–11.1 base RTTs down to 1.4–1.6 at 25 ms OWD, and from 6.9 down to 1.1 at
100 ms OWD. That is better than the
direct transport's own cold path (386–387 ms, 1299–1320 ms), because the direct
arm pays the same pairing inside its `connect()`. The spread is small — six of
the nine settled replications lie within 5 ms of their regime's median, one
`jitter25` replication at 113.4 ms being the only outlier — so this is a
decisive positive, not a noise reading. Every number in the table is a reported
arm; the settle curve quoted just below is probe evidence instead (one
replication per point, kept because it is what the arm's 3 s constant is chosen
from).

What it does **not** fix is the window between readiness and usability. An arm
that opens its window the moment the pool reports a ready entry — which is what
"configure the pool and dial" amounts to, and what an arm without a readiness
probe would measure by accident — still pays most of the charge at `clean25`
(388.4 ms, 0.88× the control) and gets a wild reading at `field100` (386.6 /
481.0 / 702.2 ms, 0.29–0.53×). A probe on this revision measured the settle
curve directly: 0 / 1 / 3 s costs 369 / 65 / 61 ms at 25 ms OWD and
579 / 205 / 212 ms at 100 ms OWD. So a freshly born mux session is not usable
for about a second even though its pool has already banked sixteen streams on
it, and **the pool's exported warm-ness is necessary but not sufficient: nothing
observable distinguishes "banked" from "usable"**. A deployment that restarts
and takes traffic immediately therefore pays the full charge for its first flow
(or its first second), and the mitigation's value is realized only after that.
The remaining empty cells name what is not attributed here.

**Run-to-run stability.** Both full runs are green on this revision's healthy
path and are the two columns of the cold table. The lane pairing reproduces
within 12 %; the chain's own unwarmed first sample does not (`clean25` 447 then
531 ms) and neither does the ingress residue, which is why the ingress bound
above is stated as a bound. The chain's totals do sit within the previous
revision's independently measured cold connection (404–494 ms at 41 ms base
RTT, 1416 ms at 192 ms), whose own spread is the same shape. The previous
revision's caveat stands: the control arms' per-arm p99 varies tens to ~200 ms
run to run, while the over-250 ms counts are stable. A third attempt at a load
average of 25 reproduced every calibration base (41.4/41.6, 41.4/41.2,
192.0/191.4 ms) but failed the pre-existing bulk-saturation instrument
assertion before the cold table printed: the direct arm carried 0.381 MiB/s
against the 0.477 MiB/s floor. That is the harness under host load, not the
change — the bulk arm's own goodput varies 0.28–0.99× between runs — so the bulk
cell is quoted, never gated.

**The pool run.** One further full run on the revision that adds the pool arms
(58 arms, 414 s, on a host also running three concurrent agent builds) is the
run the stream-pool table and the pool's readiness waits are quoted from. It
reproduced every calibration base (41.2/41.2, 41.4/41.1, 191.8/192.0 ms) and
the unwarmed arms' charge (the `clean25` chain `rr` arm still 511.6 ms, the
`field100` one 1305.9 ms, the `jitter25` one 418.9 ms), so the pool arms are
additive readings beside the existing matrix rather than a re-measurement of it.

**Vacuity.** `PROXY_PATH_PERF_FAULT=zero_samples` empties an arm,
`PROXY_PATH_PERF_FAULT=unanswered` issues a request that is never answered,
`PROXY_PATH_PERF_FAULT=warm_unanswered` leaves the warm arm's pre-window round
trip unanswered — the steady arm's own instrument path, rather than its load
shape — `PROXY_PATH_PERF_FAULT=undialed` discards an arm's dial record after
it ran, so an arm with samples reports no cold-connection reading, and
`PROXY_PATH_PERF_FAULT=pool_unwarmed` runs the pooled arm against a config that
declares no pool, so its readiness barrier must fail. All five must fail the
guard their own path shares with the healthy runs. Demonstrated on the
committed revision, each a separate run at exit 101:

| injection | failing arm and message |
| --- | --- |
| `zero_samples` | `INSTRUMENT: arm proxy_chain/rr measured zero samples` |
| `unanswered` | `INSTRUMENT: arm proxy_chain/rr left 1 request(s) unanswered` |
| `warm_unanswered` | `INSTRUMENT: arm proxy_chain/cadence_steady measured zero samples` (a warm-up that never completed produced no sample, so the zero-sample assertion is the one that fires) |
| `undialed` | `INSTRUMENT: arm proxy_chain/rr never dialed, so it measured no connection` |
| `pool_unwarmed` | `INSTRUMENT: the stream pool never banked 1 ready connection(s) for key rtpmux://127.0.0.1:60402 within 10 s, so this arm cannot tell a warm pool from a cold one and must not claim to have measured one (last error: None; pool samples seen: [])` |

The `pool_unwarmed` message is the vacuity that matters for the mitigation:
`last error: None` proves the monitor endpoint answered every poll, and
`pool samples seen: []` proves the barrier failed because the pool had nothing
to report rather than because the instrument could not ask. The barrier is the
same code path in the pooled runs, so an unwarmed pool cannot be reported as
warm there either.

The guard that `unanswered` exercises is real in healthy runs too: it is what
caught the cadence arm's own write-half teardown truncating in-flight echoes,
which is now fixed by holding the write half open across the drain.

## Residual limitations

The checker is regex-and-brace-counting, the same tool level as the netem_test
and rtp gates: it sees a `#[ignore]` attribute followed by a `fn` in the same
file, and an assertion token only inside the fn's brace-balanced body. An
assertion hidden behind a macro alias, a trait object, or a function pointer
is invisible to it. Nothing here substitutes for running the perf benchmark
when the bulk throughput it measures is in scope — the manifest guarantees the
classification and the set, not the measurements.