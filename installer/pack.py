#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""PcanWork 一键打包：自动递增版本号 → 编译 release(workspace) → 生成安装包。

用法:
    python installer/pack.py            # 递增 patch  (0.1.0 -> 0.1.1)  ← 默认
    python installer/pack.py minor      # 递增 minor  (0.1.3 -> 0.2.0)
    python installer/pack.py major      # 递增 major  (0.2.5 -> 1.0.0)
    python installer/pack.py --set 1.2.3   # 直接指定版本
    python installer/pack.py --no-bump     # 版本不动, 仅重新编译+打包
    python installer/pack.py --no-build    # 跳过 cargo 编译(用现有 release 二进制直接打包)

单一真相源 = 根 Cargo.toml 的 [package].version。本脚本同步到:
    Cargo.toml / serial/Cargo.toml / modbus/Cargo.toml / installer/pcanwork.iss
所有文件按 UTF-8 显式读写, 只用正则替换版本那一行, 中文注释不受影响。
"""
import re
import sys
import subprocess
import pathlib

# Windows 控制台默认 GBK, 直接 print 非 GBK 字符(如 › / ✓)会崩。统一改 UTF-8 输出。
for _s in (sys.stdout, sys.stderr):
    try:
        _s.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO_FILES = [ROOT / "Cargo.toml", ROOT / "serial" / "Cargo.toml", ROOT / "modbus" / "Cargo.toml"]
ISS = ROOT / "installer" / "pcanwork.iss"
ISCC = pathlib.Path(r"C:\Users\XCHARGE-2026Q1-LT08\AppData\Local\Programs\Inno Setup 6\ISCC.exe")

VER_RE = re.compile(r'(?m)^(version\s*=\s*")([0-9]+)\.([0-9]+)\.([0-9]+)(")')
ISS_RE = re.compile(r'(#define\s+AppVer\s+")([0-9]+)\.([0-9]+)\.([0-9]+)(")')


def read(p):
    return p.read_text(encoding="utf-8")


def write(p, s):
    # newline="" 保留文件原有换行风格, 不强制转换
    p.write_text(s, encoding="utf-8", newline="")


def current_version():
    m = VER_RE.search(read(ROOT / "Cargo.toml"))
    if not m:
        sys.exit("✗ 无法在根 Cargo.toml 中找到 [package].version")
    return int(m.group(2)), int(m.group(3)), int(m.group(4))


def bump(ver, kind):
    a, b, c = ver
    if kind == "major":
        return a + 1, 0, 0
    if kind == "minor":
        return a, b + 1, 0
    return a, b, c + 1  # patch


def set_version(verstr):
    a, b, c = verstr.split(".")
    repl_cargo = rf"\g<1>{a}.{b}.{c}\g<5>"
    for p in CARGO_FILES:
        s = read(p)
        s2, n = VER_RE.subn(repl_cargo, s, count=1)
        if n != 1:
            sys.exit(f"✗ {p.name}: 未能定位 version 行")
        write(p, s2)
    s = read(ISS)
    s2, n = ISS_RE.subn(rf"\g<1>{a}.{b}.{c}\g<5>", s, count=1)
    if n != 1:
        sys.exit("✗ pcanwork.iss: 未能定位 #define AppVer")
    write(ISS, s2)


def run(cmd):
    print(">", subprocess.list2cmdline([str(x) for x in cmd]), flush=True)
    r = subprocess.run(cmd, cwd=str(ROOT))
    if r.returncode != 0:
        sys.exit(f"✗ 命令失败 (exit {r.returncode})")


def main():
    args = sys.argv[1:]
    do_bump, do_build = True, True
    kind, forced = "patch", None
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--no-bump":
            do_bump = False
        elif a == "--no-build":
            do_build = False
        elif a == "--set":
            i += 1
            forced = args[i]
            do_bump = False
        elif a in ("major", "minor", "patch"):
            kind = a
        else:
            sys.exit(f"✗ 未知参数: {a}")
        i += 1

    old = current_version()
    if forced:
        new = tuple(int(x) for x in forced.split("."))
    elif do_bump:
        new = bump(old, kind)
    else:
        new = old
    verstr = ".".join(str(x) for x in new)

    if new != old:
        print(f"版本: {'.'.join(map(str, old))} -> {verstr}")
        set_version(verstr)
    else:
        print(f"版本保持: {verstr}")

    if do_build:
        run(["cargo", "build", "--release", "--workspace"])
    if not ISCC.exists():
        sys.exit(f"✗ 找不到 ISCC: {ISCC}")
    run([ISCC, ISS])

    out = ROOT / "installer" / "dist" / f"PcanWork-Setup-{verstr}.exe"
    if out.exists():
        mb = out.stat().st_size / 1024 / 1024
        print(f"\n✓ 安装包: {out}\n  体积: {mb:.2f} MB  (版本 {verstr})")
    else:
        sys.exit(f"✗ 预期产物未生成: {out}")


if __name__ == "__main__":
    main()
