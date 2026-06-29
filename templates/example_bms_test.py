#!/usr/bin/env python3
"""example_bms_test.py — a complete PcanWork automation test (single card).

Run from PcanWork's Script Runner ("Python测试" → Run). Exit 0 = PASS (green),
non-zero = FAIL (red). Plain Python — loops, ifs, asserts.

THE SCRIPT DEFINES THE DEVICE AND DBC (the runner only picks the interpreter +
script). Edit the two spots below for your bench:
  • DEVICE — open a real CAN card (required, virtual bus removed):
        pcan.connect_device("PCAN", channel_index=0, baud="500K")
    For multiple cards see example_multi_card.py (connect_devices / connect_configured).
  • DBC — the app auto-loads a sample DBC; load your own with
        pcan.load_dbc(r"C:\\path\\to\\your.dbc")
    or from the main window's 加载DBC button. Then assert on YOUR signal names.
"""
import sys
import pcanwork

# Signals to assert on (edit for your DBC/message). Defaults match the DBC the
# app auto-loads (message 0x100) and a frame the simulator emits, so this passes
# out-of-the-box. If the loaded DBC doesn't define DBC_MESSAGE, the test adapts
# to the first message that DBC defines.
DBC_MESSAGE = 0x100
DBC_SIGNALS = ["New_Signal_1", "New_Signal_1_Copy_2"]


def open_bus(pcan) -> None:
    """Open the device for this test. Edit here for real hardware."""
    if pcan.status()["connected"]:
        return
    pcan.connect_device("PCAN", channel_index=0, baud="500K")  # ← adjust for your hardware
    pcan.wait_connected(timeout=3.0)
    pcan.start()


def main() -> int:
    with pcanwork.connect() as pcan:
        # Optionally load your DBC (else the app's auto-loaded one is used):
        # pcan.load_dbc(r"C:\\path\\to\\your.dbc")

        # 1) Open the bus.
        open_bus(pcan)
        pcan.assert_true(pcan.status()["connected"], "bus is open")

        # 2) Discover what the loaded DBC defines — real signal names.
        info = pcan.dbc_info()
        pcan.log(f"DBC defines {len(info)} message(s): "
                 + ", ".join(f'0x{m["id"]:X}/{m["name"]}' for m in info[:8]))
        msg_id = DBC_MESSAGE
        if not any(m["id"] == msg_id for m in info) and info:
            msg_id = info[0]["id"]
            pcan.log(f"0x{DBC_MESSAGE:X} not in this DBC — using 0x{msg_id:X}")
        names = pcan.signals_of(msg_id)
        pcan.assert_true(len(names) > 0,
                         f"DBC knows 0x{msg_id:X} ({len(names)} signals)")

        # 3) Await a live frame and DECODE its signals by name.
        try:
            frame = pcan.wait_for(ch=1, id=msg_id, timeout=2.0)
            decoded = pcan.decode(msg_id, frame.data)        # {name: physical}
            pcan.log(f"decoded 0x{msg_id:X}: " + ", ".join(
                f"{k}={v:g}" for k, v in list(decoded.items())[:8]))
            pcan.assert_true(len(decoded) > 0,
                             f"decoded signals off live 0x{msg_id:X}")
            for sig in DBC_SIGNALS:
                if sig in names:
                    pcan.assert_true(sig in decoded, f"signal {sig} decoded")
            first = DBC_SIGNALS[0] if DBC_SIGNALS[0] in names else names[0]
            val = pcan.signal(ch=1, id=msg_id, name=first, dir="rx")
            pcan.assert_true(val is not None, f"{first} readable via signal() ({val})")
        except pcanwork.TimeoutError_:
            pcan.log(f"no live 0x{msg_id:X} on this bus within 2s "
                     f"(DBC recognition still verified via dbc_info/signals_of)")

        # 4) TX path: send a frame and verify our own transmission (poll tx cache).
        pcan.send(ch=1, id=0x123, data=bytes([0x01, 0x02, 0x03, 0x04]))
        sent = None
        for _ in range(40):                       # up to ~2 s
            sent = pcan.last(ch=1, id=0x123, dir="tx")
            if sent is not None:
                break
            pcan.sleep(0.05)
        pcan.assert_true(sent is not None and sent.count >= 1,
                         "0x123 was transmitted (tx cache)")
        if sent is not None:
            pcan.assert_eq(sent.data, bytes([0x01, 0x02, 0x03, 0x04]),
                           "0x123 tx payload")

        # 5) Periodic heartbeat: 5 sends @ 50 ms; wait_for its tx echo.
        pcan.set_periodic(handle=1001, ch=1, id=0x700,
                          data=bytes([0xAA]), period_ms=50, repeat=5)
        echo = pcan.wait_for(ch=1, id=0x700,
                             predicate=lambda f: f.tx, timeout=2.0)
        pcan.assert_true(echo.tx, "0x700 tx echo delivered to event stream")

        # 6) Counters sanity.
        st = pcan.status()
        pcan.assert_true(st["tx"] >= 1, "tx counter advanced")
        pcan.assert_true(st["rx"] >= 1, "rx counter advanced (live frames)")

        return pcan.report()  # 0 = all asserts passed, 1 = some failed


if __name__ == "__main__":
    sys.exit(main())
