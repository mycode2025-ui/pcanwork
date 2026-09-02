#!/usr/bin/env python3
"""Interactive physical Bus-Off and automatic recovery gate."""

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
from scripts.vci_bench_gate import DEVICES, matrix, wait_channel_rows, wait_delta  # noqa: E402


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def write_phase(path: pathlib.Path, phase: str, **details) -> None:
    path.write_text(
        json.dumps({"at": utc_now(), "phase": phase, **details}, ensure_ascii=False, indent=2)
        + "\n",
        encoding="utf-8",
    )


def recovered_pressure(session, frames_per_channel: int) -> dict:
    frames = []
    for sequence in range(frames_per_channel):
        for channel in (1, 2, 3):
            frames.append(
                {
                    "ch": channel,
                    "id": 0x770 + channel,
                    "data": [channel, sequence & 0xFF, 0xB0, 0x5A],
                }
            )
    before = wait_channel_rows(session)
    health_before = dict(before["capture_health"])
    queued = session.send_batch(frames)
    expected_tx = frames_per_channel * 3
    expected_rx = expected_tx * 2
    after = wait_delta(session, before, expected_tx, expected_rx)
    before_rows = {int(row["ch"]): row for row in before["channels"]}
    after_rows = {int(row["ch"]): row for row in after["channels"]}
    per_channel = []
    for channel in (1, 2, 3):
        tx = int(after_rows[channel]["tx"]) - int(before_rows[channel]["tx"])
        rx = int(after_rows[channel]["rx"]) - int(before_rows[channel]["rx"])
        if tx != frames_per_channel or rx != frames_per_channel * 2:
            raise AssertionError(f"CAN{channel} recovery pressure TX={tx} RX={rx}")
        per_channel.append({"ch": channel, "tx": tx, "rx": rx})
    health_after = after["capture_health"]
    for field in ("dropped_frames", "dropped_events", "hardware_overruns", "command_rejected"):
        if int(health_after.get(field, 0)) != int(health_before.get(field, 0)):
            raise AssertionError(f"{field} increased after Bus-Off recovery")
    if int(health_after.get("hardware_errors", 0)) != int(health_before.get("hardware_errors", 0)):
        raise AssertionError("hardware errors continued increasing after Bus-Off recovery")
    return {"queued": queued, "tx": expected_tx, "rx": expected_rx, "channels": per_channel}


def wait_for_error_counters_to_settle(session, timeout: float = 30.0) -> dict:
    deadline = time.monotonic() + timeout
    previous = None
    stable_since = time.monotonic()
    status = session.status()
    while time.monotonic() < deadline:
        status = session.status()
        current = int(status["capture_health"].get("hardware_errors", 0))
        if current != previous:
            previous = current
            stable_since = time.monotonic()
        elif time.monotonic() - stable_since >= 2.0:
            return status
        time.sleep(0.25)
    raise AssertionError("hardware error counters did not settle after Bus-Off recovery")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--target-channel", type=int, default=3)
    parser.add_argument("--fault-timeout", type=float, default=300.0)
    parser.add_argument("--recovery-timeout", type=float, default=300.0)
    parser.add_argument("--report", required=True)
    args = parser.parse_args()
    report_path = pathlib.Path(args.report).resolve()
    report_path.parent.mkdir(parents=True, exist_ok=True)
    phase_path = report_path.with_suffix(".phase.json")
    result = {"passed": False, "started_at": utc_now(), "target_channel": args.target_channel}
    session = pcanwork.Session("127.0.0.1", args.port, args.token, timeout=12)
    periodic_handle = 0xB000 + args.target_channel
    try:
        if not session.connect_devices(DEVICES, wait=True, timeout=12):
            raise AssertionError("initial three-channel connection failed")
        session.start()
        time.sleep(0.2)
        before_matrix = matrix(session)
        session.set_periodic(
            periodic_handle,
            args.target_channel,
            0x760 + args.target_channel,
            bytes([0xB0, 0x5F, args.target_channel]),
            period_ms=10,
            repeat=-1,
        )
        write_phase(phase_path, "waiting_for_bus_off", target_channel=args.target_channel)
        fault_deadline = time.monotonic() + args.fault_timeout
        fault_logs = []
        while time.monotonic() < fault_deadline:
            fault_logs = session.logs()[-200:]
            if any("Bus-Off" in line for line in fault_logs):
                break
            time.sleep(0.2)
        else:
            raise AssertionError("Bus-Off was not reported before timeout")
        write_phase(phase_path, "bus_off_detected", logs=fault_logs[-50:])

        recovery_deadline = time.monotonic() + args.recovery_timeout
        last_error = ""
        after_matrix = None
        while time.monotonic() < recovery_deadline:
            try:
                after_matrix = matrix(session)
                break
            except Exception as error:
                last_error = str(error)
                time.sleep(1.0)
        if after_matrix is None:
            raise AssertionError(f"bus did not recover before timeout: {last_error}")
        session.stop_periodic(periodic_handle)
        settled_status = wait_for_error_counters_to_settle(session)
        pressure = recovered_pressure(session, 50)
        status = session.status()
        if not status.get("connected") or not status.get("running"):
            raise AssertionError("application disconnected instead of recovering Bus-Off")
        result.update(
            {
                "passed": True,
                "completed_at": utc_now(),
                "before_matrix": before_matrix,
                "fault_logs": fault_logs,
                "after_matrix": after_matrix,
                "settled_status": settled_status,
                "after_pressure": pressure,
                "status": status,
            }
        )
        write_phase(phase_path, "completed")
        return 0
    except Exception as error:
        result.update(
            {"completed_at": utc_now(), "error": str(error), "logs": session.logs()[-200:]}
        )
        write_phase(phase_path, "failed", error=str(error))
        return 1
    finally:
        try:
            session.stop_periodic(periodic_handle)
            session.stop()
            session.disconnect()
        except Exception:
            pass
        session.close()
        report_path.write_text(
            json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )


if __name__ == "__main__":
    raise SystemExit(main())
