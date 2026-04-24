#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "aiohttp>=3.9",
# ]
# ///
"""Load-test harness for a deployed rustyhip Lambda.

Drives a mixed read/write workload at a configurable concurrency for a fixed
duration, then reports per-op latency percentiles and throughput. Output is a
human-readable summary to stderr plus a structured JSON report to stdout (or a
file with --output).

Purpose (issue monkut/rustyhip#1): establish the baseline write-QPS capacity of
a single-writer RCE=1 Lambda so the "trigger for revisiting" number becomes
well-defined — today it's TBD in the issue body.

Usage:
    uv run scripts/loadtest_rustyhip.py \\
        --url https://abc123.execute-api.ap-northeast-1.amazonaws.com \\
        --token $RUSTYHIP_AUTH_TOKEN \\
        --duration-s 60 --concurrency 8 --write-ratio 0.5

Local (floci) smoke:
    just rustyhip-dev  # in another terminal
    uv run scripts/loadtest_rustyhip.py --url http://localhost:9000 --duration-s 10

The harness is self-contained:
  - creates/drops its own table (`loadtest_events`) so it does not disturb
    production data if pointed at a prod URL
  - bounds total request count with --max-requests (safety net vs. runaway cost)
  - measures per-op latency in milliseconds (INSERT vs SELECT separately)
  - reports p50/p90/p95/p99, request counts, error counts, error-code histogram
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import random
import statistics
import string
import sys
import time
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import aiohttp

SETUP_SQL = (
    "CREATE TABLE IF NOT EXISTS loadtest_events ("
    "  id INTEGER PRIMARY KEY AUTOINCREMENT,"
    "  ts INTEGER NOT NULL,"
    "  worker_id INTEGER NOT NULL,"
    "  payload TEXT NOT NULL"
    ")"
)
TEARDOWN_SQL = "DROP TABLE IF EXISTS loadtest_events"
INSERT_SQL = "INSERT INTO loadtest_events (ts, worker_id, payload) VALUES (?, ?, ?)"
SELECT_RECENT_SQL = "SELECT id, ts, worker_id FROM loadtest_events ORDER BY id DESC LIMIT ?"
SELECT_COUNT_SQL = "SELECT COUNT(*) AS n FROM loadtest_events WHERE worker_id = ?"


@dataclass
class Sample:
    op: str  # "insert" | "select_recent" | "select_count"
    elapsed_ms: float
    status: int
    error_code: str | None = None


@dataclass
class Report:
    started_at: float
    finished_at: float = 0.0
    samples: list[Sample] = field(default_factory=list)

    @property
    def duration_s(self) -> float:
        return max(0.0, self.finished_at - self.started_at)


def percentile(values: list[float], pct: float) -> float:
    """Linear-interpolated percentile (pct in [0, 100]). Empty → nan."""
    if not values:
        return math.nan
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    k = (len(ordered) - 1) * (pct / 100.0)
    lo = math.floor(k)
    hi = math.ceil(k)
    if lo == hi:
        return ordered[int(k)]
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (k - lo)


def random_payload(size_bytes: int) -> str:
    # printable ASCII — matches what a realistic JSON field would look like
    return "".join(random.choices(string.ascii_letters + string.digits, k=size_bytes))


async def post_sql(
    session: aiohttp.ClientSession,
    url: str,
    token: str | None,
    sql: str,
    params: list[Any],
    timeout_s: float,
) -> tuple[int, dict[str, Any] | None, str | None]:
    """Issue a single POST /sql; return (status, body_json, error_code)."""
    headers = {"content-type": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    payload = {"sql": sql, "params": params}
    try:
        async with session.post(
            f"{url.rstrip('/')}/sql",
            json=payload,
            headers=headers,
            timeout=aiohttp.ClientTimeout(total=timeout_s),
        ) as resp:
            status = resp.status
            body: dict[str, Any] | None
            try:
                body = await resp.json(content_type=None)
            except (aiohttp.ContentTypeError, json.JSONDecodeError):
                body = None
            err_code: str | None = None
            if status >= 400 and body is not None and isinstance(body.get("error"), dict):
                err_code = body["error"].get("code")
            elif status >= 400:
                err_code = f"HTTP_{status}"
            return status, body, err_code
    except asyncio.TimeoutError:
        return 0, None, "TIMEOUT"
    except aiohttp.ClientError as e:
        return 0, None, f"CLIENT_{type(e).__name__}"


async def setup(session: aiohttp.ClientSession, url: str, token: str | None, timeout_s: float) -> None:
    status, body, err = await post_sql(session, url, token, SETUP_SQL, [], timeout_s)
    if status != 200:
        raise RuntimeError(f"setup failed: status={status} err={err} body={body}")


async def teardown(session: aiohttp.ClientSession, url: str, token: str | None, timeout_s: float) -> None:
    await post_sql(session, url, token, TEARDOWN_SQL, [], timeout_s)


async def worker(
    worker_id: int,
    session: aiohttp.ClientSession,
    url: str,
    token: str | None,
    deadline: float,
    write_ratio: float,
    payload_bytes: int,
    timeout_s: float,
    report: Report,
    request_budget: list[int],
) -> None:
    rng = random.Random(worker_id)
    while True:
        if request_budget[0] <= 0:
            return
        if time.monotonic() >= deadline:
            return
        request_budget[0] -= 1

        roll = rng.random()
        t0 = time.monotonic()
        if roll < write_ratio:
            ts = int(time.time())
            payload = random_payload(payload_bytes)
            status, _body, err = await post_sql(
                session, url, token, INSERT_SQL, [ts, worker_id, payload], timeout_s
            )
            op = "insert"
        elif roll < write_ratio + (1.0 - write_ratio) / 2.0:
            status, _body, err = await post_sql(
                session, url, token, SELECT_RECENT_SQL, [10], timeout_s
            )
            op = "select_recent"
        else:
            status, _body, err = await post_sql(
                session, url, token, SELECT_COUNT_SQL, [worker_id], timeout_s
            )
            op = "select_count"
        elapsed_ms = (time.monotonic() - t0) * 1000.0
        report.samples.append(Sample(op=op, elapsed_ms=elapsed_ms, status=status, error_code=err))


def summarize(report: Report) -> dict[str, Any]:
    by_op: dict[str, list[Sample]] = {}
    for s in report.samples:
        by_op.setdefault(s.op, []).append(s)

    def stats(samples: list[Sample]) -> dict[str, Any]:
        if not samples:
            return {"count": 0}
        successful = [s.elapsed_ms for s in samples if s.status == 200]
        errs = [s for s in samples if s.status != 200]
        err_hist = Counter(s.error_code for s in errs)
        return {
            "count": len(samples),
            "ok_count": len(successful),
            "error_count": len(errs),
            "error_rate": round(len(errs) / len(samples), 4),
            "qps": round(len(samples) / report.duration_s, 2) if report.duration_s > 0 else 0,
            "latency_ms": {
                "mean": round(statistics.fmean(successful), 2) if successful else None,
                "p50": round(percentile(successful, 50), 2) if successful else None,
                "p90": round(percentile(successful, 90), 2) if successful else None,
                "p95": round(percentile(successful, 95), 2) if successful else None,
                "p99": round(percentile(successful, 99), 2) if successful else None,
                "max": round(max(successful), 2) if successful else None,
            },
            "error_codes": dict(err_hist),
        }

    return {
        "duration_s": round(report.duration_s, 2),
        "total_requests": len(report.samples),
        "total_qps": round(len(report.samples) / report.duration_s, 2) if report.duration_s > 0 else 0,
        "by_op": {op: stats(samples) for op, samples in sorted(by_op.items())},
    }


def human_summary(summary: dict[str, Any]) -> str:
    lines = [
        f"duration: {summary['duration_s']}s",
        f"total: {summary['total_requests']} requests, {summary['total_qps']} QPS",
        "",
    ]
    for op, s in summary["by_op"].items():
        if s["count"] == 0:
            continue
        latency = s["latency_ms"]
        lines.append(f"[{op}] n={s['count']} qps={s['qps']} err={s['error_count']} ({s['error_rate']*100:.2f}%)")
        if latency["p50"] is not None:
            lines.append(
                f"    latency ms — p50={latency['p50']} p95={latency['p95']} p99={latency['p99']} max={latency['max']}"
            )
        if s["error_codes"]:
            lines.append(f"    errors: {s['error_codes']}")
    return "\n".join(lines)


async def run(args: argparse.Namespace) -> int:
    connector = aiohttp.TCPConnector(limit=args.concurrency * 2)
    async with aiohttp.ClientSession(connector=connector) as session:
        print(f"setup: CREATE TABLE IF NOT EXISTS loadtest_events", file=sys.stderr)
        await setup(session, args.url, args.token, args.timeout_s)

        report = Report(started_at=time.monotonic())
        deadline = report.started_at + args.duration_s
        request_budget = [args.max_requests]
        print(
            f"running: {args.concurrency} workers for {args.duration_s}s "
            f"(write_ratio={args.write_ratio}, max_requests={args.max_requests})",
            file=sys.stderr,
        )
        tasks = [
            asyncio.create_task(
                worker(
                    worker_id=i,
                    session=session,
                    url=args.url,
                    token=args.token,
                    deadline=deadline,
                    write_ratio=args.write_ratio,
                    payload_bytes=args.payload_bytes,
                    timeout_s=args.timeout_s,
                    report=report,
                    request_budget=request_budget,
                )
            )
            for i in range(args.concurrency)
        ]
        await asyncio.gather(*tasks)
        report.finished_at = time.monotonic()

        if args.teardown:
            print("teardown: DROP TABLE IF EXISTS loadtest_events", file=sys.stderr)
            await teardown(session, args.url, args.token, args.timeout_s)

    summary = summarize(report)
    summary["config"] = {
        "url": args.url,
        "concurrency": args.concurrency,
        "duration_s": args.duration_s,
        "write_ratio": args.write_ratio,
        "payload_bytes": args.payload_bytes,
        "max_requests": args.max_requests,
        "timeout_s": args.timeout_s,
    }
    print(human_summary(summary), file=sys.stderr)
    payload = json.dumps(summary, indent=2, sort_keys=True)
    if args.output is None:
        sys.stdout.write(payload + "\n")
    else:
        args.output.write_text(payload + "\n", encoding="utf-8")
        print(f"wrote {args.output}", file=sys.stderr)
    return 0


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--url", required=True, help="Base URL of the rustyhip deployment (e.g. http://localhost:9000).")
    p.add_argument("--token", default=None, help="Bearer token for Authorization header. Required if deployment has RUSTYHIP_AUTH_TOKEN set.")
    p.add_argument("--concurrency", type=int, default=4, help="Parallel async workers.")
    p.add_argument("--duration-s", type=float, default=30.0, help="Wall-clock runtime.")
    p.add_argument("--max-requests", type=int, default=100_000, help="Hard cap on total requests (cost safety net).")
    p.add_argument("--write-ratio", type=float, default=0.5, help="Fraction of ops that are writes (0..1). Remainder split evenly between the two SELECT shapes.")
    p.add_argument("--payload-bytes", type=int, default=64, help="INSERT payload size.")
    p.add_argument("--timeout-s", type=float, default=30.0, help="Per-request timeout.")
    p.add_argument("--no-teardown", dest="teardown", action="store_false", default=True, help="Leave the loadtest_events table in place after running.")
    p.add_argument("-o", "--output", type=Path, default=None, help="Write JSON report to this path (default: stdout).")
    args = p.parse_args(argv)
    if not (0.0 <= args.write_ratio <= 1.0):
        p.error("--write-ratio must be in [0, 1]")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    return asyncio.run(run(args))


if __name__ == "__main__":
    raise SystemExit(main())
