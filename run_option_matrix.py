#!/usr/bin/env python3
"""选项矩阵回归：把每个开关的差异放大成可数的命中次数，逐项对照 Word 语义。

用法：
    python run_option_matrix.py <wrepl.exe 路径> [samples 目录]

`samples 目录` 默认取脚本同级的 `samples/`（与 regress.sh 的 $SAMPLES 默认一致），
里面要有 `T-选项矩阵.docx`。它含客户信息的可能性极低，但按本仓库惯例一律
不入版本库（见 .gitignore 的 `/samples/`），所以要自备或从 regress.sh 传进来。

本脚本**完全通用**：不含任何本机路径与真实客户数据，吃一个 exe 路径就能跑，
所以跟着 regress.sh 一起进版本库（受 regress.sh 第 3 组调用）。
"""
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

if len(sys.argv) < 2:
    sys.exit("用法：python run_option_matrix.py <wrepl.exe 路径> [samples 目录]")

WREPL = Path(sys.argv[1])
SAMPLES = Path(sys.argv[2]) if len(sys.argv) > 2 else HERE / "samples"
SAMPLE = SAMPLES / "T-选项矩阵.docx"

# (说明, 期望命中数, 规则参数)
# 样本可见文本：
#   #0 saddow sasdf sa
#   #1 大小写：abc ABC Abc
#   #2 全半角：ＡＢＣ 与 ABC
#   #3 全角空格：甲　乙        （甲乙之间是 U+3000 全角空格）
#   #4 本段用于对照，不含目标词
CASES = [
    ("全字匹配 sa —— 默认（非全字）",           3, ["--rule", "sa=>X"]),
    ("全字匹配 sa —— --whole-word",            1, ["--rule", "sa=>X", "--whole-word"]),

    ("大小写 abc —— 默认（不区分大小写/全半角）", 5, ["--rule", "abc=>Y"]),
    ("大小写 abc —— --case-sensitive",         1, ["--rule", "abc=>Y", "--case-sensitive"]),

    ("全半角 ABC —— 默认（全半角等价）",          5, ["--rule", "ABC=>Z"]),
    ("全半角 ABC —— --kana-sensitive",          4, ["--rule", "ABC=>Z", "--kana-sensitive"]),
    ("全半角 ABC —— --case-sensitive --kana-sensitive", 2,
                                                ["--rule", "ABC=>Z", "--case-sensitive", "--kana-sensitive"]),

    ("空格 甲<全角>乙 规则全角 —— 默认",           1, ["--rule", "甲\u3000乙=>W"]),
    ("空格 甲<全角>乙 规则全角 —— --kana-sensitive", 1, ["--rule", "甲\u3000乙=>W", "--kana-sensitive"]),
    ("空格 甲<半角>乙 规则半角 —— 默认（折叠命中）",  1, ["--rule", "甲 乙=>W"]),
    ("空格 甲<半角>乙 规则半角 —— --kana-sensitive（不命中）", 0, ["--rule", "甲 乙=>W", "--kana-sensitive"]),
]

HIT_RE = re.compile(r"命中\s+(\d+)")


def run(args):
    p = subprocess.run([str(WREPL), "scan", str(SAMPLE), *args],
                       capture_output=True, text=True, encoding="utf-8", errors="replace")
    out = (p.stdout or "") + (p.stderr or "")
    m = HIT_RE.search(out)
    return int(m.group(1)) if m else None, out


def main():
    if not SAMPLE.exists():
        sys.exit(f"样本不存在：{SAMPLE}")
    print(f"wrepl：{WREPL}")
    print(f"样本 ：{SAMPLE.name}\n")
    print(f"{'检查项':<40} {'期望':>4} {'实际':>4}  判定")
    print("-" * 66)
    bad = 0
    for name, expect, args in CASES:
        got, out = run(args)
        ok = (got == expect)
        bad += 0 if ok else 1
        print(f"{name:<40} {expect:>4} {str(got):>4}  {'PASS' if ok else 'FAIL'}")
        if not ok:
            print(out)
    print("-" * 66)
    print(f"合计：{len(CASES) - bad}/{len(CASES)} 通过")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
