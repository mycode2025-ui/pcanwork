#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""两卡接同一总线的端到端测试: PCAN(CAN1) <-> USBCANFD-200U(CAN2), 经典 CAN 500K。

两卡互为对端 → 发送能被对方 ACK → 发送成功; 且一方发的帧另一方应收到(跨卡 RX)。
验证:
  1. 两卡同时连上。
  2. CAN1 发 0x100 -> 自身回显 ch1 + CAN2 收到(跨卡 1->2)。
  3. CAN2 发 0x200 -> 自身回显 ch2(此前失败的发送现在应成功) + CAN1 收到(跨卡 2->1)。
"""
import sys
import time
import queue
import threading
import pcanwork


def main():
    pc = pcanwork.connect()
    with pc:
        frames = []
        stop = threading.Event()
        q = queue.Queue()
        with pc._evt_lock:
            pc._evt_subs.append((set(), (lambda f: True), q))

        def drain():
            while not stop.is_set():
                try:
                    frames.append(q.get(timeout=0.2))
                except queue.Empty:
                    pass
        threading.Thread(target=drain, daemon=True).start()

        def has(ch, msg_id, tx):
            return [f for f in frames if f.ch == ch and f.id == msg_id and f.tx == tx]

        # 两卡都用经典 500K
        pc.connect_devices([
            pcanwork.dev("PCAN",          sw_channel=1, channel_index=0, baud="500K", fd=False),
            pcanwork.dev("USBCANFD-200U", sw_channel=2, channel_index=0, baud="500K", fd=False),
        ], wait=False)
        deadline = time.monotonic() + 4.0
        while time.monotonic() < deadline and not pc.status().get("connected"):
            time.sleep(0.1)
        st = pc.status()
        print("=== 连接后 status:", st)
        pc.assert_true(st.get("connected"), "两卡连接(connected)")
        pc.start()
        time.sleep(0.4)

        # --- CAN1 -> 总线 ---
        data1 = bytes([0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88])
        pc.send(1, 0x100, data1)
        time.sleep(0.6)
        echo1 = has(1, 0x100, True)
        rx_on2 = has(2, 0x100, False)
        pc.assert_true(bool(echo1), "CAN1 发送成功(本通道回显)")
        pc.assert_true(bool(rx_on2), "跨卡 CAN1 -> CAN2: 200U 收到 0x100")
        if rx_on2:
            ok = rx_on2[0].data == data1
            pc.assert_true(ok, f"CAN2 收到数据与发送一致 ({rx_on2[0].data.hex()})")

        # --- CAN2 -> 总线 (此前无对端时失败, 现在应成功) ---
        data2 = bytes([0xDE, 0xAD, 0xBE, 0xEF])
        t0 = pc.status()["tx"]
        pc.send(2, 0x200, data2)
        time.sleep(0.6)
        t1 = pc.status()["tx"]
        echo2 = has(2, 0x200, True)
        rx_on1 = has(1, 0x200, False)
        pc.assert_true(bool(echo2), f"CAN2(200U) 发送成功(本通道回显, tx {t0}->{t1})")
        pc.assert_true(bool(rx_on1), "跨卡 CAN2 -> CAN1: PCAN 收到 0x200")
        if rx_on1:
            ok = rx_on1[0].data == data2
            pc.assert_true(ok, f"CAN1 收到数据与发送一致 ({rx_on1[0].data.hex()})")

        # --- 周期发送小压测: CAN1 每 10ms 发一帧, 看 CAN2 持续接收 ---
        before2 = sum(1 for f in frames if f.ch == 2 and not f.tx)
        pc.set_periodic(handle=1, ch=1, id=0x321, data=bytes([1, 2, 3, 4]), period_ms=10, repeat=50)
        time.sleep(1.0)
        after2 = sum(1 for f in frames if f.ch == 2 and not f.tx)
        got = after2 - before2
        print(f"=== 周期压测: CAN1 发 50 帧@10ms, CAN2 收到 {got} 帧")
        pc.assert_true(got >= 40, f"周期帧跨卡接收 >=40 (实收 {got})")

        rx1 = sum(1 for f in frames if f.ch == 1 and not f.tx)
        rx2 = sum(1 for f in frames if f.ch == 2 and not f.tx)
        print(f"=== 总采集={len(frames)}  CAN1 RX={rx1}  CAN2 RX={rx2}")
        print("=== 最终 status:", pc.status())
        stop.set()
        return pc.report()


if __name__ == "__main__":
    sys.exit(main())
