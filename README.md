# PCAN-Explorer10 v0.6.3

**Windows CAN / CAN FD 工程分析平台**

面向汽车电子、储能、充电设备与工业通信研发测试，覆盖多厂商硬件接入、DBC 解析、报文采集与发送、记录回放、实时曲线、可视化仿真、printf-over-CAN、Modbus 与串口调试。

[GitHub 下载 v0.6.3](https://github.com/mycode2025-ui/PCAN-Explorer10/releases/download/v0.6.3/PCAN-Explorer10-Setup-0.6.3.exe) · [Gitee 下载 v0.6.3](https://gitee.com/mycode2025-ui/PCAN-Explorer10/releases/download/v0.6.3/PCAN-Explorer10-Setup-0.6.3.exe) · [官方网站](https://www.hexbyte.cn) · [版本说明](https://www.hexbyte.cn/release-notes-0.6.3.html)

![PCAN-Explorer10 v0.3.25 工程中心](site-assets/product/v0325-main.jpg)

## v0.6.3 更新

- 产品、主窗口、工具窗口和文档统一更名为 `PCAN-Explorer10`。
- 主程序与安装包更名为 `PCAN-Explorer10.exe` 和 `PCAN-Explorer10-Setup-0.6.3.exe`。
- 升级安装会迁移旧版设置与许可证，并清理旧品牌安装。
- GitHub、Gitee 仓库和官方网站链接统一使用新名称。

## 核心能力

- **多厂商 CAN / CAN FD**：PCAN（PEAK）、ZLG、ZHCX、GCAN 的设备扫描、通道配置和报文收发。
- **DBC 完整数值语义**：Unsigned、Signed、IEEE Float、IEEE Double，Intel / Motorola 字节序，factor、offset、单位和范围。
- **分析与记录**：16 列报文表、过滤、分组、变化高亮、实时曲线、CSV / ASC / BLF 记录与回放。
- **发送与仿真**：单次/周期发送、发送列表、可视化仿真工作区和 DBC 信号联动。
- **嵌入式调试**：printf-over-CAN 文本日志、UDS / XCP 工具入口。
- **Modbus Tools**：Modbus TCP / RTU 主站、从站仿真、寄存器视图、事件和流量监控。
- **Serial Tool**：普通串口调试、ANSI 交互终端、多行粘贴、文件和定时发送。

![PCAN-Explorer10 CAN FD 波特率配置](site-assets/product/v0325-fd-bitrate.png)

## 下载与校验

- 版本：`0.6.3`
- 安装包：`PCAN-Explorer10-Setup-0.6.3.exe`
- 大小：`35,455,094` 字节
- SHA-256：`F6A10D83356072178D461ABCC3BBCC07480DAB8B9235CC7F19ED0740DD69FF08`
- 系统：Windows 10/11 64 位
- 签名状态：当前安装包未进行 Authenticode 代码签名；构建脚本已生成内部完整性签名
- 自动测试：67 项通过，0 项失败，1 项联网测试跳过

真实 CAN/CAN FD 报文采集和发送需要兼容硬件及相应厂商驱动。工程、DBC、界面与配置等非总线功能可独立打开使用；当前版本不提供虚拟 CAN 总线。
