#!/usr/bin/env python3
"""Wait until PcanWork reports a deliberately disabled USB CAN adapter."""

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
    parser.add_argument("--report", required=True)
    args = parser.parse_args()
    session = pcanwork.Session("127.0.0.1", args.port, args.token, timeout=10)
    result = {"passed": False}
    status = {}
    logs = []
    try:
        deadline = time.monotonic() + 30
        status = session.status()
        while time.monotonic() < deadline:
            status = session.status()
            logs = session.logs()[-200:]
            text = "\n".join(logs).lower()
            actionable = any(
                marker in text
                for marker in ("usb", "已断开", "连接已丢失", "离线", "offline", "disconnect")
            )
            if not status.get("connected") and not status.get("channels") and actionable:
                result.update({"passed": True, "status": status, "logs": logs})
                return 0
            time.sleep(0.1)
        raise AssertionError(f"USB removal was not reported within 30 seconds: {status}")
    except Exception as error:
        result.update({"error": str(error), "status": status, "logs": logs})
        return 1
    finally:
        session.close()
        pathlib.Path(args.report).write_text(
            json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8"
        )


if __name__ == "__main__":
    raise SystemExit(main())
