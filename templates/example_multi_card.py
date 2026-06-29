#!/usr/bin/env python3
"""example_multi_card.py — drive MULTIPLE CAN cards from one test.

Run from PcanWork's Script Runner ("Python测试" → Run). Exit 0 = PASS (green).

This shows real multi-card operation: open several cards/channels at once and
send DIFFERENT traffic on each, routed by the sw_channel you assign. Defaults to
real CAN channels; replace with your hardware settings below.

Two ways to open multiple cards:
  A) Script-defined  — pcan.connect_devices([dev(...), dev(...)])   (used below)
  B) GUI-configured  — set up the cards in the app's 设备 dialog (add/clone rows),
                       then  pcan.connect_configured()   opens exactly those.
"""
import sys
import pcanwork

# Each card gets a distinct sw_channel; that's the number you pass as send(ch=...).
# Replace with your hardware, e.g.:
#   pcanwork.dev("PCAN",          sw_channel=1, channel_index=0, baud="500K")
#   pcanwork.dev("USBCANFD-200U", sw_channel=2, channel_index=0, fd=True,
#                baud="500K", data_baud="2M")
DEVICES = [
    pcanwork.dev("PCAN", sw_channel=1, channel_index=0, baud="500K"),
    pcanwork.dev("PCAN", sw_channel=2, channel_index=1, baud="250K"),  # ← adjust for your hardware
]


def main() -> int:
    with pcanwork.connect() as pcan:
        # 1) Open all cards at once.
        ok = pcan.connect_devices(DEVICES, timeout=4.0)
        # (alternative: n = pcan.connect_configured()  -> opens the 设备-dialog list)
        pcan.assert_true(ok, f"opened {len(DEVICES)} card(s)")
        pcan.start()

        # 2) Send DIFFERENT one-shot messages on each card.
        pcan.send(ch=1, id=0x100, data=bytes([0x11, 0x11]))   # card 1
        pcan.send(ch=2, id=0x200, data=bytes([0x22, 0x22]))   # card 2

        # 3) Periodic, different per card (independent handles). Continuous
        #    (repeat=-1) so the listener below always catches one; stopped at end.
        pcan.set_periodic(handle=1, ch=1, id=0x180,
                          data=bytes([0xA1]), period_ms=50, repeat=-1)
        pcan.set_periodic(handle=2, ch=2, id=0x280,
                          data=bytes([0xB2]), period_ms=50, repeat=-1)

        # 4) Verify each card carries ITS OWN traffic (routing is isolated).
        pcan.sleep(0.3)
        tx1 = pcan.last(ch=1, id=0x100, dir="tx")
        tx2 = pcan.last(ch=2, id=0x200, dir="tx")
        pcan.assert_true(tx1 is not None and tx1.data == bytes([0x11, 0x11]),
                         "card 1 sent 0x100")
        pcan.assert_true(tx2 is not None and tx2.data == bytes([0x22, 0x22]),
                         "card 2 sent 0x200")
        # 0x200 was sent on card 2 only — it must NOT show up on card 1.
        pcan.assert_true(pcan.last(ch=1, id=0x200, dir="tx") is None,
                         "card 1 is isolated from card 2's traffic")

        # 5) Per-card periodic tx echoes on the event stream.
        e1 = pcan.wait_for(ch=1, id=0x180, predicate=lambda f: f.tx, timeout=2.0)
        e2 = pcan.wait_for(ch=2, id=0x280, predicate=lambda f: f.tx, timeout=2.0)
        pcan.assert_true(e1.tx and e1.ch == 1, "card 1 periodic 0x180 on ch1")
        pcan.assert_true(e2.tx and e2.ch == 2, "card 2 periodic 0x280 on ch2")
        pcan.stop_periodic(handle=1)
        pcan.stop_periodic(handle=2)

        st = pcan.status()
        pcan.log(f"status: connected={st['connected']} tx={st['tx']} rx={st['rx']}")
        pcan.assert_true(st["tx"] >= 2, "both cards transmitted")

        return pcan.report()


if __name__ == "__main__":
    sys.exit(main())
