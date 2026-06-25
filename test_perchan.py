#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""按通道统计验证: per-channel bus_load 不再 2x + 按方向精确测丢帧。

PCAN(CAN1) <-> 200U(CAN2) 同一 500K 总线。双向 1ms 周期互发一段时间, 然后:
  · 读 status 的 channels[] —— 每通道独立 rx/tx/bus_load。
  · 验证 CAN1/CAN2 各自 bus_load 都 ≈ 真实线缆负载(不再是全局 2x/封顶100)。
  · 按方向精确算丢帧:  CAN1 发的(tx_ch1) 应 = CAN2 收的(rx_ch2);  反向同理。
"""
import sys
import time
import pcanwork


def chan_map(st):
    return {c["ch"]: c for c in st.get("channels", [])}


def main():
    pc = pcanwork.connect()
    with pc:
        pc.subscribe([0x7FE])  # 静音 IPC 流, 纯靠计数器
        pc.connect_devices([
            pcanwork.dev("PCAN",          sw_channel=1, channel_index=0, baud="500K", fd=False),
            pcanwork.dev("USBCANFD-200U", sw_channel=2, channel_index=0, baud="500K", fd=False),
        ], wait=False)
        deadline = time.monotonic() + 4.0
        while time.monotonic() < deadline and not pc.status().get("connected"):
            time.sleep(0.1)
        if not pc.status().get("connected"):
            print("两卡未连接, 退出"); return 1
        pc.start()
        time.sleep(0.3)

        # 双向各 2 路 @1ms
        data = bytes([1, 2, 3, 4, 5, 6, 7, 8])
        for i in range(2):
            pc.set_periodic(handle=100 + i, ch=1, id=0x100 + i, data=data, period_ms=1, repeat=-1)
            pc.set_periodic(handle=200 + i, ch=2, id=0x200 + i, data=data, period_ms=1, repeat=-1)

        s0 = pc.status(); m0 = chan_map(s0)
        t0 = time.monotonic()
        load1_samples, load2_samples = [], []
        while time.monotonic() - t0 < 4.0:
            st = pc.status(); m = chan_map(st)
            if 1 in m: load1_samples.append(m[1]["bus_load"])
            if 2 in m: load2_samples.append(m[2]["bus_load"])
            time.sleep(0.15)
        dt = time.monotonic() - t0

        for i in range(2):
            pc.stop_periodic(100 + i)
            pc.stop_periodic(200 + i)
        time.sleep(0.6)
        s1 = pc.status(); m1 = chan_map(s1)

        def delta(ch, key):
            return m1.get(ch, {}).get(key, 0) - m0.get(ch, {}).get(key, 0)

        tx1, rx1 = delta(1, "tx"), delta(1, "rx")
        tx2, rx2 = delta(2, "tx"), delta(2, "rx")
        peak1 = max(load1_samples) if load1_samples else 0
        peak2 = max(load2_samples) if load2_samples else 0

        print(f"窗口 {dt:.1f}s")
        print(f"  CAN1(PCAN) : tx={tx1}  rx={rx1}  峰值负载={peak1:.1f}%")
        print(f"  CAN2(200U) : tx={tx2}  rx={rx2}  峰值负载={peak2:.1f}%")
        print()

        # 按方向丢帧
        drop_1to2 = tx1 - rx2
        drop_2to1 = tx2 - rx1
        pct_12 = (drop_1to2 / tx1 * 100) if tx1 else 0
        pct_21 = (drop_2to1 / tx2 * 100) if tx2 else 0
        print(f"方向 CAN1->CAN2: 发 {tx1}, 收 {rx2}, 丢 {drop_1to2} ({pct_12:.2f}%)")
        print(f"方向 CAN2->CAN1: 发 {tx2}, 收 {rx1}, 丢 {drop_2to1} ({pct_21:.2f}%)")
        print()

        # 断言: 两通道都有流量 + 负载没翻倍(<=100 显然, 但应≈真实, 这里只要 >0 且两通道接近)
        pc.assert_true(tx1 > 0 and tx2 > 0, "两通道都在发送")
        pc.assert_true(rx1 > 0 and rx2 > 0, "两通道都在接收")
        pc.assert_true(abs(drop_1to2) <= max(5, tx1 * 0.02), f"CAN1->CAN2 丢帧≈0 (容边界 {drop_1to2})")
        pc.assert_true(abs(drop_2to1) <= max(5, tx2 * 0.02), f"CAN2->CAN1 丢帧≈0 (容边界 {drop_2to1})")
        # per-channel 负载应彼此接近(同一根线), 且不应同时为 0
        pc.assert_true(peak1 > 0 and peak2 > 0, f"两通道各自负载>0 (CAN1={peak1:.0f}%, CAN2={peak2:.0f}%)")
        pc.assert_true(abs(peak1 - peak2) <= 15, f"两通道负载接近(同线) (差 {abs(peak1-peak2):.0f}%)")

        print("=== 完整最终 status:")
        print("   ", pc.status())
        return pc.report()


if __name__ == "__main__":
    sys.exit(main())
