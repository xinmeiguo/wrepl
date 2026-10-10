"""比对两份 .xlsx 报告：逐 sheet、逐单元格，忽略「报告生成时间」那一格。

    python bench/xlsx_diff.py A.xlsx B.xlsx

只可用同一份输入下两次运行的报告互相比较——时间戳本来就是变量。
"""
import sys
import re
import openpyxl

# 这几处**本来就该不同**，不是回归：
#   报告生成时间 —— 每次运行都不一样
#   工具 / 封面标题 —— 记的是 `wrepl v<版本>`，升版本号后必然不同
IGNORE_LABELS = {"报告生成时间", "工具"}
VERSION_RE = re.compile(r"wrepl v\d+\.\d+\.\d+")


def mask(s):
    """把版本号抹平，其余原样保留 —— 免得只因为升了个版本号就报"不一致"。"""
    if "wrepl v" in s:
        return VERSION_RE.sub("wrepl v<ignored>", s)
    return s


def cells(path):
    wb = openpyxl.load_workbook(path, data_only=True)
    out = {}
    for ws in wb.worksheets:
        rows = []
        for row in ws.iter_rows(values_only=True):
            row = ["" if c is None else mask(str(c)) for c in row]
            # 报告是「标签 | 值」两列式：命中忽略项就把值挖掉
            if row and str(row[0]) in IGNORE_LABELS:
                row = [row[0]] + ["<ignored>"] * (len(row) - 1)
            rows.append(tuple(row))
        out[ws.title] = rows
    return out


def main():
    a, b = cells(sys.argv[1]), cells(sys.argv[2])
    bad = 0
    if set(a) != set(b):
        print(f"  工作表不同：{sorted(a)} vs {sorted(b)}")
        return 1
    for name in sorted(a):
        ra, rb = a[name], b[name]
        if len(ra) != len(rb):
            print(f"  [{name}] 行数不同：{len(ra)} vs {len(rb)}")
            bad += 1
        for i, (x, y) in enumerate(zip(ra, rb)):
            if x != y:
                print(f"  [{name}] 第 {i + 1} 行不同：")
                print(f"      A: {x}")
                print(f"      B: {y}")
                bad += 1
                if bad > 20:
                    return 1
    if bad:
        return 1
    print("  报告内容一致（忽略生成时间）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
