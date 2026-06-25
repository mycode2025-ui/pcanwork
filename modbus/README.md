# Modbus Tools (Slint + Rust)

A modern reimplementation of **Modbus Poll** (master / client) and **Modbus Slave**
(slave / server simulator) as a single cross‑platform desktop app, built with
[Slint](https://slint.dev) for the UI and [`tokio-modbus`](https://crates.io/crates/tokio-modbus)
for the protocol stack.

One window, two modes:

| Mode | Mirrors | What it does |
|------|---------|--------------|
| **Master · Poll** | Modbus Poll | Connects to a slave device and continuously polls a register/coil range, displaying live values; can write single registers/coils. |
| **Slave · Simulator** | Modbus Slave | Emulates a slave device, serving requests from masters; register/coil values are shown in an editable grid and update live as masters read/write. |

Both modes support **Modbus TCP** and **Modbus RTU** (serial).

## Features

- **Transports:** Modbus TCP (client & server) and Modbus RTU over serial
  (baud / data bits / parity / stop bits configurable). 1000 ms response timeout.
- **Function codes:** Read Coils (FC01), Read Discrete Inputs (FC02),
  Read Holding Registers (FC03), Read Input Registers (FC04),
  Write Single Coil/Register (FC05/FC06), Write Multiple Coils/Registers (FC15/FC16).
- **Write dialog:** pick the write function (05/06/15/16), address and value(s);
  multiple‑writes take a comma‑separated list. Click any value cell to pre‑fill the
  address.
- **Display formats (29):** Signed, Unsigned, Hex, Binary, ASCII‑Hex, plus
  32/64‑bit signed & unsigned integers, 32‑bit float and 64‑bit double — each in
  all four Modbus word/byte orders (ABCD / CDAB / BADC / DCBA).
- **Editable Name column:** per‑address labels in both master and slave grids.
- **Communication Traffic monitor:** the bottom panel toggles between the event
  log and the raw ADU bytes (hex) actually sent/received on the wire.
- **Address Scan / Slave Scan:** probe an address range or a slave‑ID range and
  list which responded (with value), which returned an exception, and which timed out.
- **Master:** adjustable scan rate, Tx/Rx/Error counters, live status, event log.
- **Slave:** full 16‑bit address space per table, in‑grid editing (type a value,
  press **Enter**), request counter, request log. Unit‑ID filtering on RTU
  (broadcasts to id 0 are processed silently).
- Modern Fluent‑styled UI with a mode switcher, card‑based config panels, a data
  grid, modal write/scan dialogs and a traffic log.

## Build & run

Requires a stable Rust toolchain (built with 1.95).

```sh
cargo run --release
```

Run the tests (includes a real in‑process TCP master↔slave round‑trip):

```sh
cargo test
```

## Quick self‑contained demo (no hardware needed)

A single instance can act as both server and client simultaneously — the slave
server keeps running in the background after you switch the UI to Master mode.

1. Start the app, click **Slave · Simulator**.
2. Mode = *Modbus TCP*, Port = `502` (or any free port), Table = *Holding
   Registers*, then **Start server**.
3. Edit a few Value cells and press **Enter** to seed some values.
4. Click **Master · Poll**. Mode = *Modbus TCP*, Host = `127.0.0.1`,
   Port = `502`, Function = *Read Holding Registers*, then **Connect & Poll**.
5. The grid shows the live values; counters tick up. Use the **Write single**
   panel to push a value back — switch to Slave mode and watch the cell change.

> On Windows, binding TCP port `502` does not require administrator rights.
> For RTU testing without hardware, pair two virtual COM ports (e.g. com0com).

## Architecture

```
src/
  main.rs       Slint window + callback wiring (UI thread)
  protocol.rs   Domain types: Area, Transport, DataStore (slave memory)
  format.rs     Value rendering / parsing for the data grid
  backend.rs    Tokio runtime on a background thread:
                  • controller — owns master/slave lifecycles, dispatches Cmd
                  • master engine — polls + writes via tokio-modbus client
                  • slave engine — tokio-modbus server + shared DataStore
                  • UiSink — pushes updates back via slint::Weak::upgrade_in_event_loop
ui/
  app.slint     Declarative UI (mode switcher, forms, DataGrid, LogPanel)
```

The UI thread and the async backend communicate over an `mpsc` command channel
(UI → backend) and a Slint weak handle (backend → UI), so the event loop never
blocks on I/O.

## Slave simulator

- **Register simulation:** *Simulate…* animates the displayed table/range —
  Increment, Decrement, Random or Toggle, at a configurable step, min/max and
  interval. Masters read the changing values live; values also feed the slave's
  chart.
- **Raw traffic monitor:** the slave's bottom panel toggles Event Log / Communication
  Traffic / Chart. Traffic shows the raw ADU bytes captured on each TCP connection.
- **Full write function set:** the slave answers FC05/06/15/16 plus **FC22 Mask
  Write Register** and **FC23 Read/Write Multiple Registers**.
- **Ignore Unit ID:** TCP slaves can answer any unit id (default) or filter to the
  configured id; RTU always filters (broadcasts to id 0 are processed silently).
- **Display features:** conditional colours, scaling and value-name maps apply to
  the slave grid too (the same dialogs act on whichever mode is active).

## Long-term acquisition

- **Multiple poll windows (MDI):** the tab bar above the grid hosts any number of
  independent poll windows, each with its own connection, read definition, display
  settings and live state. They all keep polling in the background; switching tabs
  restores that window's config and last view. `+ New` clones the current settings;
  `✕` closes a tab; **⧉ Float** pops the active window out into its own top-level
  window (a live monitor with grid + log/traffic/chart) so several polls can be
  watched side by side.
- **Live read definition:** changing Function / Address / Quantity / Scan rate while
  connected re-polls immediately — no reconnect needed.
- **Data logging:** *Log…* records the active window's polls to a text/CSV file —
  each read or every N seconds, comma or tab delimited, with an optional timestamp
  and a "log only on change" filter. Rows are appended live.

## Visualization & persistence

- **Real-time charts:** the bottom "Chart" tab shows one mini chart per register
  (up to 12), each auto-scaled to its own Y range over a 120-sample sliding window,
  in a scrolling list.
- **Conditional colours:** *Colors…* sets two rules (=, >, <, >=, <=, bitwise AND)
  with a value and a colour each; matching value cells are recoloured (rule 1 wins).
- **Scaling:** *Scaling…* applies a linear transform Y = m·(X − X1) + Y1 with a
  configurable decimal precision to numeric register formats.
- **Value annotations:** *Value Names…* maps register values to labels
  (`0=close`, `1=open`, …); when enabled the grid shows `value (label)` — e.g.
  `1 (open)` — so the meaning sits next to the number. Edit inline or import/export
  a `value=name` `.txt` file.
- **Editable name column:** type a register label and press Enter. The name column
  is driven by a stable model so live value refreshes never clobber in-progress
  typing; names round-trip through *Export/Import CSV*.
- **Workspace save/load:** *Save/Open Workspace* writes/reads a `.mbw` text file
  with the full master + slave configuration, scaling and colour rules (native
  file dialogs).
- **CSV register template:** *Export CSV* writes a round-trippable
  `Import,Address,Name,Value` template; *Import CSV* loads it back, applying only
  rows flagged `Import=yes` — setting names (and, in slave mode, register values).

## Implemented from the Modbus Poll manual

Read/Write definition, all read FCs, Write Single/Multiple (05/06/15/16) with a
write dialog, the full display-format matrix with word/byte orders, editable name
cells, the Communication Traffic (raw ADU byte) monitor, Tx/Rx/error counters,
Modbus exception reporting, Address Scan / Slave Scan, real-time charting,
conditional colours, scaling, workspace save/load and CSV export.

## Roadmap (not yet implemented)

- Connection types beyond TCP/RTU (UDP, TCP/Security, RTU/ASCII over TCP/UDP),
  Modbus ASCII mode, advanced serial control (RTS/DSR/CTS/DTR).
- Master-issued advanced function codes (08 Diagnostics, 11 Comm Event Counter,
  17 Report Server ID, 43/14 Device ID). (The slave already answers 22 and 23.)
- Excel logging and the OLE/COM automation interface.
