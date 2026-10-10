#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""盘点产物的**系统依赖**：导入表、架构、子系统、主线程栈。

为什么要有它
------------
「下载下来双击没反应」这类问题，绝大多数要么是**依赖缺失**（某个 DLL 在用户机器上
没有），要么是**产物本身不合规**（架构不对、子系统写错、栈被改坏）。
这两类在开发机上**永远看不出来** —— 开发机早就把运行库、编译器、显卡驱动都装齐了。

所以把断言搬进 CI：编译完先跑这一步。它做四件事：

1. **列出全部导入 DLL**，并按"Windows 自带 / 需要额外安装 / 来路不明"分类；
   见到 VC++ 运行库、MinGW 运行库、MSYS 之类就直接失败。
2. **架构必须是 x64**。发出去的包名写着 `x64-windows`，编译参数退化成 x86
   会让一部分机器直接起不来。
3. **子系统必须对**：`wrepl-gui.exe` 要是 GUI(2)——否则双击先弹一个黑框，
   而 README 承诺的是"双击不弹控制台"；`wrepl.exe` 要是 CONSOLE(3)。
4. **主线程栈预留必须恰好是 1 MB**（`SizeOfStackReserve` = 1048576）。这条**方向与直觉
   相反**，务必看清 —— 它同时是"打不开"和"打开很慢"两个问题的同一个根。

   早先 v0.2.3 开发时曾把栈提到 16 MB，理由是"1 MB 不够用"，结果窗口**再也建不出来**
   （`NoGlutinConfigs(… kind: NotFound)`，干等约 38 s 后退出码 1）。
   单变量实验（同一份二进制，只改 PE 头里那个栈字段）后来把整条曲线量了出来：

   | 栈预留 | 进程起 → 主窗口出现 |
   |---|---|
   | **1.00 MB** | **2.2 s** |
   | 1.25 MB | 3.1 s |
   | 1.50 MB | 3.5 s |
   | 2.00 MB（mingw 默认） | 5.5 s |
   | 3.00 MB | 10.9 s |
   | 4.00 MB | 19.9 s |
   | 8 MB 及以上 | **窗口永远建不出来**（退出码 1） |

   把 MSVC 产物（1 MB，2.1 s）改成 2 MB，它同样掉到 5.5 s；把 GNU 产物（2 MB，5.5 s）
   改回 1 MB，它同样回到 2.2 s。⇒ **与工具链、LTO、业务代码都无关，就是这一个字段**；
   而 `msvc` 默认恰好 1 MB、`mingw` 默认 2 MB，只靠默认值就会给出"同版本、一快一慢"
   的两份产物。所以 `build.rs` 显式写死 1 MB，这一步负责让任何偏移当场变红。

用法：
    python .github/scripts/check_pe_imports.py <exe> [<exe> ...]

退出码：
    0  全部合格
    2  有产物不合格（依赖 / 架构 / 子系统 / 栈，或文件读不出来）
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

# ── ① 会直接失败：这些 DLL 要么得让用户装运行库，要么根本不该出现在发布产物里 ──

# VC++ 运行库。MSVC 目标默认动态链接 CRT，必须靠 `-C target-feature=+crt-static` 消掉。
VC_RUNTIME = {
    "vcruntime140.dll", "vcruntime140_1.dll", "vcruntime140d.dll",
    "msvcp140.dll", "msvcp140_1.dll", "msvcp140_2.dll", "msvcp140d.dll",
    "concrt140.dll", "vccorlib140.dll",
}

# MinGW / MSYS / 其它"编译机上恰好有、用户机器上未必有"的东西。
# 用 MSVC 目标 + 静态 CRT 时它们不该出现；一旦出现，说明有人换了工具链或加了
# 动态链接的系统库 —— 那种包发出去必然有人打不开。
FOREIGN_RUNTIME = {
    "libgcc_s_seh-1.dll", "libgcc_s_dw2-1.dll", "libstdc++-6.dll",
    "libwinpthread-1.dll",
    "msys-2.0.dll", "msys-64.dll",
    "zlib1.dll", "libzstd.dll", "liblzma-5.dll", "libbz2-1.dll",
    "libssl-3-x64.dll", "libcrypto-3-x64.dll",
}

# ── ② 判为"Windows 自带，不需要额外装" ──
#
# 只要不是这个集合里的、也不是 api-ms-win-* / ext-ms-* 这类 API set，就**失败**，
# 让人工确认一次再往前加。宁可误拦，也不要静默发一个"在别人机器上少个 DLL"的包
# —— 那正是这个脚本存在的理由。
SYSTEM_DLLS = {
    # 内核 / 基础
    "ntdll.dll", "kernel32.dll", "kernelbase.dll", "advapi32.dll",
    "bcrypt.dll", "bcryptprimitives.dll", "crypt32.dll", "sechost.dll",
    "rpcrt4.dll", "cfgmgr32.dll", "setupapi.dll", "version.dll", "psapi.dll",
    "powrprof.dll", "userenv.dll", "wtsapi32.dll", "dbghelp.dll",
    # 图形 / 窗口
    "user32.dll", "gdi32.dll", "gdi32full.dll", "dwmapi.dll", "imm32.dll",
    "uxtheme.dll", "winmm.dll", "msimg32.dll", "opengl32.dll", "shcore.dll",
    "d3d11.dll", "d3d12.dll", "dxgi.dll", "dcomp.dll", "windowscodecs.dll",
    "propsys.dll", "avrt.dll", "mfplat.dll", "mf.dll", "mfreadwrite.dll",
    # Shell / COM / 对话框
    "shell32.dll", "shlwapi.dll", "comdlg32.dll", "comctl32.dll",
    "ole32.dll", "oleaut32.dll", "combase.dll", "mpr.dll", "winspool.drv",
    # 网络（rfd / webbrowser 之类的传递依赖）
    "ws2_32.dll", "iphlpapi.dll", "dnsapi.dll", "netapi32.dll", "secur32.dll",
    # UCRT：Windows 10 起是系统组件，不算依赖（与旧版脚本的判定一致）
    "ucrtbase.dll",
}


def parse_pe(path: Path) -> dict:
    """纯标准库解析 PE：架构、子系统、主线程栈预留、导入 DLL 名单。"""
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

    subsystem = struct.unpack_from("<H", data, opt + 68)[0]
    stack_reserve = struct.unpack_from(
        "<Q" if pe32p else "<I", data, opt + 72
    )[0]

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

    names: list[str] = []
    off = rva2off(imp_rva) if imp_rva else None
    while off is not None:
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
    return {
        "arch": arch,
        "subsystem": subsystem,
        "stack_reserve": stack_reserve,
        "imports": sorted({n for n in names}),
    }


def classify(name: str) -> str:
    """`bad` / `ok` / `unknown`。"""
    low = name.lower()
    if low in VC_RUNTIME or low in FOREIGN_RUNTIME:
        return "bad"
    if low.startswith("api-ms-win-") or low.startswith("ext-ms-"):
        # OS 的 API set：由系统按版本转发，不需要额外安装
        return "ok"
    if low in SYSTEM_DLLS:
        return "ok"
    return "unknown"


# ★ 栈的合法值：**恰好 1 MB**。方向与直觉相反，理由是**代价**随预留区大小单调上升 ——
#   1 MB → 2.2 s；2 MB → 5.5 s；4 MB → 19.9 s；≥8 MB → 窗口永远建不出来。
#   msvc 默认 1 MB、mingw 默认 2 MB，所以两份"同一版本"的产物会一快一慢 2.4 倍。
#   `build.rs` 显式钉死 1 MB，这里断言成品就是这个数（见文件头第 4 条）。
EXPECTED_STACK = 1 * 1024 * 1024
STACK_TOLERANCE = 0            # 不留余量：这条曲线的代价是连续的，多一点都不该放行


def check(path: Path) -> int:
    print("=== %s ===" % path.name)
    if not path.is_file():
        print("  找不到文件：%s" % path, file=sys.stderr)
        return 1

    try:
        info = parse_pe(path)
    except Exception as exc:                             # noqa: BLE001
        print("  解析失败：%s" % exc, file=sys.stderr)
        return 1

    bad = 0
    imports = info["imports"]
    kinds = {n: classify(n) for n in imports}
    print("  架构：%s ｜ 子系统：%s ｜ 主线程栈预留：%.1f MB ｜ 导入 DLL %d 个"
          % (info["arch"], _subsystem_name(info["subsystem"]),
             info["stack_reserve"] / 1024 / 1024, len(imports)))
    for n in imports:
        mark = {"bad": "✗", "unknown": "?", "ok": " "}[kinds[n]]
        print("    %s %s" % (mark, n))

    # ① 架构
    if info["arch"] != "x64":
        print("  ✗ 架构不是 x64（%s）—— 包名写着 x64-windows，发 x86 会让部分机器直接起不来"
              % info["arch"])
        bad += 1

    # ② 子系统：文件名里带 gui 的必须是 GUI(2)，其余是 CONSOLE(3)
    want = 2 if "gui" in path.stem.lower() else 3
    if info["subsystem"] != want:
        print("  ✗ 子系统是 %s，期望 %s —— %s"
              % (_subsystem_name(info["subsystem"]), _subsystem_name(want),
                 "GUI 版必须是 GUI 子系统，否则双击会先弹一个黑框（README 承诺过不弹）"
                 if want == 2 else
                 "命令行版必须是控制台子系统，否则双击一闪而过、看不到任何输出"))
        bad += 1

    # ③ 主线程栈：**必须恰好 1 MB** —— 方向与直觉相反，理由见文件头第 4 条
    if abs(info["stack_reserve"] - EXPECTED_STACK) > STACK_TOLERANCE:
        print("  ✗ 主线程栈预留是 %.2f MB，期望 %.2f MB ——\n"
              "    这个值直接决定窗口多久能出来（本机实测）：\n"
              "      1 MB → 2.2 s ｜ 1.5 MB → 3.5 s ｜ 2 MB → 5.5 s ｜\n"
              "      3 MB → 10.9 s ｜ 4 MB → 19.9 s ｜ ≥8 MB → 窗口永远建不出来\n"
              "    调大**不是更保险，而是更慢甚至打不开**：glutin 会枚举不到任何 GL 配置\n"
              "    (`NoGlutinConfigs(… kind: NotFound)`)，用户看到的就是「双击没反应」。\n"
              "    msvc 默认恰好 1 MB、mingw 默认 2 MB，所以不能靠默认值 ——\n"
              "    检查 `build.rs` 里的 /STACK:1048576（msvc）或 -Wl,--stack,1048576（gnu）。"
              % (info["stack_reserve"] / 1024 / 1024, EXPECTED_STACK / 1024 / 1024))
        bad += 1

    # ④ 依赖
    vc = sorted(n for n in imports if n.lower() in VC_RUNTIME)
    foreign = sorted(n for n in imports if n.lower() in FOREIGN_RUNTIME)
    unknown = sorted(n for n in imports if kinds[n] == "unknown")

    if vc:
        print("  ✗ 仍依赖 VC++ 运行库：%s" % "、".join(vc))
        print("    → 说明 +crt-static 没生效。目标机器没装 Redistributable 时，")
        print("      用户双击就会报「找不到 %s」。" % vc[0])
        bad += 1
    if foreign:
        print("  ✗ 依赖了 MinGW / MSYS / 第三方运行库：%s" % "、".join(foreign))
        print("    → 这些在用户机器上默认都没有。检查是不是换错工具链了。")
        bad += 1
    if unknown:
        print("  ✗ 出现白名单外的 DLL：%s" % "、".join(unknown))
        print("    → 请确认它是不是 Windows 自带：")
        print("      · 是   → 加进本脚本的 SYSTEM_DLLS（连同「它从哪个 Windows 版本起自带」）")
        print("      · 不是 → 用户机器上大概率没有这个 DLL，双击就是「缺 xxx.dll」")
        bad += 1

    if bad == 0:
        print("  ✓ 依赖全部来自 Windows 自带；架构 / 子系统 / 主线程栈都合规")
    return bad


def _subsystem_name(v: int) -> str:
    return {2: "GUI(2)", 3: "CONSOLE(3)"}.get(v, str(v))


def main() -> int:
    args = sys.argv[1:]
    if not args:
        print("用法：check_pe_imports.py <exe> [<exe> ...]", file=sys.stderr)
        return 2

    bad = sum(check(Path(a)) for a in args)

    print()
    if bad:
        print("✗ %d 项不合格 —— 这个包发出去，有人会打不开" % bad)
        return 2
    print("✓ 全部产物合规：自带运行时、架构/子系统正确、主线程栈 = 1 MB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
