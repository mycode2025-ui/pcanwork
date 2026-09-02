#!/usr/bin/env python3
"""Long-running four-channel hardware stability gate for PcanWork."""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import time
from datetime import datetime, timezone

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import pcanwork  # noqa: E402


LOSS_FIELDS = (
    "dropped_frames",
    "dropped_events",
    "hardware_overruns",
    "hardware_errors",
    "command_rejected",
)


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def append_jsonl(path: pathlib.Path, value: dict) -> None:
    with path.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(value, ensure_ascii=False) + "\n")


def wait_delta(session, before: dict, tx: int, rx: int, timeout: float) -> dict:
    deadline = time.monotonic() + timeout
    current = session.status()
    while time.monotonic() < deadline:
        current = session.status()
        if current["tx"] - before["tx"] >= tx and current["rx"] - before["rx"] >= rx:
            return current
        time.sleep(0.05)
    raise AssertionError(
        f"traffic timeout: TX {current['tx'] - before['tx']}/{tx}, "
        f"RX {current['rx'] - before['rx']}/{rx}"
    )


def validate_health(status: dict) -> None:
    health = status["capture_health"]
    bad = {field: int(health.get(field, 0)) for field in LOSS_FIELDS if health.get(field, 0)}
    if bad:
        raise AssertionError(f"loss/error counters are non-zero: {bad}")
    channels = status.get("channels", [])
    if {int(row["ch"]) for row in channels} != {1, 2, 3, 4}:
        raise AssertionError(f"four-channel set changed: {channels}")
    per_channel_errors = {
        int(row["ch"]): int(row.get("err", 0))
        for row in channels
        if int(row.get("err", 0)) != 0
    }
    if per_channel_errors:
        raise AssertionError(f"per-channel errors are non-zero: {per_channel_errors}")
    timestamp = status.get("timestamp_quality", {})
    if int(timestamp.get("monotonic_violations", 0)) != 0:
        raise AssertionError(f"hardware timestamps are non-monotonic: {timestamp}")


def validate_channel_deltas(before: dict, after: dict, burst_per_channel: int) -> None:
    before_rows = {int(row["ch"]): row for row in before.get("channels", [])}
    after_rows = {int(row["ch"]): row for row in after.get("channels", [])}
    if set(before_rows) != {1, 2, 3, 4} or set(after_rows) != {1, 2, 3, 4}:
        raise AssertionError(
            f"cannot prove per-channel traffic: before={before_rows} after={after_rows}"
        )
    expected_rx = burst_per_channel * 3
    failures = {}
    for channel in range(1, 5):
        tx_delta = int(after_rows[channel]["tx"]) - int(before_rows[channel]["tx"])
        rx_delta = int(after_rows[channel]["rx"]) - int(before_rows[channel]["rx"])
        if tx_delta != burst_per_channel or rx_delta != expected_rx:
            failures[channel] = {
                "tx": tx_delta,
                "expected_tx": burst_per_channel,
                "rx": rx_delta,
                "expected_rx": expected_rx,
            }
    if failures:
        raise AssertionError(f"per-channel traffic mismatch: {failures}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--hours", type=float, default=8.0)
    parser.add_argument("--burst-per-channel", type=int, default=20)
    parser.add_argument("--interval-seconds", type=float, default=1.0)
    parser.add_argument("--reconnect-minutes", type=float, default=30.0)
    parser.add_argument("--report", required=True)
    args = parser.parse_args()

    report_path = pathlib.Path(args.report).resolve()
    report_path.parent.mkdir(parents=True, exist_ok=True)
    progress_path = report_path.with_suffix(".jsonl")
    if progress_path.exists():
        progress_path.unlink()

    session = pcanwork.Session("127.0.0.1", args.port, args.token, timeout=10)
    started = time.monotonic()
    deadline = started + args.hours * 3600.0
    reconnect_period = max(60.0, args.reconnect_minutes * 60.0)
    next_reconnect = started + reconnect_period
    iterations = 0
    reconnects = 0
    total_tx = 0
    total_rx = 0
    final_status = {}
    result = {
        "passed": False,
        "started_at": utc_now(),
        "requested_hours": args.hours,
        "iterations": 0,
        "reconnects": 0,
    }
    try:
        if session.connect_configured(wait=True, timeout=15) != 4:
            raise AssertionError("configured hardware did not open all four channels")
        session.start()
        time.sleep(0.3)
        frames = []
        for channel in range(1, 5):
            for sequence in range(args.burst_per_channel):
                frames.append(
                    {
                        "ch": channel,
                        "id": 0x600 + channel,
                        "ext": False,
                        "fd": False,
                        "brs": False,
                        "remote": False,
                        "data": [channel, sequence & 0xFF, (sequence >> 8) & 0xFF, 0, 0, 0, 0, 0],
                    }
                )
        expected_tx = len(frames)
        expected_rx = expected_tx * 3

        while time.monotonic() < deadline:
            if time.monotonic() >= next_reconnect:
                session.stop()
                session.disconnect()
                time.sleep(0.5)
                if session.connect_configured(wait=True, timeout=15) != 4:
                    raise AssertionError("four-channel reconnect failed")
                session.start()
                time.sleep(0.3)
                reconnects += 1
                next_reconnect = time.monotonic() + reconnect_period

            before = session.status()
            queued = session.send_batch(frames)
            if queued != expected_tx:
                raise AssertionError(f"batch accepted {queued}/{expected_tx} frames")
            final_status = wait_delta(session, before, expected_tx, expected_rx, 10.0)
            validate_health(final_status)
            validate_channel_deltas(before, final_status, args.burst_per_channel)
            total_tx += final_status["tx"] - before["tx"]
            total_rx += final_status["rx"] - before["rx"]
            iterations += 1
            if iterations == 1 or iterations % 60 == 0:
                append_jsonl(
                    progress_path,
                    {
                        "at": utc_now(),
                        "elapsed_seconds": round(time.monotonic() - started, 3),
                        "iterations": iterations,
                        "reconnects": reconnects,
                        "tx": total_tx,
                        "rx": total_rx,
                        "capture_health": final_status["capture_health"],
                        "timestamp_quality": final_status["timestamp_quality"],
                    },
                )
            remaining = deadline - time.monotonic()
            if remaining > 0:
                time.sleep(min(args.interval_seconds, remaining))

        result.update(
            {
                "passed": True,
                "completed_at": utc_now(),
                "elapsed_seconds": round(time.monotonic() - started, 3),
                "iterations": iterations,
                "reconnects": reconnects,
                "tx": total_tx,
                "rx": total_rx,
                "final_status": final_status,
            }
        )
        return 0
    except Exception as error:  # product gate must preserve actionable evidence
        result.update(
            {
                "completed_at": utc_now(),
                "elapsed_seconds": round(time.monotonic() - started, 3),
                "iterations": iterations,
                "reconnects": reconnects,
                "tx": total_tx,
                "rx": total_rx,
                "error": str(error),
                "logs": session.logs()[-100:],
                "final_status": final_status,
            }
        )
        return 1
    finally:
        try:
            session.stop()
            session.disconnect()
        except Exception:
            pass
        session.close()
        report_path.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    raise SystemExit(main())
