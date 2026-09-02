#!/usr/bin/env python3
"""PcanWork four-channel hardware acceptance gate.

Verifies:
  * exactly four configured software channels open;
  * all 12 directed channel pairs carry a real DBC frame;
  * one IPC batch submits a configurable multi-channel pressure burst;
  * TX/RX totals, per-channel counters and every explicit loss/error counter agree.

The pressure phase uses a distinct CAN ID per transmitter. Sending different
payloads with the same ID from several nodes at once is an invalid CAN test: it
causes a physical-layer bit error after arbitration and correctly drives nodes
error-passive.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import pcanwork  # noqa: E402


def wait_counters(session, base, tx_delta, rx_delta, timeout):
    deadline = time.monotonic() + timeout
    current = session.status()
    while time.monotonic() < deadline:
        current = session.status()
        if current["tx"] - base["tx"] >= tx_delta and current["rx"] - base["rx"] >= rx_delta:
            break
        time.sleep(0.05)
    return current


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int)
    parser.add_argument("--token")
    parser.add_argument("--dbc", default=str(pathlib.Path.home() / "Desktop/EU_HVBOXCheck/HVBoxCheck_EU.dbc"))
    parser.add_argument("--message-id", type=lambda value: int(value, 0), default=0x200)
    parser.add_argument("--signal", default="TBattBox")
    parser.add_argument("--extended", dest="extended", action="store_true", default=True)
    parser.add_argument("--standard", dest="extended", action="store_false")
    parser.add_argument("--frames-per-channel", type=int, default=100)
    parser.add_argument("--timeout", type=float, default=12.0)
    parser.add_argument("--report")
    args = parser.parse_args()

    session = (
        pcanwork.Session("127.0.0.1", args.port, args.token or "", timeout=10)
        if args.port is not None
        else pcanwork.connect(timeout=10)
    )
    report = {"passed": False, "matrix": [], "pressure": {}}
    try:
        session.load_dbc(args.dbc)
        diagnostics = session.dbc_diagnostics()
        report["dbc_diagnostics"] = diagnostics["summary"]
        configured = session.connect_configured(wait=True, timeout=args.timeout)
        if configured != 4:
            raise AssertionError(f"expected 4 configured channels, got {configured}")
        session.start()
        time.sleep(0.3)

        for source in range(1, 5):
            physical = float(source * 25)
            payload = session.encode(
                args.message_id, {args.signal: physical}, ext=args.extended
            )
            before = {}
            for destination in range(1, 5):
                if destination != source:
                    frame = session.last(
                        destination, args.message_id, dir="rx", ext=args.extended
                    )
                    before[destination] = frame.count if frame else 0
            queued = session.send_batch(
                [{
                    "ch": source,
                    "id": args.message_id,
                    "data": payload,
                    "ext": args.extended,
                }],
                repeat=3,
            )
            if queued != 3:
                raise AssertionError(f"CAN{source}: queued {queued}, expected 3")
            pending = set(before)
            deadline = time.monotonic() + 4.0
            while pending and time.monotonic() < deadline:
                for destination in list(pending):
                    frame = session.last(
                        destination, args.message_id, dir="rx", ext=args.extended
                    )
                    if not frame or frame.count <= before[destination] or frame.data != payload:
                        continue
                    decoded = session.decode(args.message_id, frame.data, ext=args.extended)
                    if abs(decoded[args.signal] - physical) <= 1e-9:
                        report["matrix"].append({
                            "source": source,
                            "destination": destination,
                            "received": frame.count - before[destination],
                        })
                        pending.remove(destination)
                if pending:
                    time.sleep(0.02)
            if pending:
                raise AssertionError(f"CAN{source} missing destinations {sorted(pending)}")

        time.sleep(0.5)
        base = session.status()
        frames = []
        for sequence in range(args.frames_per_channel):
            for channel in range(1, 5):
                frames.append({
                    "ch": channel,
                    "id": 0x500 + channel,
                    "data": bytes([channel, sequence & 0xFF, 0xA5, 0x5A]),
                })
        started = time.perf_counter()
        queued = session.send_batch(frames)
        submit_ms = (time.perf_counter() - started) * 1000.0
        expected_tx = args.frames_per_channel * 4
        expected_rx = expected_tx * 3
        current = wait_counters(session, base, expected_tx, expected_rx, args.timeout)
        base_channels = {row["ch"]: row for row in base["channels"]}
        current_channels = {row["ch"]: row for row in current["channels"]}
        per_channel = []
        for channel in range(1, 5):
            per_channel.append({
                "ch": channel,
                "tx": current_channels[channel]["tx"] - base_channels[channel]["tx"],
                "rx": current_channels[channel]["rx"] - base_channels[channel]["rx"],
                "err": current_channels[channel]["err"] - base_channels[channel]["err"],
            })
        health = current["capture_health"]
        report["pressure"] = {
            "queued": queued,
            "submit_ms": round(submit_ms, 3),
            "tx": current["tx"] - base["tx"],
            "rx": current["rx"] - base["rx"],
            "per_channel": per_channel,
            "health": health,
            "timestamp_quality": current["timestamp_quality"],
        }
        if queued != expected_tx or report["pressure"]["tx"] != expected_tx:
            raise AssertionError("pressure TX count mismatch")
        if report["pressure"]["rx"] != expected_rx:
            raise AssertionError("pressure RX count mismatch")
        if any(
            row["tx"] != args.frames_per_channel
            or row["rx"] != args.frames_per_channel * 3
            or row["err"] != 0
            for row in per_channel
        ):
            raise AssertionError(f"per-channel counter mismatch: {per_channel}")
        for key in (
            "dropped_frames",
            "dropped_events",
            "hardware_overruns",
            "hardware_errors",
            "command_rejected",
        ):
            if health[key] != 0:
                raise AssertionError(f"capture_health.{key}={health[key]}")
        if len(report["matrix"]) != 12:
            raise AssertionError(f"matrix contains {len(report['matrix'])}/12 directions")
        report["passed"] = True
        return 0
    except Exception as error:
        report["error"] = str(error)
        report["logs"] = session.logs()[-30:]
        return 1
    finally:
        try:
            session.stop()
        except Exception:
            pass
        try:
            session.disconnect()
        except Exception:
            pass
        session.close()
        text = json.dumps(report, ensure_ascii=False, indent=2)
        print(text)
        if args.report:
            pathlib.Path(args.report).write_text(text + "\n", encoding="utf-8")


if __name__ == "__main__":
    raise SystemExit(main())
