#!/usr/bin/env python3
"""Verify a second PcanWork process cannot silently steal occupied CAN devices."""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
from datetime import datetime, timezone

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
    report = {
        "passed": False,
        "generated_at": datetime.now(timezone.utc).isoformat(),
    }
    try:
        try:
            connected = session.connect_configured(wait=True, timeout=8)
        except Exception as error:
            logs = session.logs()[-100:]
            text = str(error)
            actionable = any(
                token in (text + "\n" + "\n".join(logs)).lower()
                for token in ("占用", "busy", "initialize", "open", "连接失败", "无法")
            )
            if not actionable:
                raise AssertionError(f"occupancy error is not actionable: {error}; logs={logs}")
            report.update({"passed": True, "error": text, "logs": logs})
            return 0
        else:
            raise AssertionError(
                f"second process unexpectedly opened {connected} occupied channels"
            )
    except Exception as error:
        report.update({"error": str(error), "logs": session.logs()[-100:]})
        return 1
    finally:
        try:
            session.disconnect()
        except Exception:
            pass
        session.close()
        path = pathlib.Path(args.report).resolve()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    raise SystemExit(main())
