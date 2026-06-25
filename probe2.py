#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""决定性诊断: 两卡时 200U 到底 没开 / 开了但发送失败 / 开了但总线断。"""
import time, queue, threading, pcanwork

pc = pcanwork.connect()
with pc:
    frames = []
    stop = threading.Event()
    q = queue.Queue()
    with pc._evt_lock:
        pc._evt_subs.append((set(), (lambda f: True), q))
    def drain():
        while not stop.is_set():
            try: frames.append(q.get(timeout=0.2))
            except queue.Empty: pass
    threading.Thread(target=drain, daemon=True).start()

    pc.connect_devices([
        pcanwork.dev("PCAN", sw_channel=1, channel_index=0, baud="500K", fd=False),
        pcanwork.dev("USBCANFD-200U", sw_channel=2, channel_index=0, baud="500K", fd=False),
    ], wait=False)
    d = time.monotonic()+4
    while time.monotonic()<d and not pc.status().get("connected"): time.sleep(0.1)
    pc.start(); time.sleep(0.4)

    # 发 ch2, 看回显通道号
    pc.send(2, 0x2EE, bytes([1,2,3,4])); time.sleep(0.7)
    echo = [(f.ch, f.tx) for f in frames if f.id==0x2EE]
    # 发 ch1, 看 200U(ch2) 是否收到
    pc.send(1, 0x1EE, bytes([5,6,7,8])); time.sleep(0.7)
    rx2 = [f for f in frames if f.id==0x1EE and f.ch==2 and not f.tx]

    print("发 ch2(0x2EE) 的回显帧 (ch,tx):", echo)
    print("ch1->ch2: 200U 收到 0x1EE 帧数:", len(rx2))
    print()
    if any(c==2 for c,_ in echo):
        print("判定: ★200U 已打开(回显带 ch=2), 且发送成功 => 在总线上")
        if rx2: print("      且 CAN1->CAN2 跨卡接收正常 => 接线OK")
        else:   print("      但 CAN1 的帧 200U 没收到 => 接线可能断")
    elif any(c==1 for c,_ in echo):
        print("判定: 200U 没打开, 发送回退到 CAN1 (回显带 ch=1)")
    else:
        print("判定: 200U 已打开但发送失败(无回显) => 多半无 ACK/总线断/无终端")
        if rx2: print("      但 200U 收到了 CAN1 的帧 => 其实在总线上, 只是发送侧无对端ACK?")
    stop.set()
