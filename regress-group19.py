"""regress 第 19 组：本轮修复的几条语义，钉死在回归里。

为什么单列一组：这几条都是**静默退化型**的 —— 坏了不会报错，只会悄悄改错字节
或报出不存在的残留，靠人工看报告发现不了。

  1. 替换串里的 CR 必须写成字符引用 `&#13;`。
     字面 CR 会被任何规范 XML 解析器的 EOL 规范化变成 LF（写出去 CR、读回来 LF）。
  2. 残留自检的段落分隔符不能是 `'\\n'`（段内 `<w:br/>` 的虚拟字符就是 `'\\n'`），
     否则含换行的查找串会把「段 N 末尾 + 段 N+1 开头」拼成假命中。
  3. `scan` 与 `apply` 的重叠裁决必须同源（`--longest-first` 两边都要能开）。
  4. XML 非法控制字符在装载规则时就点名码位（否则静默丢弃、报告与产物对不上）。
  5. 通配符规则在**动手前**预检（否则一批 N 个文件全被染成 ERROR，报告一片红）。

用法：python regress-group19.py <wrepl.exe> <工作目录>
输出：每项一行 ✓/✗，最后一行 `PASS n FAIL m`（regress.sh 据此累加计数）。
退出码：有任何失败项即 1。
"""
import os
import re
import subprocess
import sys
import zipfile

from docx import Document
import openpyxl

EXE = sys.argv[1]
WORK = sys.argv[2]

PASS = 0
FAIL = 0


def ok(msg):
    global PASS
    PASS += 1
    print(f"  [OK] {msg}")


def bad(msg):
    global FAIL
    FAIL += 1
    print(f"  [FAIL] {msg}")


def expect(cond, msg):
    ok(msg) if cond else bad(msg)


def run(*args):
    r = subprocess.run(
        [EXE, *args], capture_output=True, text=True, encoding="utf-8", errors="replace"
    )
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def counts(log):
    """从 `状态：…　命中 N（替换 M / 冲突 K）` 里取出三元组。"""
    m = re.search(r"命中 (\d+)（替换 (\d+) / 冲突 (\d+)）", log)
    return m.groups() if m else None


def residue_values(report):
    wb = openpyxl.load_workbook(report, read_only=True)
    ws = wb["验证结论"]
    rows = list(ws.iter_rows(values_only=True))
    idx = list(rows[0]).index("残留自检")
    vals = [r[idx] for r in rows[1:] if r and r[0]]
    wb.close()
    return vals


def sheet_contains(path, needle):
    """整本报告里任意单元格含 `needle` 即真（不依赖具体表名/列号）。"""
    wb = openpyxl.load_workbook(path, read_only=True)
    hit = False
    for ws in wb.worksheets:
        for row in ws.iter_rows(values_only=True):
            if any(v is not None and needle in str(v) for v in row):
                hit = True
                break
        if hit:
            break
    wb.close()
    return hit


os.makedirs(WORK, exist_ok=True)

# ── 夹具：一份四段的小 docx（每段各服务一项检查）──────────────────────────
src = os.path.join(WORK, "fixture.docx")
doc = Document()
for t in ("AB-1234", "END", "START", "ABCDEF"):
    doc.add_paragraph(t)
doc.save(src)

# ───────────────────────── 1. CR 写成字符引用 ─────────────────────────
out1 = os.path.join(WORK, "cr-out")
rc, log = run("apply", src, "--rule", "AB-1234=>CD\rEF", "--out", out1)
produced = os.listdir(out1) if os.path.isdir(out1) else []
if rc == 0 and len(produced) == 1:
    xml = zipfile.ZipFile(os.path.join(out1, produced[0])).read("word/document.xml").decode("utf-8")
    expect("&#13;" in xml, "CR 替换串写成了字符引用 &#13;")
    expect("\r" not in xml, "产物里没有字面 CR（不会被 XML EOL 规范化改成 LF）")
    rc2, log2 = run("scan", os.path.join(out1, produced[0]), "--rule", "CD\rEF=>ZZZ")
    expect("命中 1（替换 1" in log2, "工具把 &#13; 解回 CR（find 带 CR 命中 1 处）")
else:
    bad(f"CR 用例未产出文件（rc={rc}）：{log.strip()[:200]}")

# ──────────────── 2. 跨段假命中：含 LF 的查找串不得命中 ────────────────
out2 = os.path.join(WORK, "xpara-out")
rep2 = os.path.join(WORK, "xpara.xlsx")
rc, log = run(
    "apply", src,
    "--rule", "END\nSTART=>X",
    "--out", out2, "--mirror", "--verify-after", "--report", rep2,
)
if rc == 0 and os.path.isfile(rep2):
    vals = residue_values(rep2)
    expect(
        len(vals) >= 1 and all("无残留" in str(v) for v in vals),
        f"含 LF 的查找串不再跨段假命中（残留自检＝无残留）：{vals}",
    )
else:
    bad(f"跨段用例失败（rc={rc}）：{log.strip()[:200]}")

# 阳性对照：真的可见残留必须照旧检出（否则等于把检测能力一起关掉了）
out3 = os.path.join(WORK, "ctl-out")
rep3 = os.path.join(WORK, "ctl.xlsx")
rc, log = run(
    "apply", src,
    "--rule", "START=>START",          # no-op：START 原样留在产物里
    "--out", out3, "--mirror", "--verify-after", "--report", rep3,
)
if os.path.isfile(rep3):
    vals3 = residue_values(rep3)
    expect(
        any("可见文本 1" in str(v) for v in vals3),
        f"阳性对照：真残留照旧检出（可见文本 1 处）：{vals3}",
    )
else:
    bad(f"阳性对照未产出报告（rc={rc}）")

# ───────────── 3. scan 与 apply 的重叠裁决同源（--longest-first）─────────────
overlap = ["--rule", "ABC=>X", "--rule", "BCDE=>Y"]   # 在段落 ABCDEF 上重叠
_, s_def = run("scan", src, *overlap)
_, s_lf = run("scan", src, "--longest-first", *overlap)
_, a_def = run("apply", src, "--dry-run", *overlap)
_, a_lf = run("apply", src, "--dry-run", "--longest-first", *overlap)
if all(counts(x) for x in (s_def, s_lf, a_def, a_lf)):
    expect(counts(s_def) == counts(a_def), f"默认裁决 scan == apply：{counts(s_def)}")
    expect(counts(s_lf) == counts(a_lf), f"最长优先 scan == apply：{counts(s_lf)}")
    expect(counts(s_def) != counts(s_lf), f"两种裁决结果确实不同（{counts(s_def)} vs {counts(s_lf)}）")
else:
    bad("裁决用例未取到命中三元组")

rc, log = run("scan", src, "--longest-first", "--chain", *overlap)
expect(rc != 0 and "不能同时使用" in log, "scan：--longest-first 与 --chain 同时给被明确拒绝")

# ───────────── 4. XML 非法控制字符在装载时点名 ─────────────
rc, log = run("scan", src, "--rule", "AB=>C\u000bD")
expect("U+000B" in log and "替换为" in log, "替换为含 U+000B → 装载时告警并点名码位")
rc, log = run("scan", src, "--rule", "AB=>CD")
expect("⚠" not in log, "阴性对照：合规规则零告警")

# ───────────── 5. 通配符规则：动手前预检，整批一个文件都不碰 ─────────────
# 旧行为：检查在「每个文件每个 part」里，一条坏规则把整批染成 ERROR；
# 新行为：批处理入口一次挡下，输出目录连建都不建，报错只有一条。
wdir = os.path.join(WORK, "wc-in")
os.makedirs(wdir, exist_ok=True)
for i in range(3):
    d = Document()
    d.add_paragraph("AB")
    d.save(os.path.join(wdir, f"f{i}.docx"))
rf = os.path.join(WORK, "wild.txt")
with open(rf, "w", encoding="utf-8", newline="") as fh:
    # 文本规则文件**没有表头行**（格式见 `wrepl rules template`）：
    # 每行 `查找内容<TAB>替换为<TAB>作用域<TAB>选项<TAB>备注`
    fh.write("AB\tCD\t全部\t通配符\t\n")
out5 = os.path.join(WORK, "wc-out")
rc, log = run("apply", wdir, "--rules-file", rf, "--out", out5)
expect(rc != 0, f"含通配符的规则表被驳回（rc={rc}）")
expect("通配符" in log, "驳回消息说清是通配符未实现")
produced5 = os.listdir(out5) if os.path.isdir(out5) else []
expect(len(produced5) == 0, f"动手前中止：输出目录 0 个文件（实际 {len(produced5)}）")
expect("ERROR" not in log, "不再是「每个文件各报一次 ERROR」")

# ───────── 6. 命名空间重声明随元素作用域回滚（不污染后续段落）─────────
# 旧行为：命名空间表只增不减，子树把 `w` 指到别的 URI 后，后面真正的
#         `<w:p>` 会被当作非 W 命名空间 → 凭空漏掉段落（静默少改）。
W_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
ns_base = os.path.join(WORK, "ns-base.docx")
_d = Document()
_d.add_paragraph("placeholder")
_d.save(ns_base)
ns_doc = os.path.join(WORK, "ns.docx")
custom = (
    f'<w:document xmlns:w="{W_NS}"><w:body>'
    "<w:p><w:r><w:t>AB</w:t></w:r></w:p>"
    '<x xmlns:w="urn:other"><w:p><w:r><w:t>HIDDEN</w:t></w:r></w:p></x>'
    "<w:p><w:r><w:t>AB</w:t></w:r></w:p>"
    "</w:body></w:document>"
)
with zipfile.ZipFile(ns_base) as zi, zipfile.ZipFile(
    ns_doc, "w", zipfile.ZIP_DEFLATED
) as zo:
    for it in zi.infolist():
        data = zi.read(it.filename)
        if it.filename == "word/document.xml":
            data = custom.encode("utf-8")
        zo.writestr(it, data)
rc, log = run("scan", ns_doc, "--rule", "AB=>ZZ")
c6 = counts(log)
expect(c6 is not None and c6[0] == "2", f"子树重声明 `w` 后仍命中两个真段落（实际 {c6}）")

# ───────── 7. 覆盖输出目录同名旧文件时有明示 ─────────
cov_out = os.path.join(WORK, "cover-out")
os.makedirs(cov_out, exist_ok=True)
_d = Document()
_d.add_paragraph("stale")
_d.save(os.path.join(cov_out, "fixture.docx"))   # 假装是上一轮留下的同名旧产物
rep7 = os.path.join(WORK, "cover.xlsx")
rc, log = run(
    "apply", src, "--rule", "AB-1234=>Q",
    "--out", cov_out, "--verify-after", "--report", rep7,
)
if rc == 0 and os.path.isfile(rep7):
    expect(
        sheet_contains(rep7, "已被本次产物覆盖"),
        "覆盖了输出目录里的同名旧文件 → 报告里明示（不静默）",
    )
else:
    bad(f"覆盖用例失败（rc={rc}）：{log.strip()[:200]}")

print(f"\nPASS {PASS} FAIL {FAIL}")
sys.exit(1 if FAIL else 0)
