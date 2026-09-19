#!/usr/bin/env python3
"""Fail when proxy decision code reads the clock directly instead of the seam.

Rule: decision code -- code that compares a stored instant/timestamp against
"now" or measures a duration against a threshold -- must read the current
instant through the `common::clock::Clock` seam, never `Instant::now()`,
`SystemTime::now()`, `Utc::now()`, or `.elapsed()`. The seam keeps the boundary
(an exact instant, a rearm after a deadline) reachable from a test; a direct
read re-hides it behind wall time. The only production sites allowed to read
time directly are the clock implementation itself (`common/src/clock.rs`) and
legitimate non-decision uses: logging/metrics timestamps, session start/end
stamps, a pure duration measurement with no threshold, and tokio's own timer
primitives.

This checker scans `common/src`, `protocol/src`, and `server/src` (test
modules and `tests.rs` files are skipped) for those tokens and fails on any
line not recorded in ALLOWLIST. Each entry is `(path, snippet, count, reason)`:
`snippet` must appear in the flagged line and exactly `count` lines may match
it, so a line that moves is fine but an added direct read -- even a byte-for-
byte duplicate of an allowed line -- is caught. An unmigrated decision site is
listed with reason `unmigrated: needs <X>`.

Run from the proxy repo root:

    python3 tools/check-clock-seam.py
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SOURCE_ROOT = REPO
SOURCE_DIRS = ("common/src", "protocol/src", "server/src")

# A direct read of the current time, or a duration measured from a stored
# instant. Matches the tokens named in the audit rule.
TIME_READ = re.compile(
    r"Instant::now|SystemTime::now|Utc::now|Local::now|Zoned::now"
    r"|OffsetDateTime::now|\.elapsed\(\)"
)

# (relative path, line snippet, expected occurrence count, reason).
#
# Categories:
#   (a) the clock implementation itself
#   (c) a legitimate non-decision use (timestamp, metric, pure measurement,
#       tokio timer primitive)
#   (b) a decision site that belongs behind the seam but has not been migrated;
#       the reason must say what migration would require
ALLOWLIST: list[tuple[str, str, int, str]] = [
    (
        "common/src/clock.rs",
        "Instant::now()",
        1,
        "(a) the SystemClock implementation of the Clock seam itself",
    ),
    (
        "common/src/proxy_runtime/client/stream.rs",
        "Instant::now()",
        2,
        "(c) RTT-probe start/end measurement, no threshold",
    ),
    (
        "common/src/proxy_runtime/client/udp.rs",
        "Instant::now()",
        2,
        "(c) RTT-probe start/end measurement, no threshold",
    ),
    (
        "common/src/proxy_runtime/client/udp.rs",
        "SystemTime::now()",
        1,
        "(c) route-confirmation wall timestamp",
    ),
    (
        "common/src/proxy_runtime/client/udp.rs",
        ".elapsed()",
        1,
        "(c) RouteConfirmation::is_fresh wall-clock term (SystemTime "
        "semantics the monotonic Clock seam does not model); the monotonic "
        "term is on the seam",
    ),
    (
        "common/src/proxy_runtime/conn_handler/stream.rs",
        "Instant::now()",
        1,
        "(c) ConnContext start timestamp",
    ),
    (
        "common/src/proxy_runtime/metrics/stream.rs",
        "SystemTime::now()",
        1,
        "(c) live-duration rendering",
    ),
    (
        "common/src/proxy_runtime/metrics/stream.rs",
        "Instant::now()",
        1,
        "(c) gauge rate baseline for rendering",
    ),
    (
        "common/src/proxy_runtime/metrics/udp.rs",
        "SystemTime::now()",
        1,
        "(c) live-duration rendering",
    ),
    (
        "common/src/proxy_runtime/metrics/udp.rs",
        "Instant::now()",
        1,
        "(c) gauge rate baseline for rendering",
    ),
    (
        "common/src/proxy_runtime/relay/copy/timed_copy_bidirectional.rs",
        "Instant::now()",
        1,
        "(c) copy end timestamp (timeout backdate), no threshold",
    ),
    (
        "common/src/proxy_runtime/relay/copy/timeout_stream.rs",
        "Instant::now()",
        3,
        "(c) tokio timer primitive (tokio::time::Instant, virtual-time aware)",
    ),
    (
        "common/src/proxy_runtime/relay/stream/mod.rs",
        "SystemTime::now()",
        2,
        "(c) session start/end timestamps",
    ),
    (
        "common/src/proxy_runtime/relay/udp.rs",
        "start: SystemTime::now(),",
        2,
        "(c) UDP session start timestamp",
    ),
    (
        "common/src/proxy_runtime/relay/udp.rs",
        "session.end = Some(SystemTime::now())",
        1,
        "(c) UDP session end timestamp",
    ),
    (
        "common/src/proxy_runtime/relay/udp.rs",
        "Instant::now()",
        8,
        "(b) flow-idle timeout and crypto-warn throttle decisions "
        "(unmigrated: needs a Clock through copy_bidirectional_udp) + (c) "
        "activity/session timestamps",
    ),
    (
        "protocol/src/reverse_tunnel/responder.rs",
        ".elapsed()",
        2,
        "(c) uptime rendering in session_stats",
    ),
    (
        "protocol/src/reverse_tunnel/responder.rs",
        "Instant::now()",
        2,
        "(c) tunnel registration timestamp",
    ),
    (
        "protocol/src/socks5/server/tcp.rs",
        "Instant::now()",
        2,
        "(c) ConnContext start timestamp",
    ),
    (
        "protocol/src/stream_proto/streams/http_tunnel/proxy.rs",
        "Instant::now()",
        1,
        "(c) copy end timestamp at :160",
    ),
    (
        "protocol/src/stream_proto/streams/http_tunnel/proxy.rs",
        "SystemTime::now()",
        1,
        "(c) session end timestamp",
    ),
    (
        "protocol/src/stream_proto/streams/http_tunnel/upstream.rs",
        "Instant::now()",
        1,
        "(c) upstream start timestamp",
    ),
    (
        "protocol/src/stream_proto/streams/tcp/access_server.rs",
        "Instant::now()",
        1,
        "(c) ConnContext start timestamp",
    ),
]


def test_spans(text: str) -> list[tuple[int, int]]:
    """Line ranges [start, end] covered by `#[cfg(test)]`-attributed items."""
    lines = text.splitlines()
    spans: list[tuple[int, int]] = []
    for index, raw in enumerate(lines):
        marker = raw.find("#[cfg(test)]")
        if marker == -1:
            continue
        # Find the item that follows the attribute; it may share the line.
        if raw[marker + len("#[cfg(test)]") :].strip():
            item = index
        else:
            item = index + 1
            while item < len(lines) and (
                not lines[item].strip()
                or lines[item].lstrip().startswith("#[")
                or lines[item].lstrip().startswith("//")
            ):
                item += 1
        if item >= len(lines):
            continue
        end = item
        if "{" in lines[item]:
            depth = 0
            cursor = item
            started = False
            while cursor < len(lines):
                for char in lines[cursor]:
                    if char == "{":
                        depth += 1
                        started = True
                    elif char == "}":
                        depth -= 1
                end = cursor
                cursor += 1
                if started and depth <= 0:
                    break
        else:
            while end < len(lines) and ";" not in lines[end]:
                end += 1
        spans.append((index, end))
    return spans


def production_lines(path: Path) -> list[tuple[int, str]]:
    """Lines of `path` outside any `#[cfg(test)]` item, 1-indexed."""
    text = path.read_text(encoding="utf-8")
    spans = test_spans(text)
    out: list[tuple[int, str]] = []
    for number, line in enumerate(text.splitlines(), start=1):
        if any(start < number <= end for start, end in spans):
            continue
        out.append((number, line))
    return out


def source_files() -> list[Path]:
    files: list[Path] = []
    for directory in SOURCE_DIRS:
        root = SOURCE_ROOT / directory
        for path in sorted(root.rglob("*.rs")):
            if path.name == "tests.rs" or path.parent.name == "tests":
                continue
            files.append(path)
    return files


def strip_comment(line: str) -> str:
    """The code part of `line`, with a trailing `//` comment removed."""
    return line.split("//", 1)[0]


def scan() -> list[tuple[str, int, str, str]]:
    """All direct time reads in production code: (path, line, text, snippet)."""
    hits: list[tuple[str, int, str, str]] = []
    for path in source_files():
        relative = path.relative_to(SOURCE_ROOT).as_posix()
        for number, line in production_lines(path):
            match = TIME_READ.search(strip_comment(line))
            if match is not None:
                hits.append((relative, number, line, match.group(0)))
    return hits


def main() -> int:
    hits = scan()
    bad = False
    budget: list[int] = [count for _path, _snippet, count, _reason in ALLOWLIST]
    for path, number, line, _token in hits:
        allowed = False
        for index, (allow_path, snippet, _count, _reason) in enumerate(ALLOWLIST):
            if path == allow_path and snippet in line and budget[index] > 0:
                budget[index] -= 1
                allowed = True
                break
        if not allowed:
            print(f"DIRECT TIME READ: {path}:{number}: {line.strip()}")
            print(
                "  rule: decision code must use the Clock seam "
                "(common/src/clock.rs); a non-decision use belongs in the "
                "allowlist with a reason"
            )
            bad = True
    for index, (path, snippet, count, reason) in enumerate(ALLOWLIST):
        if budget[index] != 0:
            print(
                f"STALE allowlist entry {path!r} ({snippet!r}): expected "
                f"{count} matching line(s), found {count - budget[index]}"
            )
            print(f"  reason: {reason}")
            bad = True
    if bad:
        print(
            f"\nclock-seam guard failed: {len(hits)} direct time read(s) in "
            f"production code",
            file=sys.stderr,
        )
        return 1
    print(
        f"clock-seam guard OK: {len(hits)} direct time read(s), all allowlisted"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
