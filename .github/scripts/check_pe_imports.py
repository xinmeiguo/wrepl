#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""校验编译出来的 exe **不依赖** VC++ 运行库（VCRUNTIME140.dll 之类）。

为什么要有它
------------
MSVC 目标**默认动态**链接 CRT，产物因此要求目标机器装有
Visual C++ 2015-2022 Redistributable；没装的机器上双击就报
「找不到 VCRUNTIME140.dll」「应用程序无法正常启动 (0xc000007b)」。

而我们的 README 与 Release 正文写的是「自带运行时，**不需要额外装 DLL**」。
这个不一致在开发机上**永远看不出来** —— 开发机早就装过运行库了。
2026-10-08 的外部用户反馈正是从这条缝隙里漏出去的。

所以把断言搬进 CI：`release.yml` 编译完先跑这一步，导入表里还有 VC++ 运行库
就直接失败。以后谁动了编译参数，退化了当场就红，不会再发到用户手上。

★ 注意别把 UCRT 也算进来：`ucrtbase.dll` / `api-ms-win-crt-*.dll` 从 Windows 10
  起是**系统组件**，自带，不构成依赖问题。那种"见到 crt 就报错"的粗判会把
  正常的产物也拦下。

用法：
    python .github/scripts/check_pe_imports.py <exe> [<exe> ...]

退出码：
    0  全部干净
    2  有产物仍依赖 VC++ 运行库（或文件读不出来）
"""

from __future__ import annotations

import struct
import sys
from pathlib import Path

# ★ Windows 的 GitHub runner 上，Python 的 stdout 默认是 cp1252（不是 UTF-8），
#   打印中文会直接 UnicodeEncodeError 崩掉。2026-10-08 这个脚本第一次在 CI 上跑
#   就死在这里，而报错现场长得像"断言没通过"，极容易误判成 +crt-static 没生效。
#   显式切 UTF-8，让本机与 CI 的行为一致。
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except Exception:  # noqa: BLE001  老 Python 或非 TextIOWrapper
        pass

# ★ 这几个才是"要装 Redistributable，否则起不来"的
VC_RUNTIME = {
    "vcruntime140.dll", "vcruntime140_1.dll", "vcruntime140d.dll",
    "msvcp140.dll", "msvcp140_1.dll", "msvcp140_2.dll", "msvcp140d.dll",
    "concrt140.dll", "vccorlib140.dll",
}


def parse_pe_imports(path: Path) -> tuple[str, list[str]]:
    """返回 (架构, 导入的 DLL 名列表)。纯标准库，不依赖 dumpbin/objdump。"""
    data = path.read_bytes()
    if data[:2] != b"MZ":
        raise ValueError("不是 PE 文件（缺 MZ）")
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    if data[e_lfanew:e_lfanew + 4] != b"PE\0\0":
        raise ValueError("不是 PE 文件（缺 PE\\0\\0）")

    coff = e_lfanew + 4
    machine, nsec = struct.unpack_from("<HH", data, coff)
    size_opt = struct.unpack_from("<H", data, coff + 16)[0]
    opt = coff + 20
    pe32p = struct.unpack_from("<H", data, opt)[0] == 0x20B

    # DataDirectory[1] = Import Table
    imp_rva, _ = struct.unpack_from("<II", data, opt + (112 if pe32p else 96) + 8)

    sec_off = opt + size_opt
    sections = []
    for i in range(nsec):
        base = sec_off + i * 40
        vsize, vaddr, rawsize, rawptr = struct.unpack_from("<IIII", data, base + 8)
        sections.append((vaddr, vsize, rawptr, rawsize))

    def rva2off(rva: int) -> int | None:
        for vaddr, vsize, rawptr, rawsize in sections:
            if vaddr <= rva < vaddr + max(vsize, rawsize):
                return rawptr + (rva - vaddr)
        return None

    off = rva2off(imp_rva)
    if off is None:
        return "?", []

    names: list[str] = []
    while True:
        ent = data[off:off + 20]
        if len(ent) < 20 or ent == b"\0" * 20:
            break
        name_rva = struct.unpack_from("<I", data, off + 12)[0]
        no = rva2off(name_rva)
        if no is None:
            break
        end = data.index(b"\0", no)
        names.append(data[no:end].decode("ascii", "replace"))
        off += 20

    arch = {0x8664: "x64", 0x14C: "x86", 0xAA64: "arm64"}.get(machine, hex(machine))
    return arch, names


def main() -> int:
    args = sys.argv[1:]
    if not args:
        print("用法：check_pe_imports.py <exe> [<exe> ...]", file=sys.stderr)
        return 2

    bad = 0
    for raw in args:
        p = Path(raw)
        print("=== %s ===" % p.name)
        if not p.is_file():
            print("  找不到文件：%s" % p, file=sys.stderr)
            bad += 1
            continue
        try:
            arch, names = parse_pe_imports(p)
        except Exception as exc:                       # noqa: BLE001
            print("  解析失败：%s" % exc, file=sys.stderr)
            bad += 1
            continue

        hits = sorted({n for n in names if n.lower() in VC_RUNTIME})
        print("  架构：%s ｜ 导入 DLL %d 个" % (arch, len(set(names))))
        if hits:
            print("  ✗ 仍依赖 VC++ 运行库：%s" % "、".join(hits))
            print("    → 说明 +crt-static 没生效。目标机器没装 Redistributable 时，")
            print("      用户双击就会报「找不到 %s」。" % hits[0])
            bad += 1
        else:
            print("  ✓ 不依赖 VC++ 运行库（UCRT 属 Windows 10+ 自带，不算依赖）")

    print()
    if bad:
        print("✗ %d 个产物不合格 —— 「不需要额外装 DLL」这句承诺没有兑现" % bad)
        return 2
    print("✓ 全部产物自带运行时")
    return 0


if __name__ == "__main__":
    sys.exit(main())
