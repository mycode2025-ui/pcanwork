#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""真实硬件多卡测试: PCAN(经典 CAN) + USBCANFD-200U(CAN FD) 同时打开。

通过 IPC 直连正在运行的 PcanWork(端口/token 由 PCANWORK_IPC_PORT/TOKEN 环境变量给出)。
验证三件事:
  1. 两张卡作为独立软件通道 CAN1 / CAN2 同时连上(connected)。
  2. 逐通道存活: 在 CAN1 发经典帧、在 CAN2 发 FD 帧, 各自回显应带本通道号
     (若某通道没真正打开, 发送会回退到 CAN1 → 对应通道收不到回显, 即判失败)。
  3. 跨总线(仅当两卡接同一总线时): CAN1 发的帧能被 CAN2 收到, 反之亦然。
     不接在一起则跳过, 不算失败。
另外采集并报告每通道自发 RX 帧数(总线上若有其它节点在跑)。
"""
import sys
import time
import queue
import threading

import pcanwork


def main():
    pc = pcanwork.connect()  # 读环境变量 PCANWORK_IPC_PORT / TOKEN
    with pc:
        # ---- catch-all 采集线程: 先注册订阅, 彻底避开 send→wait_for 竞态 ----
        cap_q: "queue.Queue" = queue.Queue()
        token = (set(), (lambda f: True), cap_q)
        with pc._evt_lock:
            pc._evt_subs.append(token)
        frames = []
        stop = threading.Event()

        def drain():
            while not stop.is_set():
                try:
                    frames.append(cap_q.get(timeout=0.2))
                except queue.Empty:
                    pass

        dt = threading.Thread(target=drain, daemon=True)
        dt.start()

        def echoed(ch, msg_id):
            return any(f.ch == ch and f.id == msg_id and f.tx for f in frames)

        def rx_on(ch, msg_id):
            return any(f.ch == ch and f.id == msg_id and not f.tx for f in frames)

        print("=== 连接前 status:", pc.status())

        # ---- 连接两张卡 ----
        pc.connect_devices([
            pcanwork.dev("PCAN",          sw_channel=1, channel_index=0, baud="500K", fd=False),
            pcanwork.dev("USBCANFD-200U", sw_channel=2, channel_index=0, baud="500K",
                         data_baud="2M", fd=True),
        ], wait=False)

        # 轮询 connected(最多 4s)
        deadline = time.monotonic() + 4.0
        while time.monotonic() < deadline and not pc.status().get("connected"):
            time.sleep(0.1)
        st = pc.status()
        print("=== 连接后 status:", st)
        pc.assert_true(st.get("connected"), "至少一个通道连上")

        pc.start()
        time.sleep(0.4)

        # ---- 逐通道存活检测 ----
        pc.send(1, 0x111, bytes([0x11, 0x22, 0x33, 0x44]))                  # CAN1 经典
        time.sleep(0.5)
        ch1_alive = echoed(1, 0x111)

        # CAN2 发 16 字节 FD 帧
        fd_payload = bytes(range(0x10))  # 16 bytes -> 必为 FD
        ch2_alive = False
        ch2_mode = ""
        try:
            pc.send(2, 0x222, fd_payload, fd=True, brs=True)
            time.sleep(0.5)
            ch2_alive = echoed(2, 0x222)
            ch2_mode = "FD"
        except Exception as e:
            print("CAN2 FD 发送异常:", e)
        if not ch2_alive:
            # 回退: 200U 可能没开成 FD, 试经典帧, 区分"没打开" vs "FD路径问题"
            try:
                pc.send(2, 0x223, bytes([0xAA, 0xBB]))
                time.sleep(0.5)
                if echoed(2, 0x223):
                    ch2_alive = True
                    ch2_mode = "经典(FD发送未回显)"
            except Exception as e:
                print("CAN2 经典发送异常:", e)

        pc.assert_true(ch1_alive, "CAN1 (PCAN) 独立打开且经典帧可发送")
        pc.assert_true(ch2_alive, f"CAN2 (USBCANFD-200U) 独立打开且可发送 [{ch2_mode}]")
        pc.assert_true(ch1_alive and ch2_alive, "★ 两张卡(PCAN+200U)同时作为独立通道打开 = 多卡支持")

        # ---- 跨总线(可选, 取决于接线) ----
        cross_12 = cross_21 = False
        pc.send(1, 0x1AA, bytes([0xA0, 0xA1, 0xA2, 0xA3]))
        pc.send(2, 0x2BB, bytes([0xB0, 0xB1, 0xB2, 0xB3]))
        time.sleep(0.8)
        cross_12 = rx_on(2, 0x1AA)
        cross_21 = rx_on(1, 0x2BB)
        if cross_12 or cross_21:
            print(f"=== 跨总线收发: CAN1->CAN2={cross_12}, CAN2->CAN1={cross_21}  (两卡接在同一总线, 端到端通)")
        else:
            print("=== 跨总线收发: 无 (两卡在不同总线/未对接, 属正常; 存活检测已证明各自独立打开)")

        # ---- 自发 RX 统计 ----
        time.sleep(0.5)
        rx1 = sum(1 for f in frames if f.ch == 1 and not f.tx)
        rx2 = sum(1 for f in frames if f.ch == 2 and not f.tx)
        print(f"=== 采集帧总数={len(frames)}  CAN1 RX={rx1}  CAN2 RX={rx2}")
        print("=== 最终 status:", pc.status())

        stop.set()
        rc = pc.report()
        return rc


if __name__ == "__main__":
    sys.exit(main())
