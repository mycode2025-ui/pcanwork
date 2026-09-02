# PcanWork printf-over-CAN 单片机接入指南

## 1. 用途

printf-over-CAN 用一个保留的 CAN 报文 ID，把单片机的调试文本发送到
PcanWork 主界面的“CAN 文本日志”控制台。它适合启动日志、状态量和故障定位，
不用于需要确认、重传或严格时序的业务数据。

## 2. 上位机设置

1. 配置 CAN 通道并启动测量。
2. 打开“数据 → CAN日志”或“工具 → CAN日志”。
3. ID 填入单片机使用的十六进制 ID，例如 `6F0`；留空会接收所有 ID。
4. 选择对应通道；“全部”表示接收任意通道。
5. 勾选“捕获”。

建议为日志分配项目专用 ID。示例使用标准帧 `0x6F0`，实际项目必须检查
DBC、诊断和网关配置，避免与现有报文冲突。不要直接占用常见的 UDS
请求/响应 ID（例如 `0x7E0`～`0x7EF`）。

## 3. 线级协议

- 方向：MCU → PcanWork。
- 载荷：原始文本字节，推荐 UTF-8；ASCII 是 UTF-8 的子集。
- `LF`（`0x0A`）结束一行。
- `CR`（`0x0D`）被忽略，因此 `\n` 和 `\r\n` 都可使用。
- `NUL`（`0x00`）被忽略，可用于 CAN/CAN FD 载荷补齐。
- 一行可以跨越任意数量的 CAN 帧，上位机会按 ID 和通道筛选后连续拼接。
- 上位机最多保留 5000 行；单行超过 8192 字节时会自动截成新行。
- 本协议是尽力传输，不含序号、确认和重传。丢帧会导致该行文本缺失。

经典 CAN 每帧最多 8 字节。CAN FD 每帧最多 64 字节，并且大于 8 字节时
只能使用 12、16、20、24、32、48、64 这些合法载荷长度。

## 4. 可移植 C 实现

下面代码不在中断里阻塞等待发送。`board_can_try_send()` 必须把数据复制进
驱动发送邮箱或软件队列，并立即返回；邮箱满时返回 `false`。

```c
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>

#define CAN_LOG_ID          0x6F0u
#define CAN_LOG_USE_FD      0
#define CAN_LOG_PAYLOAD_MAX (CAN_LOG_USE_FD ? 64u : 8u)

/* 由板级驱动实现。成功入队返回 true，邮箱/队列满返回 false。 */
extern bool board_can_try_send(uint32_t id,
                               const uint8_t *data,
                               uint8_t len,
                               bool is_fd);

/*
 * 裸机且只有一个调用者时可保持为空。
 * RTOS 多任务调用 printf 时，应替换为互斥锁；不要从 ISR 调用。
 */
#ifndef CAN_LOG_LOCK
#define CAN_LOG_LOCK()
#define CAN_LOG_UNLOCK()
#endif

static uint8_t g_can_log_buf[64];
static uint8_t g_can_log_len;
static volatile uint32_t g_can_log_dropped_frames;

static uint8_t can_log_fd_length(uint8_t length)
{
    static const uint8_t valid[] = { 8u, 12u, 16u, 20u, 24u, 32u, 48u, 64u };
    size_t index;

    if (length <= 8u) {
        return length;
    }
    for (index = 0u; index < sizeof(valid); ++index) {
        if (valid[index] >= length) {
            return valid[index];
        }
    }
    return 64u;
}

static void can_log_flush_locked(void)
{
    uint8_t tx_length;

    if (g_can_log_len == 0u) {
        return;
    }

    tx_length = CAN_LOG_USE_FD
        ? can_log_fd_length(g_can_log_len)
        : g_can_log_len;

    while (g_can_log_len < tx_length) {
        g_can_log_buf[g_can_log_len++] = 0x00u;
    }

    if (!board_can_try_send(CAN_LOG_ID,
                            g_can_log_buf,
                            tx_length,
                            CAN_LOG_USE_FD != 0)) {
        ++g_can_log_dropped_frames;
    }
    g_can_log_len = 0u;
}

void can_log_write(const void *data, size_t length)
{
    const uint8_t *bytes = (const uint8_t *)data;
    size_t index;

    CAN_LOG_LOCK();
    for (index = 0u; index < length; ++index) {
        g_can_log_buf[g_can_log_len++] = bytes[index];
        if ((bytes[index] == (uint8_t)'\n') ||
            (g_can_log_len == CAN_LOG_PAYLOAD_MAX)) {
            can_log_flush_locked();
        }
    }
    CAN_LOG_UNLOCK();
}

void can_log_flush(void)
{
    CAN_LOG_LOCK();
    can_log_flush_locked();
    CAN_LOG_UNLOCK();
}

uint32_t can_log_dropped_frames(void)
{
    return g_can_log_dropped_frames;
}

/* GCC/newlib：printf 最终调用 _write。 */
int _write(int file, char *data, int length)
{
    (void)file;
    if ((data != NULL) && (length > 0)) {
        can_log_write(data, (size_t)length);
    }
    return length;
}

/*
 * Keil MicroLIB/armcc 可改用：
 * int fputc(int ch, FILE *stream)
 * {
 *     const uint8_t byte = (uint8_t)ch;
 *     (void)stream;
 *     can_log_write(&byte, 1u);
 *     return ch;
 * }
 */
```

使用示例：

```c
printf("boot=%lu, reset=0x%08lX\r\n",
       (unsigned long)boot_count,
       (unsigned long)reset_reason);

printf("temperature=%d.%d C\r\n",
       temperature_tenth / 10,
       temperature_tenth % 10);
```

如果最后一条文本没有换行，应调用 `can_log_flush()`，否则不足一帧的字节会
继续留在缓存中。

## 5. STM32 bxCAN 适配示例（经典 CAN）

```c
#include "main.h"

extern CAN_HandleTypeDef hcan1;

bool board_can_try_send(uint32_t id,
                        const uint8_t *data,
                        uint8_t len,
                        bool is_fd)
{
    CAN_TxHeaderTypeDef header = {0};
    uint32_t mailbox;

    if (is_fd || (len > 8u) || (HAL_CAN_GetTxMailboxesFreeLevel(&hcan1) == 0u)) {
        return false;
    }

    header.StdId = id;
    header.IDE = CAN_ID_STD;
    header.RTR = CAN_RTR_DATA;
    header.DLC = len;

    return HAL_CAN_AddTxMessage(&hcan1,
                                &header,
                                (uint8_t *)data,
                                &mailbox) == HAL_OK;
}
```

发送前需完成 CAN 时钟、位时序、过滤器和 `HAL_CAN_Start()`。日志使用发送
邮箱，不需要为发送 ID 配置接收过滤器。

## 6. STM32 FDCAN 适配示例（CAN FD）

```c
#include "main.h"

extern FDCAN_HandleTypeDef hfdcan1;

static uint32_t fdcan_dlc(uint8_t length)
{
    switch (length) {
    case 0u:  return FDCAN_DLC_BYTES_0;
    case 1u:  return FDCAN_DLC_BYTES_1;
    case 2u:  return FDCAN_DLC_BYTES_2;
    case 3u:  return FDCAN_DLC_BYTES_3;
    case 4u:  return FDCAN_DLC_BYTES_4;
    case 5u:  return FDCAN_DLC_BYTES_5;
    case 6u:  return FDCAN_DLC_BYTES_6;
    case 7u:  return FDCAN_DLC_BYTES_7;
    case 8u:  return FDCAN_DLC_BYTES_8;
    case 12u: return FDCAN_DLC_BYTES_12;
    case 16u: return FDCAN_DLC_BYTES_16;
    case 20u: return FDCAN_DLC_BYTES_20;
    case 24u: return FDCAN_DLC_BYTES_24;
    case 32u: return FDCAN_DLC_BYTES_32;
    case 48u: return FDCAN_DLC_BYTES_48;
    default:  return FDCAN_DLC_BYTES_64;
    }
}

bool board_can_try_send(uint32_t id,
                        const uint8_t *data,
                        uint8_t len,
                        bool is_fd)
{
    FDCAN_TxHeaderTypeDef header = {0};

    if (!is_fd || (HAL_FDCAN_GetTxFifoFreeLevel(&hfdcan1) == 0u)) {
        return false;
    }

    header.Identifier = id;
    header.IdType = FDCAN_STANDARD_ID;
    header.TxFrameType = FDCAN_DATA_FRAME;
    header.DataLength = fdcan_dlc(len);
    header.ErrorStateIndicator = FDCAN_ESI_ACTIVE;
    header.BitRateSwitch = FDCAN_BRS_ON;
    header.FDFormat = FDCAN_FD_CAN;
    header.TxEventFifoControl = FDCAN_NO_TX_EVENTS;
    header.MessageMarker = 0u;

    return HAL_FDCAN_AddMessageToTxFifoQ(&hfdcan1,
                                         &header,
                                         data) == HAL_OK;
}
```

将 `CAN_LOG_USE_FD` 改为 `1`。PcanWork 通道的仲裁域和数据域波特率必须与
MCU 一致。总线上如果存在不支持 CAN FD 的节点，必须确认这些节点具有
FD-tolerant 能力，或继续使用经典 CAN 日志。

## 7. RTOS 与性能建议

- 多任务打印时，用互斥锁实现 `CAN_LOG_LOCK/CAN_LOG_UNLOCK`。
- 不要在中断服务程序中调用 `printf`。
- `board_can_try_send()` 应使用发送邮箱或有界队列，不得无限等待。
- 发布版可用编译开关关闭日志，避免格式化开销和总线占用。
- 浮点 `printf` 会显著增加固件体积和执行时间，优先发送定点值。
- 对日志限频；周期状态建议每 100～1000 ms 输出一次，而不是每个控制周期输出。
- 监视 `can_log_dropped_frames()`；非零表示发送邮箱或队列容量不足。

## 8. 联调验收

1. MCU 先每秒发送一次 `printf("CAN_LOG_TEST %lu\r\n", counter++)`。
2. PcanWork 确认通道已连接、测量已启动，报文表能看到配置的日志 ID。
3. 打开 CAN 文本日志，填写相同 ID 和通道，启用捕获。
4. 确认计数连续、中文 UTF-8 文本正常、长行能跨帧拼接。
5. 分别验证 `\n`、`\r\n`、无换行后手动 `can_log_flush()`。
6. 提高日志速率，确认业务报文时序和总线负载仍满足项目要求。
7. 断开或制造发送邮箱满，确认 `can_log_dropped_frames()` 能检测丢失。

若报文表能看到日志 ID、文本控制台却没有内容，请依次检查“捕获”、十六进制
ID、CAN 通道和报文方向。控制台只接收 Rx 报文，不会把本机 Tx 回环当作 MCU 日志。
