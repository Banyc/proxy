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

Proxy owns no performance oracle yet: its layer's per-stream/per-byte
allocation and relay freedoms are pinned by its own default-tier tests, and
any future proxy perf lane that asserts a mandate must land in `proxy` and be
recorded here; until then the constitution's three gates live with their
topology owner.

## Residual limitations

The checker is regex-and-brace-counting, the same tool level as the netem_test
and rtp gates: it sees a `#[ignore]` attribute followed by a `fn` in the same
file, and an assertion token only inside the fn's brace-balanced body. An
assertion hidden behind a macro alias, a trait object, or a function pointer
is invisible to it. Nothing here substitutes for running the perf benchmark
when the bulk throughput it measures is in scope — the manifest guarantees the
classification and the set, not the measurements.