# ZLG USBCAN-E-U driver dependency

This directory contains the signed ZLG USBCAN-E-U/USBCAN-2E-U Windows driver
package for USB devices `VID_0471&PID_1260` and `VID_0471&PID_1261`.

- Provider: ZLGMCU DEVELOPMENT Co., LTD / Guangzhou Zhiyuan Electronics
- Driver version: 2.3.0.6 (2020-03-26)
- Architecture: AMD64 (with the vendor's required 32-bit companion DLL)
- Catalog: `usbcan_e_u.cat` (valid Microsoft Windows Hardware Compatibility signature)

PcanWork opens types 20/21 through `zlgcan.dll`. That library loads
`USBCAN_E_64.DLL` from `kerneldlls` according to `dll_cfg.ini`; PcanWork does
not call the dependency directly. The installer also stages the complete
signed INF package through `pnputil`, so a clean Windows machine receives the
matching kernel driver.
