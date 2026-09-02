#!/usr/bin/env python3
"""Validate GCAN/CANalyst-II hardware timestamp scaling on an attached CAN bus."""

from __future__ import annotations

import argparse
import json
import pathlib
import statistics
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


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def sample_receiver(session, receiver: int, interval: float, samples: int) -> dict:
    sender = 2 if receiver == 1 else 1
    frame_id = 0x720 + receiver
    timestamps = []
    wall_times = []
    for sequence in range(samples):
        session.send(
            ch=sender,
            id=frame_id,
            data=bytes([receiver, sequence, 0x54, 0x53, 0xA5, 0x5A]),
        )
        frame = session.wait_for(
            receiver,
            frame_id,
            predicate=lambda candidate, expected=sequence: (
                len(candidate.data) >= 2 and candidate.data[1] == expected
            ),
            timeout=2.0,
        )
        timestamps.append(float(frame.t))
        wall_times.append(time.monotonic())
        if sequence + 1 < samples:
            time.sleep(interval)
    deltas = [later - earlier for earlier, later in zip(timestamps, timestamps[1:])]
    wall_deltas = [later - earlier for earlier, later in zip(wall_times, wall_times[1:])]
    if any(delta <= 0 for delta in deltas):
        raise AssertionError(f"CAN{receiver} hardware timestamps are not monotonic: {deltas}")
    median = statistics.median(deltas)
    wall_median = statistics.median(wall_deltas)
    scale_ratio = median / wall_median
    if not 0.85 <= scale_ratio <= 1.15:
        raise AssertionError(
            f"CAN{receiver} timestamp scale mismatch: timestamp={median:.6f}s, "
            f"wall={wall_median:.6f}s, ratio={scale_ratio:.6f}"
        )
    return {
        "receiver": receiver,
        "sender": sender,
        "timestamps": timestamps,
        "deltas": deltas,
        "wall_deltas": wall_deltas,
        "median_delta_seconds": median,
        "wall_median_delta_seconds": wall_median,
        "scale_ratio": scale_ratio,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--interval", type=float, default=0.1)
    parser.add_argument("--samples", type=int, default=8)
    parser.add_argument("--receivers", default="1,2,3")
    parser.add_argument("--report", required=True)
    args = parser.parse_args()
    report_path = pathlib.Path(args.report).resolve()
    report_path.parent.mkdir(parents=True, exist_ok=True)
    result = {"passed": False, "started_at": utc_now()}
    session = pcanwork.Session("127.0.0.1", args.port, args.token, timeout=12)
    try:
        if not session.connect_devices(DEVICES, wait=True, timeout=12):
            raise AssertionError("three-channel VCI connection failed")
        session.start()
        time.sleep(0.2)
        receiver_channels = tuple(
            int(value.strip()) for value in args.receivers.split(",") if value.strip()
        )
        receivers = [
            sample_receiver(session, channel, args.interval, args.samples)
            for channel in receiver_channels
        ]
        result.update(
            {
                "passed": True,
                "completed_at": utc_now(),
                "expected_interval_seconds": args.interval,
                "receivers": receivers,
                "timestamp_quality": session.status().get("timestamp_quality", {}),
            }
        )
        return 0
    except Exception as error:
        result.update(
            {
                "completed_at": utc_now(),
                "error": str(error),
                "logs": session.logs()[-100:],
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
