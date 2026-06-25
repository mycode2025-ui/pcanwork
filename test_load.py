#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""高负载压测: PCAN(CAN1) <-> 200U(CAN2) 同一 500K 总线, 双向 1ms 周期互发。

逐级增加每通道并发周期帧数(1/2/4/8 路 @1ms), 把总线从半载推到 2x 过载,
观察实际吞吐(帧/秒)、丢帧、错误帧、以及 app 报告的 bus_load。

测量纯靠 app 自身计数器(status: rx/tx/err/bus_load), 不经 IPC 流(已收窄订阅),
避免 IPC 成为瓶颈或产生假丢帧。

口径:
  · 同一根线 + 两卡都是我们的 → 每物理帧被数 2 次(本卡 tx 回显 + 对卡 rx)。
    => 真实线缆吞吐 ≈ tx_delta(成功发送数), 真实线缆负载 ≈ app_bus_load / 2。
  · 干净总线无丢帧时 rx_delta ≈ tx_delta(每发出的帧被对端收到 1 次)。
    RX 丢帧 = tx_delta - rx_delta。
理论上限: 500Kbps, 8 字节标准帧 ~111 bit/帧 → ~4500 帧/s(单向), 双向合计同此线缆上限。
"""
import sys
import time
import pcanwork

BUS_BPS = 500_000.0
FRAME_BITS = 47 + 8 * 8  # 标准帧 8 字节 ~111 bit (与 app frame_bits 同口径)
THEO_FPS = BUS_BPS / FRAME_BITS


def run_level(pc, handles_per_ch, window_s=3.0):
    # 启动每通道 handles_per_ch 路周期帧(8 字节, 1ms, 无限)
    data = bytes([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x01, 0x02])
    handles = []
    for i in range(handles_per_ch):
        h1 = 100 + i
        h2 = 200 + i
        pc.set_periodic(handle=h1, ch=1, id=0x100 + i, data=data, period_ms=1, repeat=-1)
        pc.set_periodic(handle=h2, ch=2, id=0x200 + i, data=data, period_ms=1, repeat=-1)
        handles += [h1, h2]

    s0 = pc.status()
    t0 = time.monotonic()
    peak_load = 0.0
    while time.monotonic() - t0 < window_s:
        st = pc.status()
        peak_load = max(peak_load, float(st.get("bus_load", 0) or 0))
        time.sleep(0.1)
    dt = time.monotonic() - t0

    for h in handles:
        pc.stop_periodic(h)
    time.sleep(0.5)  # 让在途帧收完
    s1 = pc.status()

    tx = s1["tx"] - s0["tx"]
    rx = s1["rx"] - s0["rx"]
    err = s1["err"] - s0["err"]
    tx_fps = tx / dt
    rx_fps = rx / dt
    drop = tx - rx
    drop_pct = (drop / tx * 100.0) if tx else 0.0
    real_load = tx_fps / THEO_FPS * 100.0  # 真实线缆负载(按成功发送数)
    return {
        "lanes": handles_per_ch, "tx": tx, "rx": rx, "err": err,
        "tx_fps": tx_fps, "rx_fps": rx_fps, "drop": drop, "drop_pct": drop_pct,
        "app_load": peak_load, "real_load": real_load,
    }


def main():
    pc = pcanwork.connect()
    with pc:
        # 收窄订阅到一个永不发送的 id, 让 IPC 不转发任何压测帧
        pc.subscribe([0x7FE])

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
        print(f"理论上限: 500Kbps, 8字节标准帧≈{FRAME_BITS}bit/帧 → 单向≈{THEO_FPS:.0f} 帧/s")
        print(f"{'级别':<6}{'路/通道':<8}{'实际TX(帧/s)':<14}{'实际RX(帧/s)':<14}"
              f"{'丢帧':<8}{'丢帧%':<8}{'错误帧':<8}{'真实负载%':<11}{'app显示%':<9}")

        results = []
        for lanes in (1, 2, 4, 8):
            r = run_level(pc, lanes, window_s=3.0)
            results.append(r)
            print(f"L{lanes:<5}{r['lanes']:<8}{r['tx_fps']:<14.0f}{r['rx_fps']:<14.0f}"
                  f"{r['drop']:<8}{r['drop_pct']:<8.2f}{r['err']:<8}"
                  f"{r['real_load']:<11.1f}{r['app_load']:<9.1f}")
            time.sleep(0.6)  # 级别间清空

        print("\n最终 status:", pc.status())
        # 简单判定: 任一级别 真实吞吐 > 半载 且 丢帧率 < 5% 视为稳健
        best = max(results, key=lambda r: r["tx_fps"])
        print(f"\n峰值实际吞吐: {best['tx_fps']:.0f} 帧/s (真实线缆负载≈{best['real_load']:.0f}%), "
              f"该级丢帧 {best['drop']} ({best['drop_pct']:.2f}%), 错误帧 {best['err']}")
        return 0


if __name__ == "__main__":
    sys.exit(main())
