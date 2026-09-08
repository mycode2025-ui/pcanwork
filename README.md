# PcanWork v0.6.0

**Windows CAN / CAN FD 工程分析平台**

面向汽车电子、储能、充电设备与工业通信研发测试，覆盖多厂商硬件接入、DBC 解析、报文采集与发送、记录回放、实时曲线、可视化仿真、printf-over-CAN、Modbus 与串口调试。

[GitHub 下载 v0.6.0](https://github.com/mycode2025-ui/pcanwork/releases/download/v0.6.0/PcanWork-Setup-0.6.0.exe) · [Gitee 下载 v0.6.0](https://gitee.com/mycode2025-ui/pcanwork/releases/download/v0.6.0/PcanWork-Setup-0.6.0.exe) · [官方网站](https://www.hexbyte.cn) · [版本说明](https://www.hexbyte.cn/release-notes-0.6.0.html)

![PcanWork v0.3.25 工程中心](site-assets/product/v0325-main.jpg)

## v0.6.0 更新

- 大文件离线回放改为批处理，快速、正常倍速和步进模式均减少事件队列压力。
- 曲线刷新直接在环形缓存中定位可视区并保留极值抽样，避免每 100 ms 复制全量几十万点数据。
- 数据回放窗口独立按需创建，首次打开不再连带初始化其他 14 个功能窗口。
- 修复回放数据、进度、循环首尾连线、绝对日期时间轴，以及曲线平移、框选缩放、游标对齐和多种 Y 轴模式。
- 曲线工具栏统一专业图标，按钮外框缩小至 33 × 30 px，内部图形放大至 24 × 24 px，工具栏高度调整为 38 px。
- CAN、Modbus、Serial 界面的主题、响应式布局、默认窗口尺寸、弹窗居中与帮助说明统一优化。
- 主程序、CAN 和 Modbus 后端拆分模块，并增加高负载增量表格与软件压力测试。

![PcanWork v0.3.25 双通道工程](site-assets/product/v0325-workspace.jpg)

![PcanWork v0.3.25 PCAN 与 ZLG 设备配置](site-assets/product/v0325-device.jpg)

## 核心能力

- **多厂商 CAN / CAN FD**：PCAN（PEAK）、ZLG、ZHCX、GCAN 的设备扫描、通道配置和报文收发。
- **DBC 完整数值语义**：Unsigned、Signed、IEEE Float、IEEE Double，Intel / Motorola 字节序，factor、offset、单位和范围。
- **分析与记录**：16 列报文表、过滤、分组、变化高亮、实时曲线、CSV / ASC / BLF 记录与回放。
- **发送与仿真**：单次/周期发送、发送列表、可视化仿真工作区和 DBC 信号联动。
- **嵌入式调试**：printf-over-CAN 文本日志、UDS / XCP 工具入口。
- **Modbus Tools**：Modbus TCP / RTU 主站、从站仿真、寄存器视图、事件和流量监控。
- **Serial Tool**：普通串口调试、ANSI 交互终端、多行粘贴、文件和定时发送。

![PcanWork CAN FD 波特率配置](site-assets/product/v0325-fd-bitrate.png)

## 下载与校验

- 版本：`0.6.0`
- 安装包：`PcanWork-Setup-0.6.0.exe`
- 大小：`35,467,693` 字节
- SHA-256：`9EB368AA5A50C5A42231EC9F6381CF59D16B38E5FE94DEBEBE4620B97C7BAD90`
- 系统：Windows 10/11 64 位
- 签名状态：当前安装包未进行 Authenticode 代码签名；构建脚本已生成内部完整性签名
- 自动测试：67 项通过，0 项失败，1 项联网测试跳过

真实 CAN/CAN FD 报文采集和发送需要兼容硬件及相应厂商驱动。工程、DBC、界面与配置等非总线功能可独立打开使用；当前版本不提供虚拟 CAN 总线。
