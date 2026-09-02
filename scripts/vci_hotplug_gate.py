#!/usr/bin/env python3
"""Interactive USB hot-plug gate for GCAN and CANalyst-II."""

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
from scripts.vci_bench_gate import DEVICES, matrix, pressure, validate_health  # noqa: E402


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def wait_for_disconnect(session, timeout: float) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        status = session.status()
        if not status.get("connected") or not status.get("channels"):
            return status
        time.sleep(0.1)
    raise AssertionError("USB unplug was not detected before timeout")


def wait_for_reconnect(session, timeout: float) -> dict:
    deadline = time.monotonic() + timeout
    last_error = ""
    while time.monotonic() < deadline:
        try:
            if session.connect_devices(DEVICES, wait=True, timeout=12):
                session.start()
                time.sleep(0.2)
                status = session.status()
                validate_health(status)
                return status
        except Exception as error:
            last_error = str(error)
        time.sleep(1.0)
    raise AssertionError(f"USB replug did not reconnect before timeout: {last_error}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--target", choices=("gcan", "zhcx"), required=True)
    parser.add_argument("--unplug-timeout", type=float, default=180.0)
    parser.add_argument("--replug-timeout", type=float, default=180.0)
    parser.add_argument("--report", required=True)
    args = parser.parse_args()
    report_path = pathlib.Path(args.report).resolve()
    report_path.parent.mkdir(parents=True, exist_ok=True)
    result = {"passed": False, "target": args.target, "started_at": utc_now()}
    session = pcanwork.Session("127.0.0.1", args.port, args.token, timeout=12)
    try:
        if not session.connect_devices(DEVICES, wait=True, timeout=12):
            raise AssertionError("initial three-channel connection failed")
        session.start()
        time.sleep(0.2)
        before_matrix = matrix(session)
        for channel in (1, 2, 3):
            session.set_periodic(
                0x9000 + channel,
                channel,
                0x740 + channel,
                bytes([channel, 0x48, 0x4F, 0x54]),
                period_ms=50,
                repeat=-1,
            )
        disconnected = wait_for_disconnect(session, args.unplug_timeout)
        disconnect_logs = session.logs()[-100:]
        reconnected = wait_for_reconnect(session, args.replug_timeout)
        after_matrix = matrix(session)
        after_pressure = pressure(session, 50)
        result.update(
            {
                "passed": True,
                "completed_at": utc_now(),
                "before_matrix": before_matrix,
                "disconnect_status": disconnected,
                "disconnect_logs": disconnect_logs,
                "reconnect_status": reconnected,
                "after_matrix": after_matrix,
                "after_pressure": after_pressure,
            }
        )
        return 0
    except Exception as error:
        result.update(
            {
                "completed_at": utc_now(),
                "error": str(error),
                "logs": session.logs()[-200:],
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
        text = json.dumps(result, ensure_ascii=False, indent=2)
        report_path.write_text(text + "\n", encoding="utf-8")
        print(text)


if __name__ == "__main__":
    raise SystemExit(main())
