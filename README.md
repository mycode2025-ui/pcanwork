# PcanWork v0.4.10

**Windows CAN / CAN FD 工程分析平台**

面向汽车电子、储能、充电设备与工业通信研发测试，覆盖多厂商硬件接入、DBC 解析、报文采集与发送、记录回放、实时曲线、可视化仿真、printf-over-CAN、Modbus 与串口调试。

[GitHub 下载 v0.4.10](https://github.com/mycode2025-ui/pcanwork/releases/download/v0.4.10/PcanWork-Setup-0.4.10.exe) · [Gitee 下载 v0.4.10](https://gitee.com/mycode2025-ui/pcanwork/releases/download/v0.4.10/PcanWork-Setup-0.4.10.exe) · [官方网站](https://www.hexbyte.cn) · [版本说明](https://www.hexbyte.cn/release-notes-0.4.10.html)

![PcanWork v0.3.25 工程中心](site-assets/product/v0325-main.jpg)

## v0.4.10 更新

- 独立曲线窗口移除“导出 CSV”“导出宽表”“信号记录”三个最右侧工具按钮及对应窗口入口，工具栏更聚焦曲线查看和分析。
- 曲线工具栏按钮内部的白色/深色图形统一为 24 px，同时保留 44 px 点击热区，提升视觉一致性和点击操作舒适度。
- Release 安装包已完成版本号、文件大小与 SHA-256 校验。

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

- 版本：`0.4.10`
- 安装包：`PcanWork-Setup-0.4.10.exe`
- 大小：`37,051,292` 字节
- SHA-256：`3434D1728C6F3D62C2D36950C36692AA1E77021B6670D64093C5D4D7B3998572`
- 系统：Windows 10/11 64 位
- 签名状态：当前安装包未进行代码签名

真实 CAN/CAN FD 报文采集和发送需要兼容硬件及相应厂商驱动。工程、DBC、界面与配置等非总线功能可独立打开使用；当前版本不提供虚拟 CAN 总线。
