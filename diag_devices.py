#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""设备诊断: 把"打开"与"发送"解耦, 判断 200U 到底有没有打开。"""
import sys
import time
import queue
import threading
import pcanwork


def wait_conn(pc, t=3.0):
    d = time.monotonic() + t
    while time.monotonic() < d:
        if pc.status().get("connected"):
            return True
        time.sleep(0.1)
    return False


def main():
    pc = pcanwork.connect()
    with pc:
        # 1) 仅 200U —— connected 为真即证明 200U 单独能打开(与 PCAN/发送无关)
        pc.connect_devices([pcanwork.dev("USBCANFD-200U", sw_channel=1, channel_index=0,
                                         baud="500K", data_baud="2M", fd=True)], wait=False)
        c200 = wait_conn(pc)
        print(f"[仅 200U ] connected={c200}  -> {'★打开成功' if c200 else '打开失败'}")
        pc.disconnect()
        time.sleep(0.3)

        # 2) 仅 PCAN
        pc.connect_devices([pcanwork.dev("PCAN", sw_channel=1, channel_index=0, baud="500K")], wait=False)
        cpcan = wait_conn(pc)
        print(f"[仅 PCAN ] connected={cpcan}  -> {'★打开成功' if cpcan else '打开失败'}")
        pc.disconnect()
        time.sleep(0.3)

        # 3) 两卡同开, 看 ch2 发送: tx 增量 + 回显通道号
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

        pc.connect_devices([
            pcanwork.dev("PCAN", sw_channel=1, channel_index=0, baud="500K"),
            pcanwork.dev("USBCANFD-200U", sw_channel=2, channel_index=0,
                         baud="500K", data_baud="2M", fd=True),
        ], wait=False)
        both = wait_conn(pc)
        pc.start()
        time.sleep(0.4)
        print(f"[两卡  ] connected={both}")

        t0 = pc.status()["tx"]
        pc.send(2, 0x222, bytes(range(8)), fd=True, brs=True)
        time.sleep(0.7)
        t1 = pc.status()["tx"]
        echo222 = [(f.ch, f.tx) for f in frames if f.id == 0x222]
        print(f"  发 ch2(FD 0x222) 后: tx {t0}->{t1}  回显[id=0x222]={echo222}")

        # 经典帧再试一次
        t2 = pc.status()["tx"]
        pc.send(2, 0x223, bytes([1, 2, 3]))
        time.sleep(0.7)
        t3 = pc.status()["tx"]
        echo223 = [(f.ch, f.tx) for f in frames if f.id == 0x223]
        print(f"  发 ch2(经典 0x223) 后: tx {t2}->{t3}  回显[id=0x223]={echo223}")

        print()
        print("解读:")
        print("  · 若 [仅 200U] connected=True  => 200U 打开链路完全正常(多卡支持成立)")
        print("  · 两卡时 ch2 发送 tx 不增 且 无回显 => 已打开但 ZCAN_Transmit 判失败")
        print("    (典型原因: 总线上无对端 ACK / 未接 120Ω 终端电阻 → 帧发不上总线)")
        print("  · 若回显出现 ch=1 => 那才是'200U 没开、回退到 CAN1'")
        stop.set()
        return 0


if __name__ == "__main__":
    sys.exit(main())
