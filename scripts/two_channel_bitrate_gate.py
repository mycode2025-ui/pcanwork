#!/usr/bin/env python3
"""Bidirectional Classical-CAN bitrate acceptance test for two adapters."""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import pcanwork  # noqa: E402


def wait_payload(session, channel, can_id, payload, minimum_count, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        frame = session.last(channel, can_id, dir="rx")
        if frame and frame.count >= minimum_count and frame.data == payload:
            return frame
        time.sleep(0.02)
    return None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--zlg-type", default="USBCAN-E-U")
    parser.add_argument("--rates", nargs="+", default=["125K", "250K", "500K", "1M"])
    parser.add_argument("--pcan-rate", help="Override PCAN rate for mismatch diagnostics")
    parser.add_argument("--termination", action="store_true",
                        help="Enable adapter-controlled 120-ohm termination where supported")
    parser.add_argument("--pcan-fd-api", action="store_true",
                        help="Initialize PCAN through CAN-FD API but transmit classic frames")
    parser.add_argument("--frames", type=int, default=10)
    parser.add_argument("--timeout", type=float, default=4.0)
    parser.add_argument("--report")
    args = parser.parse_args()

    session = pcanwork.Session("127.0.0.1", args.port, args.token, timeout=10)
    report = {"passed": False, "zlg_type": args.zlg_type, "rates": []}
    try:
        for rate_index, rate in enumerate(args.rates):
            pcan_rate = args.pcan_rate or rate
            result = {"rate": rate, "pcan_rate": pcan_rate, "passed": False}
            try:
                session.connect_devices(
                    [
                        pcanwork.dev("PCAN", sw_channel=1, channel_index=0,
                                     fd=args.pcan_fd_api, baud=pcan_rate,
                                     data_baud=pcan_rate,
                                     termination=args.termination),
                        pcanwork.dev(args.zlg_type, sw_channel=2, device_index=0,
                                     channel_index=0, baud=rate,
                                     termination=args.termination),
                    ],
                    wait=True,
                    timeout=8.0,
                )
                session.start()
                time.sleep(0.25)
                before_status = session.status()

                directions = []
                # Exercise ZLG TX first so a receive-side bitrate mismatch from
                # the PCAN direction cannot drive it Bus-Off before TX is checked.
                for source, destination, can_id in ((2, 1, 0x721), (1, 2, 0x720)):
                    payload = bytes([
                        0xA5, rate_index, source, destination,
                        args.frames & 0xFF, 0x5A, 0xC3, 0x3C,
                    ])
                    previous = session.last(destination, can_id, dir="rx")
                    previous_count = previous.count if previous else 0
                    queued = session.send_batch(
                        [{"ch": source, "id": can_id, "data": payload}],
                        repeat=args.frames,
                    )
                    received = wait_payload(
                        session, destination, can_id, payload,
                        previous_count + args.frames, args.timeout,
                    )
                    directions.append({
                        "source": source,
                        "destination": destination,
                        "queued": queued,
                        "received": 0 if received is None else received.count - previous_count,
                        "payload_ok": received is not None,
                    })

                after_status = session.status()
                before_channels = {row["ch"]: row for row in before_status.get("channels", [])}
                after_channels = {row["ch"]: row for row in after_status.get("channels", [])}
                channel_deltas = []
                for channel in (1, 2):
                    old = before_channels.get(channel, {})
                    new = after_channels.get(channel, {})
                    channel_deltas.append({
                        "ch": channel,
                        "tx": new.get("tx", 0) - old.get("tx", 0),
                        "rx": new.get("rx", 0) - old.get("rx", 0),
                        "err": new.get("err", 0) - old.get("err", 0),
                    })
                result.update({"directions": directions, "channels": channel_deltas})
                result["passed"] = (
                    all(item["queued"] == args.frames and item["payload_ok"]
                        and item["received"] >= args.frames for item in directions)
                    and all(item["err"] == 0 for item in channel_deltas)
                )
                if not result["passed"]:
                    result["capture_health"] = after_status.get("capture_health", {})
                    result["logs"] = session.logs()[-20:]
            except Exception as error:
                result["error"] = str(error)
                result["logs"] = session.logs()[-12:]
            finally:
                try:
                    session.stop()
                except Exception:
                    pass
                try:
                    session.disconnect()
                except Exception:
                    pass
                time.sleep(0.3)
            report["rates"].append(result)

        report["passed"] = all(item["passed"] for item in report["rates"])
        return 0 if report["passed"] else 1
    finally:
        session.close()
        text = json.dumps(report, ensure_ascii=False, indent=2)
        print(text)
        if args.report:
            path = pathlib.Path(args.report)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text + "\n", encoding="utf-8")


if __name__ == "__main__":
    raise SystemExit(main())
