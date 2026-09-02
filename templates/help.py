#!/usr/bin/env python3
r"""
PcanWork Python 自动化测试帮助
=============================

一、这个功能能做什么
--------------------

Python 脚本运行器把 PcanWork 的 CAN/CAN FD 能力开放给普通 Python 脚本。
脚本通过本机 IPC 与当前 PcanWork 通信，不直接加载硬件驱动，因此可以：

1. 使用电脑上已经安装的 Python 3.7 或更高版本，不需要 pip 安装 pcanwork。
2. 运行单个 .py 测试，或者按文件名顺序运行一个目录中的全部测试。
3. 使用主界面已配置的 CAN 通道，或者在脚本中指定设备和通道。
4. 使用 PCAN、ZLG、GCAN、创芯科技等 PcanWork 已适配的设备。
5. 控制多设备、多通道、经典 CAN、扩展帧、CAN FD、BRS 和远程帧。
6. 单帧发送、批量发送、周期发送、停止周期任务并读取发送结果。
7. 等待指定报文、读取最新报文、按条件等待报文并处理超时。
8. 加载 DBC、查询报文和信号、编码物理值、解码数据、等待信号条件。
9. 读取 PcanWork 运行日志和 printf-over-CAN 文本日志。
10. 使用 PASS/FAIL 断言、测试汇总、退出码和目录测试套件。

二、运行前准备
--------------

1. 连接 CAN 盒与总线，检查 CAN_H、CAN_L、地线和终端电阻。
2. 在“主页 -> 设备”中检查厂商、设备索引、物理通道、仲裁域波特率、
   数据域波特率和 CAN/CAN FD 类型。
3. 如需按信号测试，在主界面加载 DBC，或者在脚本中调用 load_dbc()。
4. 打开“功能 -> Py测试”，点击“检测已装版本”，选择可用 Python。
5. 从安装目录 templates 复制一个示例到工作目录再修改，不建议直接修改模板。

运行器会自动设置 PCANWORK_IPC_PORT、PCANWORK_IPC_TOKEN、
PCANWORK_CLIENT_DIR 和 PYTHONPATH。依赖 PcanWork 的测试必须从本运行器启动；
直接在命令行运行时没有 IPC 端口和令牌，会连接失败。

三、最快的第一次测试
----------------------

步骤 1：在“设备”窗口配置一个真实 CAN 通道并保存配置。
步骤 2：选择 templates\example_bms_test.py。
步骤 3：根据实际设备修改示例中的 open_bus()。
步骤 4：点击“运行”，输出区会实时显示 stdout 和 stderr。
步骤 5：“通过”表示退出码为 0；“失败”表示退出码非 0、异常或超时。
步骤 6：完整日志路径显示在输出第一行的 pcanwork_last_run.log。

最小脚本：

    import sys
    import pcanwork

    def main():
        with pcanwork.connect() as can:
            if not can.status().get("connected"):
                count = can.connect_configured()
                can.assert_true(count > 0, "至少配置了一个 CAN 通道")
                can.start()
            can.send(ch=1, id=0x123, data=bytes([1, 2, 3, 4]))
            sent = can.last(ch=1, id=0x123, dir="tx")
            can.assert_true(sent is not None, "0x123 已发送")
            return can.report()

    if __name__ == "__main__":
        sys.exit(main())

四、三种设备连接方式
--------------------

方式 A：使用主界面配置（推荐）

    with pcanwork.connect() as can:
        channel_count = can.connect_configured(timeout=5.0)
        can.assert_true(channel_count > 0, "已打开主界面配置的通道")
        can.start()

connect_configured() 打开“设备”窗口中配置的全部通道，并核对实际连接通道。
只配置但没有成功打开的设备不会被当作已连接。

方式 B：脚本指定一个设备

    can.connect_device(
        "PCAN", sw_channel=1, device_index=0, channel_index=0,
        fd=False, baud="500K", termination=False, timeout=4.0,
    )
    can.start()

CAN FD 示例：

    can.connect_device(
        "USBCANFD-200U", sw_channel=1, device_index=0, channel_index=0,
        fd=True, baud="500K", data_baud="2M", termination=True,
    )
    can.start()

方式 C：脚本指定多个设备或通道

    devices = [
        pcanwork.dev("PCAN", sw_channel=1, channel_index=0, baud="500K"),
        pcanwork.dev("USBCANFD-200U", sw_channel=2, device_index=0,
                     channel_index=1, fd=True, baud="500K", data_baud="2M"),
    ]
    can.connect_devices(devices, timeout=5.0)
    can.start()

sw_channel 是逻辑通道号，也是 send(ch=...)、last(ch=...) 和 wait_for(ch=...)
使用的 ch。device_index 选择同型号第几个设备；channel_index 选择该设备的
物理 CAN 口，均从 0 开始。

五、发送 CAN/CAN FD 报文
-----------------------

经典标准帧：

    can.send(ch=1, id=0x123, data=bytes([0x01, 0x02, 0x03]))

扩展 CAN FD+BRS：

    can.send(ch=2, id=0x18FF50E5, data=bytes(range(16)),
             ext=True, fd=True, brs=True)

批量发送：

    queued = can.send_batch([
        {"ch": 1, "id": 0x100, "data": b"\x01\x02"},
        {"ch": 2, "id": 0x18FF1001, "data": bytes(range(12)),
         "ext": True, "fd": True, "brs": True},
    ], repeat=3)
    can.assert_eq(queued, 6, "两帧各发送三次")

周期发送：

    can.set_periodic(handle=1001, ch=1, id=0x700,
                     data=b"\xAA\x55", period_ms=100, repeat=-1)
    can.sleep(1.0)
    can.stop_periodic(handle=1001)

handle 由脚本分配，同一时刻必须唯一。repeat=-1 表示持续发送；正整数表示指定
次数。脚本结束前应停止不再需要的无限周期任务。

六、接收、等待和检查报文
------------------------

读取最近一次接收值：

    frame = can.last(ch=1, id=0x321, dir="rx")
    if frame is not None:
        print(frame.ch, hex(frame.id), frame.data, frame.t, frame.count)

等待指定报文：

    frame = can.wait_for(ch=1, id=0x321, timeout=2.0)

按条件等待：

    frame = can.wait_for(
        ch=1, id=0x321,
        predicate=lambda f: len(f.data) >= 2 and f.data[0] == 0x5A,
        timeout=3.0,
    )

读取自己发送的报文必须明确 dir="tx"：

    sent = can.last(ch=1, id=0x123, dir="tx")

wait_for() 超时会抛出 pcanwork.TimeoutError_：

    try:
        frame = can.wait_for(ch=1, id=0x321, timeout=1.0)
    except pcanwork.TimeoutError_ as error:
        can.log(f"没有收到报文：{error}")

七、DBC 和信号测试
------------------

加载 DBC：

    name = can.load_dbc(r"D:\CAN\BMS.dbc")
    can.log(f"已加载：{name}")

查看 DBC 中的真实报文和信号名：

    for message in can.dbc_info():
        print(hex(message["id"]), message["name"], message["dlc"])
        for signal in message.get("signals", []):
            print("   ", signal["name"], signal.get("unit", ""))

查询某个 ID 的信号名：

    names = can.signals_of(0x180310E4)
    can.assert_true("BCU_Volt" in names, "DBC 包含 BCU_Volt")

编码并发送物理值：

    can.send_signals(
        id=0x180310E4,
        signals={"BCU_Volt": 750.0, "BCU_Current": -20.5},
        ch=1, ext=True, fd=False,
    )

先编码，再批量发送：

    payload = can.encode(0x180310E4, {"BCU_Volt": 750.0}, ext=True)
    can.send_batch([{"ch": 1, "id": 0x180310E4,
                     "data": payload, "ext": True}])

解码收到的报文：

    frame = can.wait_for(ch=1, id=0x180310E4, timeout=2.0)
    values = can.decode(0x180310E4, frame.data, ext=True)
    can.assert_close(values["BCU_Volt"], 750.0, tol=0.1, msg="母线电压")

等待信号满足条件：

    voltage = can.wait_for_signal(
        ch=1, id=0x180310E4, name="BCU_Volt",
        cmp=lambda value: 700.0 <= value <= 800.0,
        timeout=5.0, ext=True,
    )

DBC 信号名称区分大小写。信号不存在时先用 dbc_info() 或 signals_of() 输出实际
名称。dbc_diagnostics() 可返回静态诊断摘要和问题列表。

八、断言和测试结果
------------------

    can.assert_true(condition, "条件说明")
    can.assert_eq(actual, expected, "相等检查")
    can.assert_close(actual, expected, tol=0.01, msg="浮点误差检查")
    can.log("普通测试日志")
    return can.report()

断言不会立即终止脚本，而是累计通过和失败数量。report() 输出最终汇总：全部断言
通过返回 0；存在失败返回 1。未捕获异常也会被运行器标记为失败。

九、printf-over-CAN 文本日志
---------------------------

    can.console_config(enabled=True, id=0x7E0, ch=1, clear=True)
    can.sleep(1.0)
    text = can.console_text()
    can.assert_true("BOOT OK" in text, "单片机启动日志")

id=-1 表示任意 ID，ch=0 表示任意通道。脚本读取的是 PcanWork 已重组的文本，
不是原始 CAN 字节。

十、运行测试套件
----------------

1. 建立测试目录，每个测试放在独立 .py 文件中。
2. 每个文件使用 main() 返回退出码，并在末尾 sys.exit(main())。
3. 点击“运行套件”，选择目录。
4. run_suite.py 按文件名排序运行全部 .py 测试。
5. 每个测试运行在独立 Python 进程中，一个异常不会阻止其他测试。
6. 默认单测试超时 120 秒，可用 PCANWORK_TEST_TIMEOUT 环境变量修改。
7. run_suite.py、help.py、pcanwork.py 和以下划线开头的文件不会作为测试执行。

建议命名：01_connection.py、02_basic_rx_tx.py、03_dbc_signals.py、
04_periodic.py、05_fault_recovery.py。

十一、常见问题
--------------

“PCANWORK_IPC_PORT not set”
    脚本不是从 PcanWork Python 运行器启动，请回到 PcanWork 中运行。

“解释器校验失败”
    重新检测 Python，或浏览到真实 python.exe，不要选择 pythonw.exe。

“CAN channel connection incomplete”
    部分设备没有实际打开。检查驱动、设备索引、物理通道、波特率、占用状态和
    后端日志，不能仅凭“已配置”继续测试。

“no frame ... within ...”
    指定时间内没有匹配报文。检查通道、ID、标准/扩展帧、波特率、终端电阻和对端。

DBC 解码结果为空
    检查 DBC、ID、ext 参数和数据长度；先打印 dbc_info() 和 dbc_diagnostics()。

脚本一直不结束
    检查无限循环和自定义等待；也可点击“停止”终止当前 Python 子进程。

输出过多
    运行器使用有界队列保护主程序，持续刷屏可能丢弃部分显示行。只输出必要诊断，
    并查看输出第一行给出的完整日志路径。

十二、安装包示例和 API 速查
---------------------------

example_bms_test.py：单设备、DBC、收发、周期发送和断言汇总。
example_multi_card.py：多设备/多通道连接和独立路由验证。
run_suite.py：逐个执行目录脚本并聚合 PASS/FAIL。
help.py：本帮助；直接运行只打印帮助，不连接或操作 CAN。

会话：connect、close、status、logs、start、stop、disconnect
设备：connect_configured、connect_device、connect_devices、wait_connected_channels
发送：send、send_batch、send_signals、set_periodic、stop_periodic
接收：last、subscribe、wait_for、wait_for_signal
DBC：load_dbc、dbc_info、dbc_diagnostics、signals_of、encode、decode、signal
日志：console_config、console_text、log
验证：expect、assert_true、assert_eq、assert_close、report、sleep

完整可运行代码以 example_bms_test.py 和 example_multi_card.py 为准。
"""


def main() -> int:
    """打印帮助；不连接、不启动也不发送任何 CAN 报文。"""
    print(__doc__ or "PcanWork Python help is unavailable.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
