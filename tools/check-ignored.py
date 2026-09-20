#!/usr/bin/env python3
"""Verify the proxy opt-in ignored-test inventory in GATE.md.

`cargo test` silently skips every `#[ignore]`d test, so the opt-in set and its
classification is recorded in GATE.md. This script re-derives that set from the
workspace members' source trees and exits non-zero when the manifest and
reality disagree, so an ignored test can never be added, removed, renamed, or
reclassified without the gate documentation being updated.

The inventory has one of two honest classifications:

- `perf` — an *asserting* test kept opt-in because it is an end-to-end
  throughput benchmark (e.g. the 32 MiB bulk relay through a real proxy
  chain) whose assertion is a byte-exactness integrity check on an
  inherently slow, wall-clock-measured run. It is a real gate — it fails on
  a relay regression — but it is too slow and too load-sensitive for the
  default suite. The checker requires a `perf` entry's body to contain an
  assertion token, so the benchmark cannot silently lose its integrity
  check while staying ignored.
- `unrunnable` — a test whose triggering condition cannot be produced on
  this host. The only such test waits for a real OS suspend/resume
  notification; the decision rule it exercises is already pinned by the
  clock-seam tests in the same module. The checker requires the `#[ignore]`
  reason to state why it cannot run, so the constraint stays visible.

Files under `target/` are not scanned. The assertion-token set matches
netem_test's `tools/check-gate.py`, including the debug-only forms; the brace
counting is the same regex-level body extraction that harness uses, so all the
gates agree on what a function body is.

Usage:
    python3 tools/check-ignored.py
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MANIFEST = REPO / "GATE.md"
# Workspace members whose `src/` trees hold the crate code under test.
MEMBER_ROOTS = ("common", "protocol", "server", "tests")

CLASSIFICATIONS = {"perf", "unrunnable"}
ASSERTION_TOKENS = re.compile(
    r"(debug_assert_ne!|debug_assert_eq!|debug_assert!|assert_ne!|assert_eq!|assert!|panic!|unreachable!)"
)
IGNORE_RE = re.compile(r"#\[ignore\s*(?:=\s*\"([^\"]*)\")?\s*\]")
FN_RE = re.compile(
    r"\b(?:pub\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?fn\s+([A-Za-z0-9_]+)\s*(?:<[^>]*>)?\s*\("
)


def manifest_block(name: str) -> str | None:
    """Return the body of the ```<name> fenced block, or None."""
    text = MANIFEST.read_text(encoding="utf-8")
    block = re.search(rf"```{re.escape(name)}\n(.*?)```", text, re.S)
    return block.group(1) if block else None


def manifest_entries() -> dict[str, str]:
    block = manifest_block("ignored-manifest")
    if block is None:
        sys.exit(f"{MANIFEST}: no ```ignored-manifest block found")
    entries: dict[str, str] = {}
    for raw in block.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        name, _, classification = line.partition(" = ")
        name, classification = name.strip(), classification.strip()
        if classification not in CLASSIFICATIONS:
            sys.exit(f"{MANIFEST}: {name} has unknown classification {classification!r}")
        if name in entries:
            sys.exit(f"{MANIFEST}: duplicate entry {name}")
        entries[name] = classification
    return entries


def fn_body(text: str, start: int) -> str:
    """Brace-balanced body starting at ``start`` (the index of `{`)."""
    depth = 0
    idx = start
    while idx < len(text):
        char = text[idx]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                break
        idx += 1
    return text[start : idx + 1]


def ignored_tests() -> dict[str, tuple[str, str]]:
    """Map `relpath::fn` -> (ignore reason, body) for every `#[ignore]`d test."""
    found: dict[str, tuple[str, str]] = {}
    for root in MEMBER_ROOTS:
        for path in sorted((REPO / root).rglob("*.rs")):
            if "target" in path.parts:
                continue
            text = path.read_text(encoding="utf-8")
            for match in IGNORE_RE.finditer(text):
                fn_match = FN_RE.search(text, match.end())
                if fn_match is None:
                    sys.exit(f"{path}: #[ignore] attribute not followed by fn")
                start = text.find("{", fn_match.end())
                if start == -1:
                    sys.exit(f"{path}: fn {fn_match.group(1)} has no body")
                rel = path.relative_to(REPO).as_posix()
                found[f"{rel}::{fn_match.group(1)}"] = (
                    match.group(1) or "",
                    fn_body(text, start),
                )
    return found


def main() -> int:
    manifest = manifest_entries()
    actual = ignored_tests()

    bad = False
    missing = sorted(actual.keys() - manifest.keys())
    stale = sorted(manifest.keys() - actual.keys())
    if missing or stale:
        for name in missing:
            print(f"UNCLASSIFIED ignored test (add to GATE.md ignored-manifest): {name}")
        for name in stale:
            print(f"STALE GATE.md entry (no longer #[ignore]d): {name}")
        bad = True

    for name, classification in sorted(manifest.items()):
        if name not in actual:
            continue
        reason, body = actual[name]
        tokens = ASSERTION_TOKENS.findall(body)
        if classification == "perf":
            if not tokens:
                print(
                    f"perf {name} contains no assertion token: it has silently "
                    f"stopped being a gate (reclassify as unrunnable or restore "
                    f"the assertion)"
                )
                bad = True
        elif classification == "unrunnable":
            if not reason:
                print(
                    f"unrunnable {name} has an #[ignore] reason that does not "
                    f"state why it cannot run"
                )
                bad = True

    if bad:
        print(
            f"\nmanifest has {len(manifest)} entries, source reports "
            f"{len(actual)} #[ignore]d tests; update GATE.md",
            file=sys.stderr,
        )
        return 1

    print(f"ignored-test manifest OK: {len(actual)} #[ignore]d tests classified")
    for classification in sorted(CLASSIFICATIONS):
        count = sum(1 for c in manifest.values() if c == classification)
        print(f"  {classification}: {count}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())