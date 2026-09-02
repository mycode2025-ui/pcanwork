# PcanWork product remediation audit

This document is the completion checklist for PcanWork, Modbus Tools, and
Serial Tool. A row is complete only when the implementation and the evidence
gate both pass. “Implemented” alone is not release evidence.

## Release trust boundary

| Requirement | Current implementation | Release evidence |
|---|---|---|
| No private/test key in source or production | Tracked PEM fixtures were removed; tests generate per-run ephemeral CA/server/client identities; build and installer never copy TLS identities | Secret scan, source-package inspection, installed-file inspection |
| Stable Windows identity | Inno Setup has a stable `AppId` and version metadata | Upgrade install over previous build |
| Signed executables, installer and uninstaller | `installer/build-signed-release.ps1` requires a valid code-signing certificate with private key and RFC3161 timestamping | Authenticode status must be `Valid` for all four artifacts |
| Reproducible source root | Installer sources are relative to the repository | Run signed build outside the original developer profile |

No release may be called signed while the code-signing certificate is absent.
The unsigned `installer/dist` artifact is not release evidence.

## Runtime isolation and loss policy

| Path | Queue | Policy | Visible evidence |
|---|---:|---|---|
| CAN hardware → UI | 1024 events, 64 control reserve | Drop complete frame batch before blocking hardware poll; count every frame | RX depth/capacity/high-water/drop in status |
| CAN UI → controller | 512 commands | Non-blocking reject; operation is not executed | Command depth/capacity/high-water/reject in status and serious log |
| CAN frames → recorder | 256 frame batches, 16 control | Fail recording on full queue; never silently omit recorded frames | Recorder depth/capacity/high-water/drop and failure event |
| Python IPC → CAN UI | 64 requests | Single-client request/response; reject excess work with `BUSY` | Client receives an explicit error and the operation is not executed |
| Python process → runner UI | 4096 lines | Drop excess output without blocking stdout/stderr readers | Run is marked failed and reports dropped/total lines |
| Serial readers → UI | 4096 events, 64 control reserve | Drop receive bytes before control reserve; count bytes/events | Queue depth/high-water/drop in status and receive notice |
| Serial UI → writer | 256 jobs | Non-blocking reject with explicit error | Writer depth/high-water/rejected bytes |
| Modbus UI → backend | 512 commands | Non-blocking reject | Runtime command watermark and reject status |
| Modbus master/slave control | 256 commands per engine | Non-blocking reject | Active engine status reports rejection |
| Modbus raw traffic | 1024 chunks per connection | Drop monitor-only chunks; protocol remains live | Traffic view reports dropped chunks/bytes/high-water |
| Modbus slave protocol events | 1024 events | Drop display/log event; protocol response remains live | Slave status/event log reports drop/high-water |

Modbus traffic and slave logs are pushed to Slint in 50 ms batches. CAN and
Serial timers are both bounded by event count and an 8 ms UI time budget.

## Protocol correctness

### CAN

- PCAN, ControlCAN/VCI, and ZCAN hardware timestamps are mapped into one
  monotonic process timeline with documented device units and counter-wrap
  handling.
- PCAN queue/receive overrun and bus-state errors are counted.
- VCI/ZCAN driver error sentinels no longer create phantom zero frames.
- Repeated hardware-removal errors are confirmed over three consecutive polls;
  capture and periodic transmission stop, the connection state clears, and the
  UI remains alive with a channel-specific diagnostic.
- Cross-adapter validation rejects duplicate software channels, duplicate
  physical endpoints, unsupported FD/classic combinations, invalid
  adapter-specific bit rates, and invalid network IP/port values before the
  active connection is replaced.
- Timestamp quality exposes sample count, current/max transport jitter, clock
  drift in ppm, and monotonicity violations.

### Modbus

- FC01/02: 1..2000 bits.
- FC03/04: 1..125 registers.
- FC05/06: exactly one value.
- FC15: 1..1968 coils.
- FC16: 1..123 registers.
- FC23: 1..125 read registers and 1..121 write registers.
- Every address plus quantity is checked against the 0..65535 address space.
- UI constraints and backend validation are independent defenses.
- TCP traffic uses MBAP length validation and split/sticky-frame reassembly.

### Serial

- Terminal transmit honors UTF-8, GBK, GB18030, ISO-8859-1, and ASCII without
  lossy replacement.
- Binary files are read as bytes; non-UTF-8 data receives a bounded hex preview.
- Multiline paste preserves internal CR/LF and appends the selected line ending
  exactly once to the entire byte block.
- `#` has no local comment semantics and is transmitted as a normal byte.
- Receive recording contains only bytes actually received from the peer.

## Recording and persistence

- BLF recording writes recoverable containers incrementally and checkpoints
  header metadata; it no longer retains the entire capture in memory.
- CSV/ASC/BLF writes run on a dedicated recorder thread.
- ASC `date` is generated from the current local date.
- Recorder and Modbus logger stop with a visible diagnostic after a write
  failure such as disk full.
- CAN signal CSV logging checkpoints once per second and reports header,
  streaming write, periodic flush, and final flush failures.
- Workspace, value-name, grid CSV, server CSV, and chart exports report the
  actual filesystem result. A file-dialog selection is not treated as success.

## Shared UI system

`ui/design-system.slint` is the source of truth for:

- light/dark brand and semantic colors;
- Segoe UI Variable / Microsoft YaHei UI / Noto Sans CJK font stack;
- Cascadia Mono / Consolas / DejaVu Sans Mono terminal stack;
- 11/12/14/16/20 px type scale;
- four-pixel spacing scale;
- row/control heights and radii;
- success/warning/danger semantics;
- shared language state and translation helper;
- accessible status semantics and keyboard focus tokens.

Application-specific components may wrap these tokens, but must not introduce
new status meanings or hard-coded substitute palettes. The three production UI
trees contain no literal colors outside `ui/design-system.slint`; terminal,
scrim, selection, status, shadow, and on-brand colors are shared tokens.

Actual `slint-viewer` screenshots cover the CAN window, Serial normal mode,
Serial terminal mode with a pasted multiline command block, Modbus Chinese
light mode, and Modbus English dark mode. Modbus also synchronizes
programmatically restored `dark` and `lang-en` properties to the shared theme,
standard-widget palette, and language state.

## Product gates

`scripts/run-product-gates.ps1` is the authoritative local gate:

1. Rust formatting and private-key/palette boundary scans.
2. All three Slint syntax checks and stable render evidence.
3. Workspace/all-target tests, zero-warning Clippy, and the vendored
   text-selection regression.
4. A real-time 24-hour capture queue soak (default 20,000 frames/s) with
   sent/received equality and zero reported loss.
5. Single-job release workspace build.
6. Optional mandatory signed release/installer path when a certificate
   thumbprint is supplied.

Additional hardware-lab evidence still required before declaring the overall
goal complete:

- hot-unplug/replug on every supported adapter family;
- bus-off and driver-overrun injection;
- timestamp drift/jitter capture against a reference clock;
- disk-full recording recovery on NTFS;
- signed upgrade install and uninstall on a clean Windows machine.

## Latest local evidence

Run through 2026-07-27 with one Cargo job:

- workspace/all-target tests: 117 passed, 0 failed, 1 ignored (the explicit
  24-hour gate);
- vendored Slint visible-selection regression: 1 passed, 0 failed;
- Clippy workspace/all-target audit with `-D warnings`: passed;
- formal 24-hour capture-queue soak at 20,000 frames/s: passed with exact
  sent/received equality and zero reported loss;
- Rust formatting, all three Slint syntax checks, source private-key scan,
  literal-palette boundary, and unbounded-queue boundary: passed;
- Chinese/light and English/dark Modbus renders plus Serial normal/terminal
  and CAN renders were visually inspected.
- custom CAN, Serial, and Modbus controls use zero-width keyboard focus scopes,
  so their first pointer click reaches the action target instead of only
  transferring focus; real one-click interaction screenshots cover one control
  in every application;
- version 0.1.17 passed the full workspace Release build with ThinLTO,
  `opt-level = 3`, one codegen unit, symbol stripping, and abort-on-panic;
- version 0.1.18 exposes the printf-over-CAN text console in the main CAN UI,
  embeds the MCU integration guide, and passed the full workspace Release build
  with ThinLTO and one Cargo job;
- the unsigned validation installer was built and installed over the previous
  version; all three installed executable versions are 0.1.15, their SHA-256
  hashes exactly match the Release outputs, and each opened a real main window
  during the installed smoke test.

A signed release remains pending until a valid code-signing certificate with
private key is installed.
