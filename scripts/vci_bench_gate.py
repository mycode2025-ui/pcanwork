#!/usr/bin/env python3
"""GCAN USBCAN-I + CANalyst-II two-channel hardware acceptance gate."""

from __future__ import annotations

import argparse
import ctypes
import json
import pathlib
import sys
import time
from datetime import datetime, timezone

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import pcanwork  # noqa: E402


DEVICES = [
    pcanwork.dev("GCAN", sw_channel=1, device_index=0, channel_index=0, baud="500K"),
    pcanwork.dev("ZHCX", sw_channel=2, device_index=0, channel_index=0, baud="500K"),
    pcanwork.dev("ZHCX", sw_channel=3, device_index=0, channel_index=1, baud="500K"),
]
CHANNELS = (1, 2, 3)
LOSS_FIELDS = (
    "dropped_frames",
    "dropped_events",
    "hardware_overruns",
    "hardware_errors",
    "command_rejected",
)


class ProcessMemoryCounters(ctypes.Structure):
    _fields_ = [
        ("cb", ctypes.c_uint32),
        ("PageFaultCount", ctypes.c_uint32),
        ("PeakWorkingSetSize", ctypes.c_size_t),
        ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
        ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t),
        ("PeakPagefileUsage", ctypes.c_size_t),
    ]


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def process_metrics(pid: int) -> dict[str, int]:
    if not pid:
        return {}
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    kernel32.OpenProcess.argtypes = [ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32]
    kernel32.OpenProcess.restype = ctypes.c_void_p
    kernel32.GetProcessHandleCount.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint32)]
    kernel32.GetProcessHandleCount.restype = ctypes.c_int
    kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
    kernel32.CloseHandle.restype = ctypes.c_int
    psapi.GetProcessMemoryInfo.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(ProcessMemoryCounters),
        ctypes.c_uint32,
    ]
    psapi.GetProcessMemoryInfo.restype = ctypes.c_int
    handle = kernel32.OpenProcess(0x0400 | 0x0010, False, pid)
    if not handle:
        raise OSError(ctypes.get_last_error(), f"OpenProcess({pid})")
    try:
        count = ctypes.c_uint32()
        if not kernel32.GetProcessHandleCount(handle, ctypes.byref(count)):
            raise OSError(ctypes.get_last_error(), "GetProcessHandleCount")
        memory = ProcessMemoryCounters()
        memory.cb = ctypes.sizeof(memory)
        if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(memory), memory.cb):
            raise OSError(ctypes.get_last_error(), "GetProcessMemoryInfo")
        return {"handles": int(count.value), "working_set": int(memory.WorkingSetSize)}
    finally:
        kernel32.CloseHandle(handle)


def connect(session) -> None:
    if not session.connect_devices(DEVICES, wait=True, timeout=12):
        raise AssertionError("three-channel VCI connection failed")
    session.start()
    time.sleep(0.15)


def disconnect(session) -> None:
    session.stop()
    session.disconnect()


def wait_delta(session, before: dict, tx: int, rx: int, timeout: float = 8.0) -> dict:
    deadline = time.monotonic() + timeout
    status = session.status()
    while time.monotonic() < deadline:
        status = session.status()
        if status["tx"] - before["tx"] >= tx and status["rx"] - before["rx"] >= rx:
            return status
        time.sleep(0.02)
    raise AssertionError(
        f"traffic timeout TX={status['tx'] - before['tx']}/{tx} "
        f"RX={status['rx'] - before['rx']}/{rx}"
    )


def wait_channel_rows(session, timeout: float = 3.0) -> dict:
    deadline = time.monotonic() + timeout
    status = session.status()
    while time.monotonic() < deadline:
        status = session.status()
        if {int(row["ch"]) for row in status.get("channels", [])} == set(CHANNELS):
            return status
        time.sleep(0.02)
    raise AssertionError(f"channel statistics not ready: {status.get('channels', [])}")


def validate_health(status: dict) -> None:
    actual = {int(row["ch"]) for row in status.get("channels", [])}
    if actual != set(CHANNELS):
        raise AssertionError(f"channel set changed: {sorted(actual)}")
    health = status["capture_health"]
    bad = {name: int(health.get(name, 0)) for name in LOSS_FIELDS if health.get(name, 0)}
    if bad:
        raise AssertionError(f"capture/driver errors: {bad}")
    channel_errors = {
        int(row["ch"]): int(row.get("err", 0))
        for row in status["channels"]
        if int(row.get("err", 0))
    }
    if channel_errors:
        raise AssertionError(f"per-channel errors: {channel_errors}")
    timestamp = status.get("timestamp_quality", {})
    if int(timestamp.get("monotonic_violations", 0)):
        raise AssertionError(f"non-monotonic hardware timestamps: {timestamp}")


def matrix(session) -> list[dict]:
    result = []
    for source in CHANNELS:
        frame_id = 0x680 + source
        payload = bytes([0xA0 + source, 1, 2, 3, 4, 5, 6, 7])
        before = {}
        for destination in CHANNELS:
            if destination != source:
                current = session.last(destination, frame_id, dir="rx")
                before[destination] = current.count if current else 0
        session.send(ch=source, id=frame_id, data=payload)
        pending = set(before)
        deadline = time.monotonic() + 3.0
        while pending and time.monotonic() < deadline:
            for destination in list(pending):
                current = session.last(destination, frame_id, dir="rx")
                if current and current.count > before[destination] and current.data == payload:
                    result.append({"source": source, "destination": destination})
                    pending.remove(destination)
            time.sleep(0.01)
        if pending:
            raise AssertionError(f"CAN{source} missing destinations {sorted(pending)}")
    if len(result) != 6:
        raise AssertionError(f"matrix contains {len(result)}/6 directions")
    return result


def pressure(session, frames_per_channel: int) -> dict:
    frames = []
    for sequence in range(frames_per_channel):
        for channel in CHANNELS:
            frames.append(
                {
                    "ch": channel,
                    "id": 0x690 + channel,
                    "data": [channel, sequence & 0xFF, (sequence >> 8) & 0xFF, 0xA5, 0x5A],
                }
            )
    before = wait_channel_rows(session)
    queued = session.send_batch(frames)
    expected_tx = frames_per_channel * len(CHANNELS)
    expected_rx = expected_tx * (len(CHANNELS) - 1)
    if queued != expected_tx:
        raise AssertionError(f"batch accepted {queued}/{expected_tx}")
    after = wait_delta(session, before, expected_tx, expected_rx)
    validate_health(after)
    before_rows = {int(row["ch"]): row for row in before["channels"]}
    after_rows = {int(row["ch"]): row for row in after["channels"]}
    if set(before_rows) != set(CHANNELS) or set(after_rows) != set(CHANNELS):
        raise AssertionError(
            f"incomplete pressure statistics before={sorted(before_rows)} "
            f"after={sorted(after_rows)}"
        )
    per_channel = []
    for channel in CHANNELS:
        tx = int(after_rows[channel]["tx"]) - int(before_rows[channel]["tx"])
        rx = int(after_rows[channel]["rx"]) - int(before_rows[channel]["rx"])
        if tx != frames_per_channel or rx != frames_per_channel * 2:
            raise AssertionError(f"CAN{channel} pressure mismatch TX={tx} RX={rx}")
        per_channel.append({"ch": channel, "tx": tx, "rx": rx})
    return {"queued": queued, "tx": expected_tx, "rx": expected_rx, "channels": per_channel}


def run_quick(session, args) -> dict:
    connect(session)
    result = {"matrix": matrix(session), "pressure": pressure(session, args.frames)}
    disconnect(session)
    return result


def run_cycles(session, args) -> dict:
    samples = []
    connect(session)
    matrix(session)
    disconnect(session)
    time.sleep(0.1)
    baseline = process_metrics(args.pid)
    for cycle in range(1, args.cycles + 1):
        connect(session)
        pressure(session, 2)
        disconnect(session)
        if cycle == 1 or cycle % 10 == 0 or cycle == args.cycles:
            time.sleep(0.05)
            samples.append({"cycle": cycle, **process_metrics(args.pid)})
    final = process_metrics(args.pid)
    handle_growth = final.get("handles", 0) - baseline.get("handles", 0)
    memory_growth = final.get("working_set", 0) - baseline.get("working_set", 0)
    if args.pid and handle_growth > 8:
        raise AssertionError(f"process handle growth {handle_growth} exceeds 8")
    if args.pid and memory_growth > 64 * 1024 * 1024:
        raise AssertionError(f"working-set growth {memory_growth} exceeds 64 MiB")
    return {
        "cycles": args.cycles,
        "baseline": baseline,
        "final": final,
        "handle_growth": handle_growth,
        "working_set_growth": memory_growth,
        "samples": samples,
    }


def run_soak(session, args, report_path: pathlib.Path) -> dict:
    progress_path = report_path.with_suffix(".jsonl")
    if progress_path.exists():
        progress_path.unlink()
    started = time.monotonic()
    deadline = started + args.hours * 3600.0
    next_reconnect = started + args.reconnect_minutes * 60.0
    iterations = reconnects = total_tx = total_rx = 0
    connect(session)
    matrix(session)
    while time.monotonic() < deadline:
        if time.monotonic() >= next_reconnect:
            disconnect(session)
            time.sleep(0.2)
            connect(session)
            reconnects += 1
            next_reconnect = time.monotonic() + args.reconnect_minutes * 60.0
        current = pressure(session, args.frames)
        iterations += 1
        total_tx += current["tx"]
        total_rx += current["rx"]
        if iterations == 1 or iterations % 60 == 0:
            with progress_path.open("a", encoding="utf-8") as stream:
                stream.write(
                    json.dumps(
                        {
                            "at": utc_now(),
                            "elapsed_seconds": round(time.monotonic() - started, 3),
                            "iterations": iterations,
                            "reconnects": reconnects,
                            "tx": total_tx,
                            "rx": total_rx,
                            "process": process_metrics(args.pid),
                        },
                        ensure_ascii=False,
                    )
                    + "\n"
                )
        remaining = deadline - time.monotonic()
        if remaining > 0:
            time.sleep(min(args.interval, remaining))
    disconnect(session)
    return {
        "hours": args.hours,
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "iterations": iterations,
        "reconnects": reconnects,
        "tx": total_tx,
        "rx": total_rx,
        "process": process_metrics(args.pid),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("quick", "cycles", "soak"))
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--pid", type=int, default=0)
    parser.add_argument("--frames", type=int, default=100)
    parser.add_argument("--cycles", type=int, default=100)
    parser.add_argument("--hours", type=float, default=8.0)
    parser.add_argument("--interval", type=float, default=1.0)
    parser.add_argument("--reconnect-minutes", type=float, default=30.0)
    parser.add_argument("--report", required=True)
    args = parser.parse_args()
    report_path = pathlib.Path(args.report).resolve()
    report_path.parent.mkdir(parents=True, exist_ok=True)
    result = {"passed": False, "mode": args.mode, "started_at": utc_now()}
    session = pcanwork.Session("127.0.0.1", args.port, args.token, timeout=12)
    try:
        if args.mode == "quick":
            details = run_quick(session, args)
        elif args.mode == "cycles":
            details = run_cycles(session, args)
        else:
            details = run_soak(session, args, report_path)
        result.update({"passed": True, "completed_at": utc_now(), "details": details})
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
            disconnect(session)
        except Exception:
            pass
        session.close()
        text = json.dumps(result, ensure_ascii=False, indent=2)
        report_path.write_text(text + "\n", encoding="utf-8")
        print(text)


if __name__ == "__main__":
    raise SystemExit(main())
