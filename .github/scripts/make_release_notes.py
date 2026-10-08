#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""从 CHANGELOG.md 抽出指定版本的「重点」摘要，拼成 GitHub Release 的正文。

Release 页面只放**重点**，不放整节明细 —— 明细在 CHANGELOG.md 里，正文给个链接过去。
所以每个版本小节的开头要有一组引用行（`> ` 开头），那就是要摘出来的东西：

    ## [0.1.3] - 2026-10-10

    > - 新增：……
    > - 修复：……

    ### 新增
    ……（这里可以写多长写多长，不会进 Release 正文）

CI 调的就是这个文件（release.yml 的「生成 Release 正文」一步），本机也能原样复跑，
所以在推 tag 之前就能先看一眼正文长什么样：

    python .github/scripts/make_release_notes.py \\
        --version v0.1.2 --repo xinmeiguo/wrepl --out dist/release-notes.md

退出码：
    0  生成成功
    2  CHANGELOG.md 里没有该版本的小节，或该小节没有「重点」摘要块
       （明确报错，不会静默发一个没有更新内容的 Release）

为什么是 Python 而不是 PowerShell：本机 PowerShell 沙箱起不来，PS 写的步骤没法在本地复跑验证；
这个脚本在 CI 上和在本机跑的是**同一个文件**，改完立刻能验。
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

DOWNLOAD_BLOCK = """## 下载

解压 `wrepl-*-x64-windows.zip`，里面是两个免安装的可执行文件：

| 文件 | 用途 |
|---|---|
| `wrepl.exe` | 命令行 |
| `wrepl-gui.exe` | 图形界面，双击即用 |

自带运行时，**不需要额外装 DLL，也不需要装 Word / Office**。
包内附 `README.md`、`LICENSE`、`CHANGELOG.md`，以及同目录的 `.sha256` 校验值。"""


def code_fence_flags(lines: list[str]) -> list[bool]:
    """标记每行是否落在围栏代码块（``` / ~~~）里。

    本文件顶部的使用说明里会写 `## [0.1.3]` 这种**示例**标题；
    不把这层屏蔽掉，等真发 v0.1.3 的时候，脚本会优先抽到说明里的示例
    （它位置更靠前，还会先被匹配上）。
    """
    flags: list[bool] = []
    inside = False
    for line in lines:
        if re.match(r"^\s*(```|~~~)", line):
            flags.append(True)      # 围栏行本身也算代码块内
            inside = not inside
        else:
            flags.append(inside)
    return flags


def iter_heading_lines(lines: list[str], in_code: list[bool]):
    """产出所有二级标题（`## `）的行号。`### ` 不算 —— 那是版本小节里的分组标题。"""
    for i, line in enumerate(lines):
        if not in_code[i] and re.match(r"^##\s", line):
            yield i


def extract_section(text: str, version: str) -> str:
    """取 `## [<version>]` 那一节的正文（不含标题行本身）。

    接受两种写法：`## [0.1.3] - 2026-10-10` 和 `## v0.1.3`。
    """
    ver = version.strip().lstrip("vV")
    lines = text.split("\n")
    in_code = code_fence_flags(lines)

    head = re.compile(r"^##\s*\[?\s*v?" + re.escape(ver) + r"\s*\]?(\s|$|[-–—:：])")
    start = next(
        (i for i, line in enumerate(lines) if not in_code[i] and head.match(line)), None
    )
    if start is None:
        return ""

    # 小节范围 = 本标题之后，到下一个二级标题之前
    stop = next((i for i in iter_heading_lines(lines, in_code) if i > start), len(lines))
    body = lines[start + 1:stop]

    # 版本之间的分隔线（`---`）落在上一节的范围里，它不是本节内容，去掉；
    # 否则拼进正文后会和「下载」前的分隔线叠成两条。
    def strip_tail() -> None:
        while body and (not body[-1].strip() or re.fullmatch(r"\s*[-*_]{3,}\s*", body[-1])):
            body.pop()

    strip_tail()
    while body and not body[0].strip():
        body.pop(0)
    return "\n".join(body)


def extract_highlights(section: str) -> list[str]:
    """取小节**开头**那一组连续的引用行（`> `）作为「重点」。

    必须紧贴小节标题，中间隔了正文就一律不认 —— 免得抓错正文里偶然出现的引用。
    返回的行保留原文（含 `>`），直接拼进 Release 正文。
    """
    out: list[str] = []
    for line in section.split("\n"):
        if line.lstrip().startswith(">"):
            out.append(line.rstrip())
        elif out:
            break          # 引用块到此为止
        elif line.strip():
            break          # 开头就是正文 —— 没有摘要块
    return out


def build_body(text: str, version: str, repo: str) -> str:
    section = extract_section(text, version)
    if not section:
        raise LookupError(
            "CHANGELOG.md 里找不到 [%s] 小节。\n"
            "发版前请先在 CHANGELOG.md 顶部补一节，标题写成：\n"
            "    ## [%s] - <发布日期>\n"
            "然后连这次改动一起提交、**再**打 tag（tag 指向的提交里必须已经带上这一节）。"
            % (version, version.strip().lstrip("vV"))
        )

    highlights = extract_highlights(section)
    if not highlights:
        raise LookupError(
            "CHANGELOG.md 的 [%s] 小节没有「重点」摘要块。\n"
            "Release 正文只摘重点，所以在小节标题下面要写一组引用行，例如：\n"
            "    ## [%s] - <发布日期>\n"
            "\n"
            "    > - 新增：……\n"
            "    > - 修复：……\n"
            "\n"
            "写在小节里的普通内容（`### 新增` 那些明细）不会进 Release 正文，可以尽管写细。"
            % (version, version.strip().lstrip("vV"))
        )

    return "\n".join(
        [
            "## 更新内容",
            "",
            *highlights,
            "",
            "查看完整改动：[`CHANGELOG.md`]"
            "(https://github.com/%s/blob/%s/CHANGELOG.md)" % (repo, version),
            "",
            "---",
            "",
            DOWNLOAD_BLOCK,
            "",
        ]
    )


def main() -> int:
    ap = argparse.ArgumentParser(description="生成 GitHub Release 正文")
    ap.add_argument("--version", required=True, help="tag 名，例如 v0.1.2")
    ap.add_argument("--repo", required=True, help="owner/repo")
    ap.add_argument("--out", required=True, help="输出文件路径")
    ap.add_argument("--changelog", default="CHANGELOG.md", help="更新日志路径")
    args = ap.parse_args()

    path = Path(args.changelog)
    if not path.is_file():
        print("找不到 %s" % path, file=sys.stderr)
        return 2

    # utf-8-sig：容忍 BOM；再把 CRLF 归一成 LF，免得正文里带 \r
    text = path.read_text(encoding="utf-8-sig").replace("\r\n", "\n").replace("\r", "\n")

    try:
        body = build_body(text, args.version, args.repo)
    except LookupError as exc:
        print(str(exc), file=sys.stderr)
        return 2

    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    # newline="\n"：正文按 LF 写，跨平台一致
    with out.open("w", encoding="utf-8", newline="\n") as fh:
        fh.write(body)

    print("已生成 %s（%s）——%d 字符" % (out, args.version, len(body)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
