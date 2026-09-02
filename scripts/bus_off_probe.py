#!/usr/bin/env python3
"""Prove that a deliberately mismatched ZLG channel reports and recovers Bus-Off."""

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
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--channel", type=int, default=2)
    parser.add_argument("--report", required=True)
    args = parser.parse_args()
    session = pcanwork.Session("127.0.0.1", args.port, args.token, timeout=10)
    result = {"passed": False, "channel": args.channel}
    try:
        if session.connect_configured(wait=True, timeout=15) != 4:
            raise AssertionError("fault configuration did not open all four channels")
        session.start()
        frames = [
            {
                "ch": args.channel,
                "id": 0x760,
                "data": [sequence & 0xFF, 0xBA, 0xD0, 0xFF, 0, 0, 0, 0],
            }
            for sequence in range(256)
        ]
        session.send_batch(frames, repeat=8)
        deadline = time.monotonic() + 20
        status = session.status()
        logs = []
        while time.monotonic() < deadline:
            status = session.status()
            logs = session.logs()[-200:]
            text = "\n".join(logs).lower()
            actionable = "bus-off" in text or "总线错误" in text or "错误被动" in text
            if int(status["capture_health"].get("hardware_errors", 0)) > 0 and actionable:
                result.update({"passed": True, "status": status, "logs": logs})
                return 0
            time.sleep(0.1)
        raise AssertionError(f"Bus-Off was not reported with an actionable error: {status}")
    except Exception as error:
        result.update({"error": str(error), "status": session.status(), "logs": session.logs()[-200:]})
        return 1
    finally:
        try:
            session.stop()
            session.disconnect()
        except Exception:
            pass
        session.close()
        pathlib.Path(args.report).write_text(
            json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8"
        )


if __name__ == "__main__":
    raise SystemExit(main())
