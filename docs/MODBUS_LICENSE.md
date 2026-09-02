# PcanWork 离线授权

PcanWork 与 Modbus Tools 共用同一份离线授权。未授权时，每次启动提供 60 分钟试用；倒计时结束后程序自动退出。Serial Tool 不受此授权限制。

## 签发 `.pcanlic`

私钥位于发布机的独立目录，不进入源码、Release 二进制或安装包：

`D:\_LicenseSecrets\PcanWork\pcanwork-ed25519-private.pem`

授权管理员在受控发布机运行：

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\sign-license.ps1 `
  -MachineCode "XXXX-XXXX-XXXX-XXXX" `
  -LicenseId "CUSTOMER-2026-001" `
  -Products pcanwork,modbus `
  -Features * `
  -ValidDays 0 `
  -OutputPath ".\CUSTOMER-2026-001.pcanlic"
```

`ValidDays 0` 表示永久授权。产品默认同时包含 `pcanwork` 与 `modbus`；功能默认使用通配符 `*`。签发工具只读取外部私钥，生成的 `.pcanlic` 只包含签名载荷和 Ed25519 签名。

临时延长授权可以按小时或天签发，例如绑定当前 CPU 延长 72 小时：

```powershell
.\tools\sign-license.ps1 -MachineCode "XXXX-XXXX-XXXX-XXXX" `
  -LicenseId "TEMP-72H" -ValidHours 72 -OutputPath ".\TEMP-72H.pcanlic"
```

不绑定 CPU 的超级授权仍需 Ed25519 私钥签名，可设置有效期或永久：

```powershell
# 所有电脑可用 30 天
.\tools\sign-license.ps1 -Unbound -LicenseId "PORTABLE-30D" `
  -ValidDays 30 -OutputPath ".\PORTABLE-30D.pcanlic"

# 所有电脑永久可用（高风险，仅内部受控使用）
.\tools\sign-license.ps1 -Unbound -LicenseId "SUPER-PERMANENT" `
  -OutputPath ".\SUPER-PERMANENT.pcanlic"
```

`-Unbound` 会在签名载荷中写入 `machine_code: "*"`。它不能被伪造，但可以被任意复制；永久超级授权一旦泄露就等同于所有电脑永久授权，因此不得放入源码、安装包、公开下载目录或客户通用资料。

## 客户端导入

1. 在 PcanWork 或 Modbus Tools 顶部点击试用倒计时。
2. 复制机器码并交给授权管理员。
3. 收到 `.pcanlic` 后点击“导入授权文件”。
4. 验证通过后文件安装到 `%LOCALAPPDATA%\PcanWork\license.pcanlic`，两款软件立即共用。

客户端只内置经过掩码处理的 Ed25519 公钥，不能生成新授权。授权绑定 CPU 机器码，可限制产品、功能和到期时间；修改载荷、签名或机器码都会导致验证失败。

## Release 完整性

Release 构建启用 LTO、单代码生成单元、符号删除与 `panic=abort`。发布时使用同一外部私钥分别签发 `pcanwork.exe.integrity` 和 `modbus-tools.exe.integrity`；程序启动时校验签名、产品、版本、文件名和 exe 的 SHA-256，不匹配时拒绝启动。

旧 HMAC 密码签发方式已停用，`scripts\generate-modbus-password.ps1` 会直接报错，避免误发旧授权。
