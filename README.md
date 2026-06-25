# PcanWork

参考 ZLG ZXDoc 风格的 **CAN/CAN FD 报文分析工具**，Slint + Rust 开发，原生支持 **PCAN(PEAK)** CAN 卡。

按 `r1.md` / `r2.md` 需求实现。本版本是**可运行的工程基座**：主框架与核心数据链路已打通，高级功能留有清晰扩展点（见下）。

## 运行

```powershell
cd D:\_Xcharge\Pcanwork
cargo run            # 调试版
cargo run --release  # 发布版（流畅）
```

操作流程：
1. 点 **连接设备** → 自动尝试打开 `PCAN_USBBUS1 @500K`；没插卡时**自动回退到虚拟总线**（生成演示报文），保证无硬件也能跑通。
2. 点 **启动** 开始接收，中央表格出现报文。
3. 点 **加载DBC** 选工程根目录的 `sample.dbc`（与虚拟总线 ID 匹配）→ 报文出现名称(EngineData/BMS_Status…)。
4. 点某条报文 → 底部 **信号解析** 显示该报文信号(Raw/Physical/单位/起始位/字节序…)。
5. 信号行点 **添加** → 切到 **曲线** Tab 看实时折线。
6. **发送报文** Tab → 新增 → 发一次 / 周期。

## 已实现（对照需求）

| 需求 | 状态 |
|---|---|
| 工具栏 + 状态指示灯(连接/运行/Rx/Tx/Err/DBC) | ✅ |
| 左侧工程树(设备/通道/DBC/发送/曲线) | ✅ |
| 中央报文表 16 列(No./Time/Delta/Ch/Dir/ID/Name/Type/FD/BRS/DLC/Len/Data/Cycle/Count/Comment) | ✅ |
| Trace / Overwrite 双模式 | ✅ |
| 变化字节高亮(Data 单元格) | ✅(单元格级，逐字节高亮见下) |
| Rx/Tx/错误帧 颜色规则、等宽字体、横向滚动、虚拟滚动 | ✅ |
| 快速过滤(ID 单值/列表/范围/排除、Name 通配、Data 字节序列) | ✅ |
| DBC 加载 + 信号解码(Intel/Motorola 位提取、有符号、factor/offset、越界判定) | ✅ |
| 信号解析面板 + 报文联动 | ✅ |
| 实时曲线(多信号、自动缩放、抽稀、10Hz 刷新) | ✅ |
| 发送(单次 / 周期，周期在控制线程精确计时) | ✅ |
| 统计(通道统计 + ID 统计 Count/AvgCycle/Min/Max/LastTime) | ✅ |
| 运行日志 | ✅ |
| 记录(导出 CSV) | ✅ |
| 接收线程与 UI 解耦、100ms 批量刷新、不阻塞接收 | ✅ |
| PCAN 真卡 + 虚拟总线自动回退 | ✅ |

## 架构

```
ui/app.slint        声明式界面(全部控件/主题/列宽/表格)
src/can.rs          设备抽象层：CanFrame + CanAdapter trait
                    ├ VirtualBus  无硬件演示数据源
                    ├ PcanBus     PCANBasic.dll FFI(libloading)
                    └ controller  独立控制线程(命令/事件 channel)
src/dbc.rs          DBC 加载 + 信号位提取/解码
src/main.rs         状态模型 App + 100ms 批量刷新 + 全部回调
sample.dbc          与虚拟总线匹配的示例数据库
```

数据流：`控制线程(收/发) → mpsc channel → UI 100ms Timer 批量取 → 重建 Slint 模型`。

## 当前简化 / 待扩展（明确边界，便于继续开发）

- **逐字节高亮**：现为 Data 单元格整体高亮(有变化即黄底)。逐字节高亮需把 Data 拆成 `[byte]` 模型按位染色——扩展点在 `MsgRow.data` + slint Data 单元格。
- **自动滚动到底部**：Slint `ListView` 无直接滚动 API，Trace 模式现以时间正序显示最后 1500 帧。Overwrite 模式是更实用的实时视图。
- **Trace 显示上限**：单次刷新最多渲染 1500 行(原始缓存仍保留 10 万行)，保证刷新轻量。
- **高级过滤弹窗 / 多级排序 / 列头下拉筛选 / 分组 / 停靠拖拽 / CAN FD 真卡收发**：UI 与数据结构已预留，尚未接入。
- **总线负载/FPS**、绝对时间轴、双游标、曲线↔报文时间同步：占位，待接入。
- PCAN 真卡路径目前为**经典 CAN**(标准/扩展帧)；CAN FD 收发(`CAN_InitializeFD`)为下一步。

## 依赖

slint 1.16 / can-dbc 9.1 / rfd(文件对话框) / libloading(动态加载 PCANBasic.dll)。
PCAN 真卡需安装 PEAK 驱动(`PCANBasic.dll` 已在 `C:\Windows\System32`)。
