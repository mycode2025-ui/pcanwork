# GCAN / CANalyst-II 驱动适配与验收记录

## 适配边界

- GCAN USBCAN-I 使用 `ECanVci64.dll`，设备类型采用官方 `ECanVci.h` 定义的 `USBCAN1 = 3`。
- 创芯科技 CANalyst-II 使用 `ControlCAN.dll`，设备类型采用官方 `ControlCAN.h` 定义的 `VCI_USBCAN2 = 4`。
- CANalyst-II 的 CAN1/CAN2 是同一物理设备的两个硬件通道，不是两个 USB 设备；软件只打开一次物理设备，再分别初始化两个通道。

## ABI 核对

Rust FFI 与随驱动提供的官方头文件逐字段一致，并由测试固定以下尺寸：

- `BOARD_INFO` / `VCI_BOARD_INFO`: 80 bytes
- `CAN_OBJ` / `VCI_CAN_OBJ`: 24 bytes
- `CAN_STATUS` / `VCI_CAN_STATUS`: 12 bytes
- `INIT_CONFIG` / `VCI_INIT_CONFIG`: 16 bytes

GCAN 当前驱动的 `ReadBoardInfo` 可返回有效序列号，但 `can_Num` 和硬件类型文本存在异常值。因此自动扫描对 GCAN 保守固定为单通道，避免把损坏字段解释成虚假通道。

## 时间戳策略

两个官方头文件只声明 `TimeStamp` 与 `TimeFlag` 字段，没有给出时钟频率或换算单位。实机 100 ms 间隔测试证明：

- GCAN DLL 返回的原始计数不能按常见的 0.1 ms 单位解释；错误换算会把约 1.2 s 的实际间隔放大到数百秒。
- CANalyst-II DLL 返回的是另一种未文档化计数率，同样不能与 GCAN 共用固定换算。

因此 legacy VCI 驱动统一使用进程单调时钟记录接收时间。这样牺牲了未文档化硬件计数的理论精度，但保证跨设备、重连和长时间采集的时间轴正确且单调。实机修正后，三个接收通道相对墙钟的中位比例分别为 1.018、0.995、1.008，单调违规为 0。

## 设备生命周期

实机直接调用厂商 DLL 复现到：GCAN 每次 `OpenDevice`/`CloseDevice` 增长约 1 个进程句柄，CANalyst-II 每次增长约 4 个。这是 DLL 内部行为，不是 Rust 动态库对象未释放。

应用采用进程级物理设备缓存：

- 自动扫描、连接与重连复用同一个物理设备句柄。
- 普通“断开”只停止并复位 CAN 通道。
- 初始化失败、确认 USB 掉线或应用退出时才关闭物理设备。

修正后的 100 次三通道连接/发送/断开循环结果：句柄从 421 降至 412（增长 -9），工作集增长 2,674,688 bytes，未出现持续泄漏。

## 可重复验收

- `scripts/vci_bench_gate.py quick`: 六方向互发与批量压力。
- `scripts/vci_bench_gate.py cycles`: 连接/断开、句柄和工作集门禁。
- `scripts/vci_bench_gate.py soak`: 定时重连与长时间压力。
- `scripts/vci_timestamp_gate.py`: 各接收通道时间轴与墙钟比例、单调性门禁。

当前实机报告保存在 `artifacts/vci-certification/20260811-110534`。
