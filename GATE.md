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

**Tier.** `perf` in this file's own inventory — and `full` in the shared gate
manifest, because the shared checker reserves `perf` for a report-only
scenario (see "Two vocabularies share the word `perf`") — opt-in, report-only
with respect to the product. It asserts
only its instrument (non-zero samples, zero unanswered requests, echo
integrity, every arm dialed, matched base RTT, bulk traffic actually carried)
and it restates no mandate bound: those live in `rtp_mux/GATE.md` §Performance
and the mandate metrics are reported, never gated here.

**Cost.** Measured 414 s wall-clock on a warm release build over 65 arms (32
pair arms and 6 protocol-only arms, which are the run's own `arms` list — 38
entries — less its `proto_arms` list, 6; plus the 27 stream-pool arms in its
three `pool_*_arms` lists), on a host also running three concurrent agent builds;
earlier figures were 292 s and 260 s over the 40-arm set on a quieter machine,
and 234 s over the 34-arm set. An earlier revision of this section said 67 arms
over "34 pair arms, 6 protocol-only arms, 27 stream-pool arms"; that does not
add up on any revision, because 34 is the *total* arm count of the revision
before the protocol-only arms existed (its `report.json` lists 34 arms and no
`direct_proto` arm), so the line mixed one revision's total with another's
breakdown. The counts are stable only if they are read from the run's own
`report.json`, which is why they are quoted that way here. It starts the
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

**Deployment decision (nightly): the pool stays empty.** The deployed access
config ships `[stream] pool = []`, and this section's benefit does not apply to
the traffic that deployment serves. The charge measured here is per **cold
connection**, and a cold connection is created per *client connection* to the
access server — so for a client that multiplexes its traffic over one
long-lived mux session it is paid **once per session**, not once per request.
Pre-pairing would save one establishment per session lifetime, in exchange for
a resident population of warm connections on both relays (see the readiness
gauge above), and steady state is unchanged either way. Revisit if the client
shape changes to many short-lived connections — one per request or per page
load — which is the pattern these arms model and the regime where the
0.13–0.16× applies.

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

## The dual-mandate declaration (time and coverage)

`netem_test/tools/check-gate.py` is the shared checker that enforces the
perf-test dual mandate of `AGENTS.md` ("The perf-test dual mandate — time and
coverage") from a crate's own `GATE.md`: `gate-perf-design` names each declared
scenario with its tier, its nominal cost, how it stands to a reference row and
the coverage cells it claims; `gate-budgets` states each tier's budget and the
reference rows; and `gate-coverage-gaps` records the cells the set does not
cover, with the reason. It reads three further blocks: `gate-manifest` (the
shared tier taxonomy, `target::test = tier`, for the scenario directory the
invocation is pointed at — the same set this file's `ignored-manifest` records
in `tools/check-ignored.py`'s vocabulary), `gate-asserting` (the asserting /
report-only split) and `gate-perf-guard-helpers` (the asserting helpers a
report-only `perf` scenario reaches — empty here, see below).

One invocation covers **one package and one scenario directory**, so the command
names the package that owns this workspace's perf scenario:

```sh
python3 ../netem_test/tools/check-gate.py --crate . server server/tests GATE.md
```

The three blocks it needs that this file did not have before are below. The
`gate-manifest` block is scoped to `server/tests` (the directory the invocation
is pointed at), and `gate-perf-guard-helpers` is empty because no scenario of
this package is in the report-only `perf` tier.

```gate-manifest
proxy_path_perf::proxy_path_matched_rtt_delta = full
```

```gate-asserting
proxy_path_perf::proxy_path_matched_rtt_delta
```

```gate-perf-guard-helpers
```

`tests`'s `perf_bulk_rtp_mux` (a `--lib` unit test of the package `tests`) and
`common`'s `basics` are outside this invocation; they stay recorded in
`ignored-manifest` and checked by `tools/check-ignored.py`.

### Two vocabularies share the word `perf`

This file's `ignored-manifest` calls `proxy_path_matched_rtt_delta` `perf`,
meaning *an asserting end-to-end performance scenario, kept opt-in because it is
slow*. The shared checker's `perf` tier means **report-only**: it scans the
scenario's own body for an assertion token and refuses a `perf` row that has
one, then scans every asserting helper the scenario reaches. This scenario
asserts its own instrument — one `assert!` in the body itself, plus its
`assert_sane` guard, `assert_matched_rtt`, `assert_proto_rtt_matched` and the
pooled arms' readiness barrier below it — so the shared taxonomy files it as
`full`. Both labels describe the same `#[ignore]`d opt-in scenario; only the
contract each one names differs, and nothing about the test changes.

### The declared rows, and why only two

The checker's row identity is a compiled test: a `gate-perf-design` row is
`<target>::<test>`, resolved from the compiled test binaries, and a family needs
at least two rows — its reference and a row stating a relation against it. Two
consequences fix the shape of this declaration:

- **Every arm of the diagnosis is inside one test.** The 65 arms the scenario
records are internal — the loops over regimes, shapes, controls and pool
replications in `server/tests/proxy_path_perf.rs` — not 65 tests, so no arm can
be a row. A row per arm needs the scenario split into 65 test functions (a
change to the perf scenario itself, which this declaration may not make) or a
grammar extension in the shared tooling, which this crate does not own. The
arm-level inventory, its cells and its attribution are therefore the prose and
the `gate-coverage-gaps` lines below, not rows.
- **The `server` package owns exactly one opt-in scenario**, so the reference
row cannot be a sibling measurement. It is the cheapest scenario in the same
directory that the diagnosis's instrument actually depends on:
`monitor::the_monitor_router_serves_health_metrics_and_both_session_tables`
serves and asserts the `/metrics` route the pooled arms' readiness barrier
polls (`wait_pool_ready` → `http_get` → `pool_ready_from_metrics`). The
diagnosis stands against it as a **composite**, which is the honest reading: it
varies five dimensions from that reference, and there is no sibling arm one
dimension away from it.

The `default` budget below covers only the rows declared here, not the
package's whole default tier (which this declaration does not enumerate).

```gate-perf-design
monitor::the_monitor_router_serves_health_metrics_and_both_session_tables = default | 0.2 | baseline | proxy-instrument@layer=monitor+route=metrics-and-sessions+metric=routes-served
proxy_path_perf::proxy_path_matched_rtt_delta = full | 403.6 | composite(hop,impairment,metric,scale,shape) | proxy-path@hop=rtpmux+impairment=iid2pct-and-lossless-and-shaped+scale=owd25-and-owd100+shape=rr-cadence-flows4-bulk+metric=latency-and-cold-connection-and-goodput
```

```gate-budgets
default = 5
full = 480
baseline = monitor::the_monitor_router_serves_health_metrics_and_both_session_tables
drift = 0.5
drift_floor_s = 2.0
```

### The arm inventory the grammar cannot hold

The diagnosis records **65 arms**: 32 pair arms (a `proxy_chain` and a
`direct_transport` arm per shape and regime, plus the attribution ladder and
the two controls), 6 protocol-only arms and 27 stream-pool arms. Each line below
is an arm group, the dimension it varies from the group above it, and the
regimes it runs at — the one-dimension-per-arm rule applied to the scenario's
own structure rather than to a row set.

| arm group | arms | dimension varied | regimes |
| --- | --- | --- | --- |
| `rr` calibration pair (`proxy_chain/rr`, `direct_transport/rr`) | 6 | reference (256 B, depth 1, 4 s window) | clean25, jitter25, field100 |
| `cadence` pair (`proxy_chain/cadence`, `direct_transport/cadence`) | 6 | `shape`: rr → 5 ms cadence | all three |
| `direct_tcp_front/cadence` | 3 | `stage`: the client-side TCP front the access server interposes | all three |
| `direct_relay/cadence` | 2 | `stage`: the server-side byte relay, no protocol | clean25, jitter25 |
| `direct_front_relay/cadence` | 3 | **composite(`stage`)**: front + relay stacked at once | all three |
| `direct_front_relay_tout`, `direct_front_relay_timed` (cadence) | 4 | `relay-impl`: harness copy → `TimeoutStreamShared` → plus the limiter | clean25, jitter25 |
| `cadence_steady` pair | 4 | `window`: opens cold → after one warm round trip | clean25, jitter25 |
| `direct_proto` (`rr`, `cadence`) | 6 | `layer`: chain-minus-ingress, entered by the protocol client | all three |
| `flows4` pair | 2 | `flows`: 1 → 4 concurrent access flows | clean25 |
| `bulk` pair on `clean25_shaped` | 2 | **composite(`shape`,`rate`)**: bulk upload + 8 Mbps shaper | clean25_shaped |
| pool set (`proxy_chain_nopool`, `_pool_ready`, `_pool`) × 3 replications | 27 | `mitigation`: no pool → pooled; then `settle`: 0 s → 3 s | all three |
| (total) | **65** | | |

**Where the attribution costs something, and what it costs.** The ladder is
one dimension per step except in three places, all of them named in the prose
above and none of them a defect of the arms:

- `direct_front_relay` varies two stages at once by construction (it is the
  "both stages, no proxy protocol" control); its one-dimension relatives are
  `direct_front_relay_tout` and `direct_front_relay_timed`, so what it
  attributes is the relay *implementation*, not the two stages separately. The
  two stages separately are `direct_tcp_front` and `direct_relay`, which vary
  one each from `direct_transport`.
- The bulk pair varies shape **and** the shaper's rate against the cadence arm,
  and no arm in the set varies either alone (an unshaped bulk arm, or a shaped
  interactive arm), so the rate axis is unattributable here and the prose
  quotes the pair as a 0.28–0.99× range instead. This is the one arm group in
  the scenario with no one-dimension relative at all.
- `proxy_chain_nopool` re-measures `proxy_chain/rr` — the same configuration,
  on its own fresh process — so the pool step is read against a second
  realization of the reference rather than against the reference itself. That
  is deliberate (all three pool arms must share one clock and one host
  condition) and it is the only re-measurement in the set.

`direct_proto` is a single declared dimension (`layer`) even though it differs
from `direct_transport` in two implementation stages (the ingress stage and the
protocol preamble), because the arm pairs them by construction and its cells
claim one layer contrast; a reader who needs the two separated has
`direct_front_relay`'s relation instead.

### Cost, measured rather than cited

Neither row's cost is recorded in any document, and the shared checker refuses
a guessed number, so both were measured on this checkout's dependencies
(published tags: `rtp v0.0.97`, `rtp_mux v0.0.24`), on a warm release build, with
`/usr/bin/time -p` around a single-test invocation and libtest's own
`finished in` line as the per-test number (`proxy`'s toolchain is **stable**, so
libtest's `-Z unstable-options --report-time` stamp is unavailable and the
whole-binary total is the test's own time because the invocation selects one
test):

| row | command | measured |
| --- | --- | --- |
| `monitor::the_monitor_router_serves_health_metrics_and_both_session_tables` | `cargo test --release -p server --test monitor` | 0.20 s (`finished in 0.20s`; `/usr/bin/time` real 0.35 s) |
| `proxy_path_perf::proxy_path_matched_rtt_delta` | `cargo test --release -p server --test proxy_path_perf -- --ignored --nocapture` | 403.6 s (`finished in 403.64s`; `/usr/bin/time` real 403.79 s) |

The diagnosis was measured on a host whose 1-minute load average was 11.6 at
start (other work on the machine); that invocation exited 0 with every
instrument assertion holding and recorded the same 65 arms as the earlier full
run, so the two are comparable measurements of one scenario rather than two
shapes. The same scenario measured 414.6 s on the earlier full run the `Cost.`
paragraph above cites as 414 s — the 65-arm shape, with the pool arms — and
143.6–172.9 s on a quieter host at the pre-pool revision. The spread is host
load, not a change in the arms, so the `full` budget is the measured cost with
headroom rather than a tight bound: 403.6 s + 72.6 s (18 %), rounded up to a
whole ten seconds, is the 480 s budget the `gate-budgets` block above declares.
That budget covers the declared rows only; the ruling figure is printed by the
checker itself in its `gate-budgets:` summary line.

```gate-coverage-gaps
arm-row@granularity=test-not-arm = a `gate-perf-design` row is a compiled test (`<target>::<test>`), and all 65 arms of the diagnosis are internal to one `#[ignore]`d test function, so no arm can be a row. The repair is one of two changes this declaration may not make: split the scenario into one test per arm (a change to the perf scenario itself), or extend the shared grammar with an arm-level row identity (tooling this crate does not own). What the arms do cover is recorded instead as the inventory table above and the lines below.
attribution@baseline-family=proxy-path = the diagnosis is declared as a `composite(hop,impairment,metric,scale,shape)` against the only row in the same package it can stand against, the monitor-route instrument test, so the declaration attributes nothing about the diagnosis: no sibling arm one declared dimension away exists in this package, and the arm-level structure that *is* one dimension per step (the `direct_tcp_front`/`direct_relay`/`direct_front_relay_tout` ladder, the `rr`→`cadence` and clean→jitter→field regime steps, the pool and settle steps) is not expressible as rows for the reason on the `arm-row` line. A second opt-in scenario in this package one dimension from the diagnosis would close the attribution half; a grammar change would close both.
proxy-path@impairment=GE-burst = the tri-mandate `hostile` arm's Gilbert-Elliot model is measured at the `rtp_mux` layer; this scenario's job is the level delta, which the iid-loss and lossless arms bracket, so no burst arm is claimed here.
proxy-path@topology=cross-host = every arm is loopback, so host-local CPU and loopback TCP are inside the measurement; a cross-host path would need a second host, which a single-process harness cannot supply.
proxy-path@layer=rtp-session-handshake = the `rtp` session handshake and the mux lane pairing are reported whole (`mux_dial_ms`), not split; separating them needs an arm against `rtp`'s own public session API, in a crate this workspace does not own.
proxy-path@metric=residual-retransmission-accounting = the per-segment wire multiple is reported (`wire_interactive_c2s_bytes / offered_bytes`); its decomposition into retransmissions and FEC parity is not, and belongs to the transport layer that owns those counters.
proxy-path@scale=pool-queue-depth-above-16 = every pool arm dials one flow, so what a burst of more than the pool's 16-entry queue depth pays, and whether the queue drains in time for a 17th, is outside the scenario.
proxy-path@state=config-reload-and-suspend = the pool's lifetime across a config reload or a system resume is not measured: the reload path replaces the pool wholesale and no arm reloads, and a real suspend cannot be produced in a test process (see `basics` above).
proxy-path@state=pool-warm-versus-usable = the readiness barrier observes the pool's exported gauge, which is *established and unpulled* — necessary but not sufficient for usability — and the 3 s settle the settled arm takes is a constant chosen from a probe's settle curve, not a measurement of what makes a fresh session unusable. The instrument shows the effect, not its cause; the candidates (the session's own path exploration settling, its send-window ramp, the second lane's lazy birth) remain candidates.
proxy-path@arm=relay-stack-at-owd100 = `direct_front_relay_tout` and `direct_front_relay_timed` are not run at 100 ms OWD: they exist to separate the relay implementation from the delta the 25 ms-OWD cadence carries, and the 100 ms-OWD direct arm is itself a harness queueing artifact, so a further control there would not attribute.
proxy-path@arm=protocol-only-cadence-at-owd100 = the protocol-only cadence arm at 100 ms OWD has the same queueing shape for the same reason: it is quoted in the prose, not attributed.
framework@package=tests = the workspace's other perf scenario, `tests::perf_bulk_rtp_mux` (a `--lib` unit test of the package `tests` that pushes 32 MiB through a real chain and asserts byte-exact delivery), cannot be declared in this block: the checker takes one package and one scenario directory per invocation, and its `gate-manifest` block must equal the ignored set of the directory it is pointed at, so a block carrying both packages' rows fails in both invocations. The repair is a second declaration file for the `tests` package (or a gate that spans the workspace), neither of which this change may add; the scenario stays recorded in `ignored-manifest`, checked by `tools/check-ignored.py`.
framework@taxonomy=perf = the shared checker's `perf` tier means report-only and this workspace's `ignored-manifest` uses `perf` for an asserting opt-in benchmark, so the same test carries two labels that read as a contradiction until the difference is stated (see "Two vocabularies share the word `perf`"). The repair is a rename in one vocabulary, or a shared tier whose name does not collide; until then the two blocks must be read together.
framework@family=two-row-minimum = a family needs a reference row plus a row stated against it, and this package owns one opt-in scenario, so the reference row is a functional monitor-route test rather than a sibling measurement. The repair is a sibling arm one dimension from the diagnosis in the same package, or shared-tooling support for a single-row family.
```

## Residual limitations

The checker is regex-and-brace-counting, the same tool level as the netem_test
and rtp gates: it sees a `#[ignore]` attribute followed by a `fn` in the same
file, and an assertion token only inside the fn's brace-balanced body. An
assertion hidden behind a macro alias, a trait object, or a function pointer
is invisible to it. Nothing here substitutes for running the perf benchmark
when the bulk throughput it measures is in scope — the manifest guarantees the
classification and the set, not the measurements.