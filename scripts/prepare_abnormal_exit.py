#!/usr/bin/env python3
"""Put all four CAN channels under load, then leave the app owning the devices."""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import pcanwork  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--token", required=True)
    parser.add_argument("--report", required=True)
    args = parser.parse_args()

    session = pcanwork.Session("127.0.0.1", args.port, args.token, timeout=10)
    try:
        if session.connect_configured(wait=True, timeout=15) != 4:
            raise AssertionError("configured hardware did not open all four channels")
        session.start()
        frames = [
            {
                "ch": channel,
                "id": 0x720 + channel,
                "data": [channel, sequence, 0xA5, 0x5A, 0, 0, 0, 0],
            }
            for channel in range(1, 5)
            for sequence in range(20)
        ]
        before = session.status()
        if session.send_batch(frames) != len(frames):
            raise AssertionError("abnormal-exit preparation batch was not fully accepted")
        deadline = time.monotonic() + 10
        after = before
        while time.monotonic() < deadline:
            after = session.status()
            if after["tx"] - before["tx"] >= 80 and after["rx"] - before["rx"] >= 240:
                break
            time.sleep(0.05)
        else:
            raise AssertionError(f"traffic did not complete before forced exit: {after}")
        pathlib.Path(args.report).write_text(
            json.dumps({"prepared": True, "before": before, "after": after}, indent=2),
            encoding="utf-8",
        )
        return 0
    finally:
        # Deliberately do not stop or disconnect. The parent force-terminates the
        # owning application to verify that driver handles recover on restart.
        session.close()


if __name__ == "__main__":
    raise SystemExit(main())
