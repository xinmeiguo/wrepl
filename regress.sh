#!/usr/bin/env bash
# wrepl 全量回归 —— 一条命令跑完全部硬指标。
#
# 用法：
#   bash regress.sh [wrepl.exe 路径] [wrepl-gui.exe 路径]
#
# 设计原则：每一项都对照**外部可复算**的量（SHA256 / 命中数 / 退出码），
# 不依赖工具自己的说法。任一项失败即非零退出。
#
# 背景：本机 C: 盘禁止第三方写入/执行（sandbox 之外也一样），
# 所有构建产物与工作目录都在 D: 盘，见文末「本机环境」一节。

set -u

WREPL="${1:-D:/test/cargo-target/debug/wrepl.exe}"
GUIPATH="${2:-D:/test/cargo-target/debug/wrepl-gui.exe}"
SRC="${WREPL_SRC:-D:/test/wrepl}"
PY="${WREPL_PY:-python3}"

# ── 语料与样本：真实值不进版本库 ────────────────────────────────────────
#
# 本回归跑的是**真实交付包**，里面的项目编号与客户名属于商业信息，不适合
# 随源码一起公开；可脚本又必须靠这些字面量去 cp 文件、grep 期望值。折中办法：
# 全部走变量，下面的默认值是**脱敏占位**；本机真实值放在同目录的
# `regress-samples.local.sh`（已在 .gitignore 里），存在就把上面覆盖掉。
#
# 换自己的语料跑：把 WREPL_CORPUS 指到你的 docx 目录，再把
# P_OLD / P_NEW / C_OLD / C_NEW 换成语料里**真实存在**的串即可。
CORPUS="${WREPL_CORPUS:-./corpus}"                # 待替换的 docx 目录
CORPUS_BASELINE="${WREPL_CORPUS_BASELINE:-}"      # 历史基线产物目录（可留空＝跳过该项）
RUN_DIR="${WREPL_RUN_DIR:-./corpus/run}"          # 端到端用的规则文件所在目录
SAMPLES="${WREPL_SAMPLES:-./samples}"             # 单文件样本目录
MATRIX="${WREPL_MATRIX:-./run_option_matrix.py}"  # 选项矩阵脚本
SAMPLE_REAL="${SAMPLE_REAL:-REAL-2025-005AUTc-OQ-模板-2026-06-22}"

# 语料里的字面量（默认＝脱敏占位）
P_OLD="${P_OLD:-2026-001CE}"                      # 语料里的旧项目编号
P_NEW="${P_NEW:-2026-002CE}"                      # 归一后的新编号
P_ALT="${P_ALT:-2022-007CE}"                      # 只在个别文件里出现的更老编号
C_OLD="${C_OLD:-某某制药有限公司}"
C_NEW="${C_NEW:-某某生物}"
C_OLD_EN="${C_OLD_EN:-Xxx Pharmaceutical Co., Ltd.}"
C_NEW_EN="${C_NEW_EN:-Xxx Biology}"

# 本机真实值（该文件不进版本库）。没有它就跑脱敏占位 —— 语法没毛病，
# 但 cp 找不到文件、grep 对不上期望值，会整片报红。这是设计如此。
LOCAL_OVERRIDE="$(cd "$(dirname "$0")" && pwd)/regress-samples.local.sh"
[ -f "$LOCAL_OVERRIDE" ] && . "$LOCAL_OVERRIDE"

# 由上面推导出来的（放在覆盖之后，好让本机值参与推导）
F1="${F1:-$P_OLD-OQ-附录-V01-2026.08.28}"
F2="${F2:-标14-文件包装箱封面-模板}"
F3="${F3:-标2-$P_OLD-MC-材质证明清单(双语）-2024-08-21}"
RULES_FILE="${RULES_FILE:-$RUN_DIR/rules-$P_NEW.txt}"
# ⚠ 不要用 `rm -rf $WORK` 清工作目录：本机删除操作会被静默拦截（fail-closed），
# 残留的上一轮产物会让「改名后目标已存在 → 自动加序号」生效，凭空多出一倍文件，
# 表现为"26 个产物变成 47 个"这种看不懂的失败。改成**每次唯一目录名**，从根上避免。
WORK="${WREPL_WORK:-D:/test/regress-run-$$}"
if [ -e "$WORK" ]; then
  printf '\033[31m工作目录已存在，为免污染请换个 PID 或手动删除：%s\033[0m\n' "$WORK"
  exit 1
fi
mkdir -p "$WORK"

PASS=0
FAIL=0
ok()   { PASS=$((PASS+1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  \033[31m✗\033[0m %s\n' "$1"; }
head1() { printf '\n\033[36m══════ %s ══════\033[0m\n' "$1"; }

sha() { sha256sum "$1" 2>/dev/null | cut -d' ' -f1; }

# ───────────────────────── 1. 单元测试 ─────────────────────────
head1 "1. 单元测试（cargo test --offline --lib）"
cd "$SRC" || exit 1
if cargo test --offline --lib -j 8 > "$WORK/test.log" 2>&1; then
  n=$(grep -oE '[0-9]+ passed' "$WORK/test.log" | head -1)
  ok "单元测试全部通过（$n）"
else
  bad "单元测试失败 —— 见 $WORK/test.log"
fi

# ───────────────────── 2. 零改动透传（SHA 必须相同）─────────────────────
head1 "2. 零改动透传：读入再写出，SHA256 必须一字不差"
PT=$WORK/pt; mkdir -p "$PT"
for n in "$SAMPLE_REAL" "S-全坑样本" "T-选项矩阵"; do
  "$WREPL" passthrough "$SAMPLES/$n.docx" "$PT/$n.docx" >/dev/null 2>&1
  if [ "$(sha "$SAMPLES/$n.docx")" = "$(sha "$PT/$n.docx")" ]; then
    ok "$n  SHA 完全相同"
  else
    bad "$n  SHA 不同"
  fi
done
# python-docx 生成的样本：ZIP 容器层（local header extra field）会变，
# 但 part 内容必须全部一致 —— 用 diff 子命令证明。
for n in "A-URS" "B-点检表" "C-电气图档"; do
  "$WREPL" passthrough "$SAMPLES/$n.docx" "$PT/$n.docx" >/dev/null 2>&1
  if "$WREPL" diff "$SAMPLES/$n.docx" "$PT/$n.docx" 2>&1 | grep -q "全部 part 内容一致"; then
    ok "$n  part 内容全一致（仅容器层差异，已知）"
  else
    bad "$n  part 内容不一致"
  fi
done

# ───────────────────── 3. 选项矩阵 11 项 ─────────────────────
head1 "3. 选项矩阵（外部脚本逐项对照期望命中数）"
if [ -f "$MATRIX" ]; then
  if "$PY" "$MATRIX" "$WREPL" > "$WORK/matrix.log" 2>&1 && grep -q "11/11 通过" "$WORK/matrix.log"; then
    ok "选项矩阵 11/11 通过"
  else
    bad "选项矩阵未全通过 —— 见 $WORK/matrix.log"
  fi
else
  bad "找不到 $MATRIX"
fi

# ───────────────────── 4. 字符引用 &#nnn; ─────────────────────
head1 "4. 字符引用 &#nnn;（quick-xml GeneralRef 坑）"
CH=$WORK/ch; mkdir -p "$CH"
CHSRC="D:/test/regress/CH-charref.docx"
"$WREPL" apply "$CHSRC" --rule "设计情况=>已改造情况" --out "$CH" >/dev/null 2>&1
if [ -f "$CH/CH-charref.docx" ]; then
  before=$("$PY" -c "import zipfile,re,sys;print(len(re.findall(r'&#\d+;',zipfile.ZipFile(sys.argv[1]).read('word/document.xml').decode('utf-8','replace'))))" "$CHSRC")
  after=$("$PY" -c "import zipfile,re,sys;print(len(re.findall(r'&#\d+;',zipfile.ZipFile(sys.argv[1]).read('word/document.xml').decode('utf-8','replace'))))" "$CH/CH-charref.docx")
  hit=$("$PY" -c "import zipfile,sys;print(zipfile.ZipFile(sys.argv[1]).read('word/document.xml').decode('utf-8','replace').count('已改造情况'))" "$CH/CH-charref.docx")
  if [ "$before" != "0" ] && [ "$after" = "0" ] && [ "$hit" = "1" ]; then
    ok "字符引用 ${before} 处 → 0，替换落地 1 处"
  else
    bad "字符引用处理异常（前 $before / 后 $after / 替换 $hit）"
  fi
else
  bad "字符引用测试未产出文件"
fi

# ───────────────────── 5. 区间重叠必须报冲突 ─────────────────────
head1 "5. 区间重叠：必须报冲突，不许猜"
CF=$WORK/cf; mkdir -p "$CF"
"$WREPL" apply "$SAMPLES/T-选项矩阵.docx" --rule "abc=>X" --rule "bc=>Y" --out "$CF" > "$WORK/cf.log" 2>&1
if grep -q "状态：CONFLICT 1" "$WORK/cf.log" && grep -q "已替换 0" "$WORK/cf.log"; then
  ok "重叠区间全部判为冲突，0 处误替换"
else
  bad "冲突判定异常 —— 见 $WORK/cf.log"
fi

# ───────────────────── 6. 默认独立 vs --chain ─────────────────────
head1 "6. 规则默认独立（基于原文）vs --chain（前一条输出喂后一条）"
a=$("$WREPL" apply "$SAMPLES/T-选项矩阵.docx" --rule "abc=>XY" --rule "XY=>Z" --out "$WORK/x" --dry-run 2>&1 | grep -oE "命中 [0-9]+" | head -1)
b=$("$WREPL" apply "$SAMPLES/T-选项矩阵.docx" --rule "abc=>XY" --rule "XY=>Z" --chain --out "$WORK/x" --dry-run 2>&1 | grep -oE "命中 [0-9]+" | head -1)
if [ "$a" = "命中 5" ] && [ "$b" = "命中 10" ]; then
  ok "默认独立 $a ／ 链式 $b（链式多出的正是前一条产出）"
else
  bad "独立/链式语义异常（默认 $a / 链式 $b）"
fi

# ───────────────────── 7. 真实交付包端到端（26 文件）─────────────────────
head1 "7. 真实交付包端到端：$CORPUS → 归一 $P_NEW"
RUN="$RUN_DIR"
OUT=$WORK/cli; mkdir -p "$OUT"
if [ -f "$RULES_FILE" ]; then
  "$WREPL" apply "$CORPUS" --rules-file "$RULES_FILE" --out "$OUT" --rename-files > "$WORK/e2e.log" 2>&1
  got=$(grep -oE "文件 [0-9]+ ｜ 命中 [0-9]+ ｜ 已替换 [0-9]+ ｜ 冲突 [0-9]+" "$WORK/e2e.log" | head -1)
  if [ "$got" = "文件 26 ｜ 命中 122 ｜ 已替换 122 ｜ 冲突 0" ]; then
    ok "命中/替换/冲突 $got"
  else
    bad "端到端计数异常：$got"
  fi
  # 改名后的批量验证：两级配对（同名 + 结构指纹）
  "$WREPL" verify "$CORPUS" "$OUT" --batch > "$WORK/vb.log" 2>&1
  if grep -q "关卡 1+2：通过 26 / 不通过 0" "$WORK/vb.log" && grep -q "结构指纹（已改名）21" "$WORK/vb.log"; then
    ok "改名后批量验证 26/26 通过（同名 5 + 结构指纹 21，0 未配对）"
  else
    bad "改名后批量验证未全通过 —— 见 $WORK/vb.log"
  fi
  # 跨版本一致性：与历史基线产物逐字节相同
  if [ -d "$CORPUS_BASELINE" ]; then
    same=0; diffn=0
    for f in "$OUT"/*; do
      bn=$(basename "$f")
      if [ -f "$CORPUS_BASELINE/$bn" ]; then
        [ "$(sha "$f")" = "$(sha "$CORPUS_BASELINE/$bn")" ] && same=$((same+1)) || diffn=$((diffn+1))
      fi
    done
    if [ "$same" = "26" ] && [ "$diffn" = "0" ]; then
      ok "与历史基线产物 26/26 SHA256 完全相同（内核零漂移）"
    else
      bad "与基线不一致：同 $same / 异 $diffn"
    fi
  fi
else
  bad "找不到 $RUN/rules-009CE.txt"
fi

# ───────────────────── 8. 侦察（probe）能力 ─────────────────────
head1 "8. 目录侦察：自动列出候选项目编号与客户名"
"$WREPL" probe "$CORPUS" --out "$WORK/候选.txt" > "$WORK/probe.log" 2>&1
if grep -q "$P_OLD" "$WORK/probe.log" \
   && grep -q "$C_OLD" "$WORK/probe.log" \
   && grep -q "$C_OLD_EN" "$WORK/probe.log"; then
  ok "编号 + 中英文客户名都识别到，并写出建议规则文件"
else
  bad "侦察输出不完整 —— 见 $WORK/probe.log"
fi

# ───────────────────── 9. GUI 无头自检 ─────────────────────
head1 "9. GUI 无头自检（加载字体 → 跑 3 帧布局 → 跑完整批）"
if [ -x "$GUIPATH" ] && [ -f "$RULES_FILE" ]; then
  g=$WORK/gui; mkdir -p "$g"
  "$GUIPATH" --selftest "$CORPUS" "$g" "$RULES_FILE" > "$WORK/gui.log" 2>&1
  if grep -q "自检通过" "$WORK/gui.log" && grep -q "验证 26/26" "$WORK/gui.log"; then
    ok "GUI 自检通过，且验证 26/26"
  else
    bad "GUI 自检失败 —— 见 $WORK/gui.log"
  fi
  # 规则的 Excel 往返（界面上「从 Excel 导入 / 导出为 Excel」两个按钮的同一条路）
  if grep -q "规则 Excel 往返通过" "$WORK/gui.log"; then
    ok "规则的 Excel 导出→读回一致"
  else
    bad "规则 Excel 往返失败 —— 见 $WORK/gui.log"
  fi
  # GUI 与 CLI 必须产出逐字节相同的文件（共享内核，零漂移）
  same=0; diffn=0
  for f in "$OUT"/*; do
    bn=$(basename "$f")
    if [ -f "$g/$bn" ]; then
      [ "$(sha "$f")" = "$(sha "$g/$bn")" ] && same=$((same+1)) || diffn=$((diffn+1))
    fi
  done
  if [ "$same" = "26" ] && [ "$diffn" = "0" ]; then
    ok "GUI 产物与 CLI 产物 26/26 SHA256 完全相同"
  else
    bad "GUI 与 CLI 产物不一致：同 $same / 异 $diffn"
  fi
  # 运行日志与报告都落在产物目录里（界面上一句话不说，东西必须在）
  if [ -s "$g/运行日志.txt" ] && grep -q "合计：文件 26" "$g/运行日志.txt"; then
    ok "运行日志落在输出目录，且含本次执行的汇总行"
  else
    bad "运行日志没落地或内容不对 —— 见 $g/运行日志.txt"
  fi
  if [ -s "$g/替换报告.xlsx" ]; then
    ok "报告落在输出目录"
  else
    bad "报告没落地：$g/替换报告.xlsx"
  fi
else
  bad "找不到 $GUIPATH 或规则文件"
fi

# ───────────── 10. 默认就地替换源文件 + 未命中的不输出 ─────────────
head1 "10. 默认就地替换源文件（不给 --out）＋ 备份可选 ＋ 未命中的不落盘"
IP=$WORK/inplace; mkdir -p "$IP/src" "$IP/src2" "$IP/orig"
# 原件另存一份（orig/）：下面的复原与比对都拿它当基准，
# **不依赖 .bak** —— .bak 现在是可选项，拿它当唯一凭据会把测试测成循环论证。
for n in "$F1" "$F2" "$F3"; do
  cp "$CORPUS/$n.docx" "$IP/src/$n.docx"
  cp "$CORPUS/$n.docx" "$IP/orig/$n.docx"
done
: > "$IP/before.txt"
for f in "$IP/orig"/*.docx; do printf '%s\t%s\n' "$(sha "$f")" "$(basename "$f")" >> "$IP/before.txt"; done
R1="$P_OLD=>$P_NEW"
R2="$C_OLD=>$C_NEW"
R3="$C_OLD_EN=>$C_NEW_EN"
# 标14（包装箱封面）正文里只有 $P_ALT，前三条都打不到它
R4="$P_ALT=>$P_NEW"

"$WREPL" apply "$IP/src" --rule "$R1" --rule "$R2" --rule "$R3" --rule "$R4" --verify-after > "$IP/run1.log" 2>&1
changed=0; total=0
while IFS=$'\t' read -r h n; do
  total=$((total+1))
  [ "$(sha "$IP/src/$n")" != "$h" ] && changed=$((changed+1))
done < "$IP/before.txt"
nbak=$(ls "$IP/src"/*.bak 2>/dev/null | wc -l)
if [ "$changed" = "$total" ] && [ "$nbak" = "0" ]; then
  ok "就地替换生效，且默认不留备份（正本改动 $changed/$total，源目录 .bak $nbak 个）"
else
  bad "就地替换异常：正本改动 $changed/$total，源目录 .bak $nbak 个 —— 见 $IP/run1.log"
fi

# 10b）要备份就必须显式 --backup；备份内容＝**最初那版**
for n in "$F1" "$F2" "$F3"; do cp "$IP/orig/$n.docx" "$IP/src/$n.docx"; done
"$WREPL" apply "$IP/src" --rule "$R1" --rule "$R2" --rule "$R3" --rule "$R4" --backup > "$IP/run1b.log" 2>&1
bakok=0; nbak=0
while IFS=$'\t' read -r h n; do
  [ -f "$IP/src/$n.bak" ] && nbak=$((nbak+1))
  [ "$(sha "$IP/src/$n.bak")" = "$h" ] && bakok=$((bakok+1))
done < "$IP/before.txt"
if [ "$bakok" = "$total" ] && [ "$nbak" = "$total" ]; then
  ok "--backup 时留一份 .bak，内容＝最初那版（$bakok/$total）"
else
  bad "--backup 备份不正确：有 $nbak 份、内容对 $bakok/$total —— 见 $IP/run1b.log"
fi

"$WREPL" apply "$IP/src" --rule "$R1" --rule "$R2" --rule "$R3" --rule "$R4" > "$IP/run2.log" 2>&1
r2=$(grep -oE "文件 [0-9]+ ｜ 命中 [0-9]+" "$IP/run2.log" | head -1)
if [ "$r2" = "文件 3 ｜ 命中 0" ]; then
  ok "重复执行幂等：$r2"
else
  bad "幂等性异常：$r2 —— 见 $IP/run2.log"
fi

"$WREPL" apply "$IP/src" --rule "$R1" --out "$IP/out0" > /dev/null 2>&1
n0=$(ls "$IP/out0" 2>/dev/null | wc -l)
if [ "$n0" = "0" ]; then
  ok "未命中的文件不落盘（输出目录 0 个，符合设计）"
else
  bad "未命中的文件被写进了输出目录（$n0 个）"
fi

# 跨模式一致性：就地替换的正本 == 同一批源走 --out 的产物
while IFS=$'\t' read -r h n; do cp "$IP/orig/$n" "$IP/src2/$n"; done < "$IP/before.txt"
"$WREPL" apply "$IP/src2" --rule "$R1" --rule "$R2" --rule "$R3" --rule "$R4" --out "$IP/out3" > /dev/null 2>&1
same=0; diffn=0
while IFS=$'\t' read -r h n; do
  if [ -f "$IP/out3/$n" ]; then
    [ "$(sha "$IP/src/$n")" = "$(sha "$IP/out3/$n")" ] && same=$((same+1)) || diffn=$((diffn+1))
  else
    diffn=$((diffn+1))
  fi
done < "$IP/before.txt"
if [ "$same" = "$total" ] && [ "$diffn" = "0" ]; then
  ok "就地替换与写副本的产出逐字节相同（$same/$total SHA256）"
else
  bad "两种落盘方式产物不一致：同 $same / 异 $diffn"
fi

# ───────────── 11. Excel 规则表：列格式、认列、按工作表顺序找表 ─────────────
head1 "11. Excel 规则表（表名随意 / 第1列查找内容 / 第2列替换为 / 无序号列）"
XL=$WORK/xl; mkdir -p "$XL"
"$WREPL" rules template --out "$XL/new.xlsx" > "$XL/tmpl.log" 2>&1
"$WREPL" rules template --out "$XL/same.xlsx" --rule "$R1" --rule "$R2" --rule "$R3" > "$XL/t3.log" 2>&1

"$PY" - "$XL" "$P_OLD" "$P_NEW" "$C_OLD" "$C_NEW" "$C_OLD_EN" "$C_NEW_EN" <<'PYEOF'
import sys, os, openpyxl
d, P_OLD, P_NEW, C_OLD, C_NEW, C_OLD_EN, C_NEW_EN = sys.argv[1:8]
def mk(name, rows):
    wb = openpyxl.Workbook(); ws = wb.active; ws.title = "规则"
    for r in rows:
        ws.append(r)
    wb.save(os.path.join(d, name))
# 旧格式：第 1 列是「序号」
mk("legacy.xlsx", [
    ["序号", "查找内容", "替换为", "区分大小写", "全字匹配", "使用通配符", "区分全半角", "作用域", "启用", "备注"],
    ["1", P_OLD, P_NEW, "N", "N", "N", "N", "全部", "是", ""],
    ["2", C_OLD, C_NEW, "N", "N", "N", "N", "全部", "是", ""],
    ["3", C_OLD_EN, C_NEW_EN, "N", "N", "N", "N", "全部", "是", ""],
])
# 无表头，只有两列
mk("bare.xlsx", [
    [P_OLD, P_NEW],
    [C_OLD, C_NEW],
    [C_OLD_EN, C_NEW_EN],
])
# 列名乱序
mk("shuffled.xlsx", [
    ["启用", "作用域", "替换为", "查找内容"],
    ["是", "全部", P_NEW, P_OLD],
])
# 认不出的表头 —— 必须报错
mk("bad.xlsx", [["原内容", "新内容"], [P_OLD, P_NEW]])

RULES3 = [
    [P_OLD, P_NEW],
    [C_OLD, C_NEW],
    [C_OLD_EN, C_NEW_EN],
]
HDR2 = ["查找内容", "替换为"]

# 表名随意：不该要求必须叫「规则」
wb = openpyxl.Workbook(); ws = wb.active; ws.title = "替换清单"
ws.append(HDR2)
for r in RULES3:
    ws.append(r)
wb.save(os.path.join(d, "anyname.xlsx"))

# 多工作表：第一张是「填写说明」（无表头、第一列却也有字），第二张才是规则
# → 必须跳到第二张，不能被说明页抢走
wb = openpyxl.Workbook(); ws = wb.active; ws.title = "填写说明"
ws.append(["本工作簿怎么用"])
ws.append(["在第一列填要查找的文字，第二列填替换成什么"])
ws2 = wb.create_sheet("Sheet2")
ws2.append(HDR2)
for r in RULES3:
    ws2.append(r)
wb.save(os.path.join(d, "multisheet.xlsx"))

# 多工作表，第二张是**无表头**的裸两列表 → 退到第二轮也要能找到
wb = openpyxl.Workbook(); ws = wb.active; ws.title = "封面"
ws.append(["某某项目替换规则表"])
ws.append(["制表：张三"])
ws2 = wb.create_sheet("规则")
for r in RULES3:
    ws2.append(r)
wb.save(os.path.join(d, "multinaked.xlsx"))

# 整本都读不出条款 → 必须明确报错，不许静默当"零规则"跑完
wb = openpyxl.Workbook(); ws = wb.active; ws.title = "说明"
ws.append(["说明"]); ws.append(["这里没有规则"])
ws2 = wb.create_sheet("备注"); ws2.append(["也没有"])
wb.save(os.path.join(d, "noclause.xlsx"))
PYEOF

HDR=$("$PY" -c "
import sys, openpyxl
wb = openpyxl.load_workbook(sys.argv[1]); ws = wb['规则']
print('|'.join(str(c.value or '') for c in ws[1]))
" "$XL/new.xlsx")
if [ "$HDR" = "查找内容|替换为|区分大小写|全字匹配|使用通配符|区分全半角|作用域|启用|备注" ]; then
  ok "模板表头：第1列查找内容、第2列替换为，无序号列"
else
  bad "模板表头不符：$HDR"
fi

# 内容相同的规则，换一种表格式不该换语义 → 命中总数必须一致
sum_hits() {
  "$WREPL" apply "$CORPUS" --rules-book "$1" --dry-run 2>&1 \
    | grep "状态：" | grep -oE "命中 [0-9]+" | awk '{s+=$2} END{print s+0}'
}
h_new=$(sum_hits "$XL/same.xlsx")
h_leg=$(sum_hits "$XL/legacy.xlsx")
h_bar=$(sum_hits "$XL/bare.xlsx")
if [ "$h_new" != "0" ] && [ "$h_new" = "$h_leg" ] && [ "$h_new" = "$h_bar" ]; then
  ok "新格式 / 旧格式(带序号) / 裸两列 解析结果一致（命中 $h_new）"
else
  bad "表格格式影响了解析：新 $h_new / 旧带序号 $h_leg / 裸两列 $h_bar"
fi

h_shf=$(sum_hits "$XL/shuffled.xlsx")
if [ "$h_shf" != "0" ]; then
  ok "列名乱序仍能识别（命中 $h_shf）"
else
  bad "列名乱序后读不出规则（命中 $h_shf）"
fi

# 脚本本身没有 errexit，失败命令不会中断，直接取退出码
BAD_OUT=$("$WREPL" apply "$CORPUS" --rules-book "$XL/bad.xlsx" --dry-run 2>&1); BAD_RC=$?
if [ "$BAD_RC" != "0" ] && printf '%s' "$BAD_OUT" | grep -q "查找内容"; then
  ok "认不出的表头 → 明确报错（退出码 $BAD_RC），不当成规则读进去"
else
  bad "坏表头没被拦住（退出码 $BAD_RC）：$BAD_OUT"
fi

# 表名不必叫「规则」；多工作表按顺序找
h_any=$(sum_hits "$XL/anyname.xlsx")
h_multi=$(sum_hits "$XL/multisheet.xlsx")
h_naked=$(sum_hits "$XL/multinaked.xlsx")
if [ "$h_any" = "$h_new" ] && [ "$h_multi" = "$h_new" ] && [ "$h_naked" = "$h_new" ]; then
  ok "表名随意 / 说明页在前 / 裸两列在后 —— 都找得到规则表（命中 $h_new）"
else
  bad "按工作表顺序找规则表失败：随意表名 $h_any / 说明页在前 $h_multi / 裸两列在后 $h_naked / 基准 $h_new"
fi

NC_OUT=$("$WREPL" apply "$CORPUS" --rules-book "$XL/noclause.xlsx" --dry-run 2>&1); NC_RC=$?
if [ "$NC_RC" != "0" ] && printf '%s' "$NC_OUT" | grep -q "没读出任何规则"; then
  ok "整本工作表都读不出条款 → 明确报错（退出码 $NC_RC），不当成零规则跑完"
else
  bad "空规则表没被拦住（退出码 $NC_RC）：$NC_OUT"
fi

# ───────── 12. 界面精简后：越界的规则必须挡在表外 ─────────
head1 "12. 规则文件里的「禁用 / 通配符」行必须挡在界面之外"
if [ -x "$GUIPATH" ] && [ -f "$RULES_FILE" ]; then
  g12=$WORK/gui12; mkdir -p "$g12"
  R12=$WORK/rules-mixed.txt
  cat "$RULES_FILE" > "$R12"
  printf 'PENICILLIN\t青霉素\t全部\toff\n' >> "$R12"
  printf 'DP-*-01\tDP-AUC-01\t全部\twildcard\n' >> "$R12"
  "$GUIPATH" --selftest "$CORPUS" "$g12" "$R12" > "$WORK/gui12.log" 2>&1
  if grep -q "跳过 1 条标着「禁用」" "$WORK/gui12.log" \
     && grep -q "跳过 1 条要求「使用通配符」" "$WORK/gui12.log"; then
    ok "两条越界规则被挡在表外，并在日志里说清原因"
  else
    bad "禁用/通配符行没被正确挡住 —— 见 $WORK/gui12.log"
  fi
  # 挡掉的那两条本来就不参与替换 → 命中数必须与只含启用规则时**一模一样**
  h12=$(grep -oE "命中 [0-9]+" "$WORK/gui12.log" | tail -1 | grep -oE "[0-9]+")
  h9=$(grep -oE "命中 [0-9]+" "$WORK/gui.log" | tail -1 | grep -oE "[0-9]+")
  if [ -n "$h12" ] && [ "$h12" = "$h9" ]; then
    ok "命中数与只含启用规则时一致（$h12）"
  else
    bad "命中数被越界规则影响了：混合 $h12 / 纯启用 $h9"
  fi
else
  bad "找不到 GUI 或规则文件"
fi

# ───────── 13. 文件级并行：线程数只影响快慢，不影响一个字节 ─────────
head1 "13. 并行：线程数只该影响快慢，不该影响产物"
if [ -f "$RULES_FILE" ]; then
  p1=$WORK/par-t1; p8=$WORK/par-t8; pz=$WORK/par-auto
  mkdir -p "$p1" "$p8" "$pz"
  "$WREPL" apply "$CORPUS" --rules-file "$RULES_FILE" --out "$p1" --rename-files --threads 1 > "$WORK/par-t1.log" 2>&1
  "$WREPL" apply "$CORPUS" --rules-file "$RULES_FILE" --out "$p8" --rename-files --threads 8 > "$WORK/par-t8.log" 2>&1
  "$WREPL" apply "$CORPUS" --rules-file "$RULES_FILE" --out "$pz" --rename-files > "$WORK/par-auto.log" 2>&1

  # 判据一：产物逐字节相同。这条同时覆盖了"输出路径预分配必须串行"——
  # 26 个文件里有 21 个要改名，重名消解的顺序若随线程调度变化，文件名就会漂。
  same=0; diffn=0; miss=0
  for f in "$p1"/*; do
    bn=$(basename "$f")
    if [ -f "$p8/$bn" ] && [ -f "$pz/$bn" ]; then
      if [ "$(sha "$f")" = "$(sha "$p8/$bn")" ] && [ "$(sha "$f")" = "$(sha "$pz/$bn")" ]; then
        same=$((same+1))
      else
        diffn=$((diffn+1))
      fi
    else
      miss=$((miss+1))
    fi
  done
  if [ "$same" = "26" ] && [ "$diffn" = "0" ] && [ "$miss" = "0" ]; then
    ok "26/26 文件在 1 / 8 / 自动 三种线程数下逐字节相同（含 21 个改名）"
  else
    bad "线程数影响了产物：同 $same / 异 $diffn / 缺 $miss"
  fi

  # 判据二：汇总计数一致（报告行序不随完成顺序抖动）
  c1=$(grep -oE "文件 [0-9]+ ｜ 命中 [0-9]+ ｜ 已替换 [0-9]+ ｜ 冲突 [0-9]+" "$WORK/par-t1.log" | head -1)
  cz=$(grep -oE "文件 [0-9]+ ｜ 命中 [0-9]+ ｜ 已替换 [0-9]+ ｜ 冲突 [0-9]+" "$WORK/par-auto.log" | head -1)
  if [ -n "$c1" ] && [ "$c1" = "$cz" ]; then
    ok "汇总计数一致：$c1"
  else
    bad "汇总计数不一致：1线程[$c1] 自动[$cz]"
  fi

  # 判据三：确认它真的开了多线程，而不是"结果对但根本没并行"
  t1=$(grep -oE "并行：[0-9]+ 线程" "$WORK/par-t1.log" | head -1)
  tz=$(grep -oE "并行：[0-9]+ 线程" "$WORK/par-auto.log" | head -1)
  nz=$(printf '%s' "$tz" | grep -oE "[0-9]+")
  if [ "$t1" = "并行：1 线程" ] && [ -n "$nz" ] && [ "$nz" -ge 2 ]; then
    ok "--threads 1 →「$t1」；默认 →「$tz」，确实启用了多线程"
  else
    bad "线程数没报对：[$t1] / [$tz]"
  fi
else
  bad "找不到 $RUN/rules-009CE.txt"
fi

# ───────── 14. 最长匹配优先：重叠时让更长的规则落笔 ─────────
head1 "14. 最长匹配优先（--longest-first）"
lf=$WORK/lf; mkdir -p "$lf/a" "$lf/b" "$lf/c" "$lf/d"

# ① 默认：重叠 = 两条都不改（原有"不猜"策略，必须原样保留）
"$WREPL" apply "$SAMPLES/T-选项矩阵.docx" --rule "abc=>X" --rule "bc=>Y" \
  --out "$lf/a" > "$WORK/lf-a.log" 2>&1
# ② 最长优先：abc（3 字）赢 bc（2 字）——两条各命中 5 处、全部重叠，真改动 0 → 5
"$WREPL" apply "$SAMPLES/T-选项矩阵.docx" --rule "abc=>X" --rule "bc=>Y" \
  --out "$lf/b" --longest-first > "$WORK/lf-b.log" 2>&1
ca=$(grep -oE "文件 [0-9]+ ｜ 命中 [0-9]+ ｜ 已替换 [0-9]+ ｜ 冲突 [0-9]+" "$WORK/lf-a.log" | head -1)
cb=$(grep -oE "文件 [0-9]+ ｜ 命中 [0-9]+ ｜ 已替换 [0-9]+ ｜ 冲突 [0-9]+" "$WORK/lf-b.log" | head -1)
if [ "$ca" = "文件 1 ｜ 命中 10 ｜ 已替换 0 ｜ 冲突 10" ] && [ "$cb" = "文件 1 ｜ 命中 10 ｜ 已替换 5 ｜ 冲突 5" ]; then
  ok "默认 $ca ／ 最长优先 $cb（abc 赢 bc：真改动 0 → 5；被挤的 5 处仍如实报出）"
else
  bad "最长优先裁决异常：默认[$ca] 最长[$cb]"
fi

# ② 模式标注要打出来（免得"到底哪种裁决"只能靠猜）
if grep -q "区间重叠按「最长匹配优先」裁决" "$WORK/lf-b.log"; then
  ok "执行模式行标出「最长匹配优先」"
else
  bad "最长优先模式下没标出裁决方式"
fi

# ③ 规则不重叠时，两种模式必须逐字节相同（证明默认路径没被改坏）
"$WREPL" apply "$SAMPLES/T-选项矩阵.docx" --rule "abc=>X" --out "$lf/c" > "$WORK/lf-c.log" 2>&1
"$WREPL" apply "$SAMPLES/T-选项矩阵.docx" --rule "abc=>X" --out "$lf/d" --longest-first > "$WORK/lf-d.log" 2>&1
if [ "$(sha "$lf/c/T-选项矩阵.docx")" = "$(sha "$lf/d/T-选项矩阵.docx")" ]; then
  ok "无重叠时两种裁决产物逐字节相同"
else
  bad "无重叠时产物居然不同 —— 默认路径被改坏了"
fi

# ④ 与 --chain 互斥：链式无重叠，两者并给是语义矛盾，必须明确拒绝而非静默忽略
"$WREPL" apply "$SAMPLES/T-选项矩阵.docx" --rule "abc=>X" --chain --longest-first \
  --out "$lf/a" > "$WORK/lf-e.log" 2>&1
rc=$?
if [ "$rc" -ne 0 ] && grep -q "不能同时使用" "$WORK/lf-e.log"; then
  ok "与 --chain 同时给 → 明确拒绝（退出码 $rc）"
else
  bad "矛盾组合未被拒绝（退出码 $rc）"
fi

# ───────────── 15. 就地替换 × 文件名同步改名 × 执行后验证 ─────────────
#
# 这一组钉的是"验证基准从哪来"。就地替换把源文件原地覆盖，**磁盘上再没有原件**；
# 若验证阶段还按 (src, dst) 两条路径去读，就地模式下这两条路径指的是同一个文件
# （拿产物跟它自己比 ⇒ 关卡 1/2 必然全等通过，等于没验），一旦还开了改名，
# src 路径连存在都不存在了 ⇒ 报「关卡1 读取失败：打不开文件」。
# 正确做法：写盘**前**采下基准，写盘**后**立刻比完。
head1 "15. 就地 + 改名 + 执行后验证（基准必须在覆盖前扣下）"
IV=$WORK/verify-inplace; mkdir -p "$IV/src"
IVF1="$F1"
IVF2="$F3"
for n in "$IVF1" "$IVF2"; do cp "$CORPUS/$n.docx" "$IV/src/$n.docx"; done

"$WREPL" apply "$IV/src" --rule "$P_OLD=>$P_NEW" --rename-files \
  --verify-after --report "$IV/报告.xlsx" > "$IV/run.log" 2>&1

# ① 全通过（修复前这里是「通过 0 / 不通过 2」）
if grep -q "自动验证：2 个文件　通过 2 / 不通过 0" "$IV/run.log"; then
  ok "就地+改名后执行后验证 2/2 通过"
else
  bad "就地+改名后验证未通过 —— 见 $IV/run.log"
fi

# ② 改名确实落到了磁盘（旧名不留、新名到位）
olds=0; news=0
for n in "$IVF1" "$IVF2"; do [ -f "$IV/src/$n.docx" ] && olds=$((olds+1)); done
for n in "$IV/src"/$P_NEW-*.docx "$IV/src"/标2-$P_NEW-*.docx; do
  [ -f "$n" ] && news=$((news+1))
done
if [ "$olds" = "0" ] && [ "$news" = "2" ]; then
  ok "就地改名落盘正确（旧名残留 0 / 新名 2）"
else
  bad "就地改名异常：旧名残留 $olds 个、新名 $news 个"
fi

# ③ ★ 关键判据：验证必须**真的比过两个不同状态**
#
# 拿产物跟它自己比时，关卡 1 的"改动 part"必然是空的，报告备注会退化成
# "改动 part："（冒号后面什么都没有）。非空才说明左侧基准确实是"覆盖前的原件"。
IV_OUT=$("$PY" - "$IV/报告.xlsx" "$P_NEW" <<'PYEOF'
import sys, openpyxl
P_NEW = sys.argv[2]
rows = list(openpyxl.load_workbook(sys.argv[1])["验证结论"].iter_rows(values_only=True))[1:]
print("ROWS=%d L1BAD=%d L2BAD=%d RESBAD=%d EMPTYNOTE=%d RENAMED=%d" % (
    len(rows),
    sum(1 for r in rows if r[1] != "通过"),
    sum(1 for r in rows if r[2] != "通过"),
    sum(1 for r in rows if r[3] != "无残留"),
    sum(1 for r in rows
        if (r[4] or "").strip().startswith("改动 part：")
        and len((r[4] or "").strip()) <= len("改动 part：")),
    sum(1 for r in rows if P_NEW in str(r[0])),
))
PYEOF
)
if [ "$IV_OUT" = "ROWS=2 L1BAD=0 L2BAD=0 RESBAD=0 EMPTYNOTE=0 RENAMED=2" ]; then
  ok "验证结论真的比过两个状态（关卡1 报出「改动 part」、且文件名已更新）"
else
  bad "验证结论存疑：$IV_OUT"
fi

# ④ 就地 + 不改名同样必须是真的验证（不许"拿文件跟自己比"蒙混过关）
IV2=$WORK/verify-inplace2; mkdir -p "$IV2/src"
for n in "$IVF1" "$IVF2"; do cp "$CORPUS/$n.docx" "$IV2/src/$n.docx"; done
"$WREPL" apply "$IV2/src" --rule "$P_OLD=>$P_NEW" \
  --verify-after --report "$IV2/报告.xlsx" > "$IV2/run.log" 2>&1
IV2_OUT=$("$PY" - "$IV2/报告.xlsx" <<'PYEOF'
import sys, openpyxl
rows = list(openpyxl.load_workbook(sys.argv[1])["验证结论"].iter_rows(values_only=True))[1:]
print("ROWS=%d EMPTYNOTE=%d BAD=%d" % (
    len(rows),
    sum(1 for r in rows
        if (r[4] or "").strip().startswith("改动 part：")
        and len((r[4] or "").strip()) <= len("改动 part：")),
    sum(1 for r in rows if r[1] != "通过" or r[2] != "通过" or r[3] != "无残留"),
))
PYEOF
)
if [ "$IV2_OUT" = "ROWS=2 EMPTYNOTE=0 BAD=0" ]; then
  ok "就地（不改名）也是真验证：2/2「改动 part」非空"
else
  bad "就地（不改名）验证存疑：$IV2_OUT"
fi

# ───────── 16. 写副本 × 未命中不落盘 × 执行后验证（没有产物的不能拿去验）─────────
#
# 写副本模式下"没改动的文件不写进输出目录"是**设计**（第 10 组钉着）。早先验证阶段
# 对这些文件照样拿 (源文件, 产物路径) 去比对，而那个产物路径压根不存在 ⇒ 整批报
#   「残留自检未能执行：打不开文件：…out/…docx: 系统找不到指定的文件 (os error 2)」
# 还把该行记成"不通过"。**没有产物，就没有"产物 vs 源文件"可比**——自检对象该退回
# 源文件，并在备注里写明「未产出」，否则一片"通过"会让人以为输出目录里有这个文件。
head1 "16. 写副本 + 未命中不落盘 + 执行后验证（没有产物的不能拿去验）"
VO=$WORK/verify-out; mkdir -p "$VO/src"
for n in "$F1" "$F2" "$F3"; do cp "$CORPUS/$n.docx" "$VO/src/$n.docx"; done
: > "$VO/before.txt"
for f in "$VO/src"/*.docx; do printf '%s\t%s\n' "$(sha "$f")" "$(basename "$f")" >> "$VO/before.txt"; done

# 只用前三条规则（$R1/$R2/$R3 来自第 10 组）：$F2（包装箱封面）正文里只有 $P_ALT，
# 前三条都打不到它 ⇒ 必然零命中 ⇒ 不落盘 —— 这正是触发条件，先跑出来。
"$WREPL" apply "$VO/src" --rule "$R1" --rule "$R2" --rule "$R3" \
  --out "$VO/out" --verify-after --report "$VO/报告.xlsx" > "$VO/run.log" 2>&1

# ① 前提必须成立，否则后面几条等于没测
n_out=$(ls "$VO"/out/*.docx 2>/dev/null | wc -l)
if [ "$n_out" = "2" ] && [ ! -f "$VO/out/$F2.docx" ]; then
  ok "前提成立：零命中的 $F2 不落盘（源 3 个 / 输出 2 个）"
else
  bad "前提不成立：输出目录 $n_out 个 —— 零命中文件被写进去了？见 $VO/run.log"
fi

# ② 不许再出现"拿不存在的产物去验"
if ! grep -q "打不开文件" "$VO/run.log" \
   && grep -q "自动验证：3 个文件　通过 3 / 不通过 0" "$VO/run.log"; then
  ok "写副本 + 验证：3/3 通过，不再报「打不开文件」"
else
  bad "写副本 + 验证仍然报错 —— 见 $VO/run.log"
fi

# ③ 那一行必须写明「未产出」，而不是含糊的一片"通过"
VO_OUT=$("$PY" - "$VO/报告.xlsx" <<'PYEOF'
import sys, openpyxl
rows = list(openpyxl.load_workbook(sys.argv[1])["验证结论"].iter_rows(values_only=True))[1:]
print("ROWS=%d NOTPROD=%d SELFFAIL=%d BAD=%d" % (
    len(rows),
    sum(1 for r in rows if str(r[4] or "").startswith("未产出（")),
    sum(1 for r in rows if r[3] == "自检失败"),
    sum(1 for r in rows if r[1] != "通过" or r[2] != "通过" or r[3] != "无残留"),
))
PYEOF
)
if [ "$VO_OUT" = "ROWS=3 NOTPROD=1 SELFFAIL=0 BAD=0" ]; then
  ok "验证结论：3 行、1 行写明「未产出」、无「自检失败」、无不合格"
else
  bad "验证结论不符合预期：$VO_OUT"
fi

# ④ 写副本模式源文件一个字节都不许动
moved=0
while IFS=$'\t' read -r h n; do
  [ "$(sha "$VO/src/$n")" != "$h" ] && moved=$((moved+1))
done < "$VO/before.txt"
if [ "$moved" = "0" ]; then
  ok "写副本模式源文件零改动（3/3 SHA256 未变）"
else
  bad "写副本模式居然改了源文件：$moved 个"
fi

# ⑤ ★ 输出目录里躺着**上一轮的同名旧文件**（这轮不再命中它）：
#    那不是本次产物 —— 不许拿去验证，也不许覆盖，只如实写进备注。
cp "$VO/src/$F2.docx" "$VO/out/$F2.docx"
stale_sha=$(sha "$VO/out/$F2.docx")
"$WREPL" apply "$VO/src" --rule "$R1" --rule "$R2" --rule "$R3" \
  --out "$VO/out" --verify-after --report "$VO/报告2.xlsx" > "$VO/run2.log" 2>&1
VO2=$("$PY" - "$VO/报告2.xlsx" <<'PYEOF'
import sys, openpyxl
rows = list(openpyxl.load_workbook(sys.argv[1])["验证结论"].iter_rows(values_only=True))[1:]
print("STALE=%d NOHIT=%d OPENERR=%d" % (
    sum(1 for r in rows if "已有同名旧文件" in str(r[4] or "")),
    sum(1 for r in rows if str(r[4] or "").startswith("未产出（")),
    sum(1 for r in rows if r[3] == "自检失败"),
))
PYEOF
)
if [ "$VO2" = "STALE=1 NOHIT=1 OPENERR=0" ] \
   && [ "$(sha "$VO/out/$F2.docx")" = "$stale_sha" ]; then
  ok "旧产物没被当成产物：备注点名、文件未被覆盖、未参与验证"
else
  bad "旧产物处理有问题：$VO2（或旧文件被覆盖了）见 $VO/run2.log"
fi

# ⑥ 同一个"产物路径一定存在"的假设还有第二处：**文件名**命中规则、正文零命中
#    （⇒ 不落盘）的文件，改名阶段照样对它 rename ⇒ 报「系统找不到指定的路径
#    (os error 3)」，看着像文件名同步功能坏了。没产物就没有文件可改名。
NM=$WORK/verify-out-name; mkdir -p "$NM/src"
cp "$CORPUS/$F2.docx" "$NM/src/封面-$P_OLD-模板.docx"
printf '封面\t封面A\t文件名\t\t只在文件名里命中\n' > "$NM/r.txt"
"$WREPL" apply "$NM/src" --rules-file "$NM/r.txt" \
  --out "$NM/out" --rename-files --report "$NM/报告.xlsx" > "$NM/run.log" 2>&1
NM_OUT=$("$PY" - "$NM/报告.xlsx" <<'PYEOF'
import sys, openpyxl
rows = list(openpyxl.load_workbook(sys.argv[1])["文件名对照"].iter_rows(values_only=True))[1:]
print("ROWS=%d OSERR=%d SKIP=%d CHANGED=%d" % (
    len(rows),
    sum(1 for r in rows if "os error" in str(r[4] or "")),
    sum(1 for r in rows if "改名未执行" in str(r[4] or "")),
    sum(1 for r in rows if r[3] == "是"),
))
PYEOF
)
n_out2=$(ls "$NM"/out/*.docx 2>/dev/null | wc -l)
if [ "$NM_OUT" = "ROWS=1 OSERR=0 SKIP=1 CHANGED=0" ] && [ "$n_out2" = "0" ] \
   && ! grep -q "os error" "$NM/run.log"; then
  ok "零命中文件不落盘 ⇒ 更不改名（不再报 os error 3）"
else
  bad "零命中文件的改名处理有问题：$NM_OUT（输出 $n_out2 个）见 $NM/run.log"
fi

# ───────── 17. 完整镜像（--mirror）：未改动的文件也进输出目录，且逐字节相同 ─────────
#
# 默认 `--out` 里只有改过的文件（第 10 / 16 组钉着），做交付包时输出目录拿不齐全。
# `--mirror` 把没改动的文件**原样复制**过去 —— 必须是 `fs::copy` 出来的逐字节相同，
# 不能走重打包：重打包会重排 zip 条目、换压缩参数，做不到逐字节相同，也就没法用
# 整文件 SHA256 自证复制无损。文件名同样按规则归一。
head1 "17. 完整镜像（--mirror）：未改动的文件也复制过去，且逐字节相同"
MI=$WORK/mirror; mkdir -p "$MI/src"
for n in "$F1" "$F2" "$F3"; do cp "$CORPUS/$n.docx" "$MI/src/$n.docx"; done
: > "$MI/src.sha"
for f in "$MI/src"/*.docx; do printf '%s\t%s\n' "$(sha "$f")" "$(basename "$f")" >> "$MI/src.sha"; done

"$WREPL" apply "$MI/src" --rule "$R1" --rule "$R2" --rule "$R3" \
  --out "$MI/out" --mirror --rename-files --verify-after --report "$MI/报告.xlsx" \
  > "$MI/run.log" 2>&1

# ① 前提：三个文件**全部**落进输出目录（零命中的 $F2 也在），否则后面几条等于空测。
#    先钉前提再断言结论 —— 哪天"未命中不落盘"的默认变了，这里会立刻报红，
#    而不是变成一组"白测还全绿"。
n_out=$(ls "$MI"/out/*.docx 2>/dev/null | wc -l)
if [ "$n_out" = "3" ] && [ -f "$MI/out/$F2.docx" ]; then
  ok "前提成立：未改动的 $F2 也进了输出目录（源 3 / 输出 3）"
else
  bad "前提不成立：输出目录 $n_out 个（应 3）—— 见 $MI/run.log"
fi

# ② 镜像件与源件**逐字节相同** —— 这就是"镜像"这个词的全部内容
if [ -f "$MI/out/$F2.docx" ] && [ "$(sha "$MI/out/$F2.docx")" = "$(sha "$MI/src/$F2.docx")" ]; then
  ok "镜像件与源件逐字节相同（$F2）"
else
  bad "镜像件与源件不一致 —— 见 $MI/run.log"
fi

# ③ 命中的文件照旧被真正改写（内容确实变了，不是"整包原样搬过去"）
MI_F1_NEW="${F1/$P_OLD/$P_NEW}"
if [ -f "$MI/out/$MI_F1_NEW.docx" ] \
   && [ "$(sha "$MI/out/$MI_F1_NEW.docx")" != "$(sha "$MI/src/$F1.docx")" ]; then
  ok "命中的文件内容仍被正确改写（$F1）"
else
  bad "命中的文件没被改写或改名不对 —— 见 $MI/run.log"
fi

# ④ 改名对两类文件都成立：改过的 $F1 换成新编号，镜像件保持原名（文件名里本就没编号）
if [ -f "$MI/out/$MI_F1_NEW.docx" ] && [ -f "$MI/out/$F2.docx" ]; then
  ok "文件名归一：改过的改名成功，镜像件保持原名"
else
  bad "输出目录文件名不对 —— 见 $MI/run.log"
fi

# ⑤ 报告里镜像行给的是"整文件逐字节一致"，且**不再出现「未产出」**
#    （镜像模式下每个输入文件都有产物，那条措辞不该再冒出来）
MI_REP=$("$PY" - "$MI/报告.xlsx" <<'PYEOF'
import sys, openpyxl
rows = list(openpyxl.load_workbook(sys.argv[1])["验证结论"].iter_rows(values_only=True))[1:]
note = lambda r: str(r[4] or "")
print("ROWS=%d BYTEID=%d NOPROD=%d BAD=%d" % (
    len(rows),
    sum(1 for r in rows if "整文件逐字节一致" in note(r)),
    sum(1 for r in rows if "未产出" in note(r)),
    sum(1 for r in rows if r[1] != "通过" or r[2] != "通过"),
))
PYEOF
)
if [ "$MI_REP" = "ROWS=3 BYTEID=1 NOPROD=0 BAD=0" ]; then
  ok "报告：镜像行标注「整文件逐字节一致」，且不再出现「未产出」"
else
  bad "镜像模式报告不符：$MI_REP"
fi

# ⑥ 镜像模式仍然**一个源文件都不许动**
mi_same=0
while IFS=$'\t' read -r h n; do
  [ "$(sha "$MI/src/$n")" = "$h" ] && mi_same=$((mi_same+1))
done < "$MI/src.sha"
if [ "$mi_same" = "3" ]; then
  ok "镜像模式源文件零改动（3/3 SHA256 未变）"
else
  bad "镜像模式动了源文件：只有 $mi_same/3 未变"
fi

# ⑦ 镜像只对写副本有意义：不给 --out 时必须**明确拒绝**，不许静默什么都不做
"$WREPL" apply "$MI/src" --rule "$R1" --mirror > "$MI/bad.log" 2>&1
rc=$?
if [ "$rc" != "0" ] && grep -q -- "--out" "$MI/bad.log"; then
  ok "--mirror 不给 --out 时明确拒绝（退出码 $rc）"
else
  bad "--mirror 缺少 --out 却没被拒绝：退出码 $rc —— 见 $MI/bad.log"
fi

# ───────── 18. 产物判据（真写过盘）与 CLI 退出码 ─────────
#
# ① 一个文件里既有"能落笔的命中"、又有"区间冲突的命中"时，它**是被改写过**的：
#    状态是 CONFLICT（如实表达"有冲突"），但内容确实落了盘。产物判据必须是
#    "本轮真写过盘"，不能是 `status == "OK"` —— 否则这种真产物会被当成"未产出"：
#    不改名、跳过格式验证，报告备注还会写成"输出目录里已有同名旧文件"（与实际相反）。
# ② 一批跑完但有文件报错时退出码必须是 2；全通过时是 0。
#    否则 `wrepl apply ... && echo ok` 这类链条在有失败时照样"成功"。
head1 "18. 产物判据（真写过盘）与 CLI 退出码"

PJ=$WORK/product-judge; mkdir -p "$PJ/out"
cp "$SAMPLES/T-选项矩阵.docx" "$PJ/T-选项矩阵.docx"
# abc / bc 全部重叠（5+5=10 处冲突）；saddow 单独命中 1 处（能落笔）；
# 选项→选择 只作用到文件名（样本正文里没有"选项"）。
"$WREPL" apply "$PJ/T-选项矩阵.docx" \
  --rule "abc=>X" --rule "bc=>Y" --rule "saddow=>S" --rule "选项=>选择" \
  --out "$PJ/out" --rename-files --verify-after --report "$PJ/报告.xlsx" \
  > "$PJ/run.log" 2>&1

# ① 前提必须是"部分落笔 + 部分冲突"，否则后面两条等于空测
pj_cnt=$(grep -oE "文件 [0-9]+ ｜ 命中 [0-9]+ ｜ 已替换 [0-9]+ ｜ 冲突 [0-9]+" "$PJ/run.log" | head -1)
if [ "$pj_cnt" = "文件 1 ｜ 命中 11 ｜ 已替换 1 ｜ 冲突 10" ] \
   && grep -q "状态：CONFLICT" "$PJ/run.log"; then
  ok "前提成立：$pj_cnt（状态 CONFLICT，但文件确实被改写过）"
else
  bad "前提不成立：$pj_cnt —— 见 $PJ/run.log"
fi

# ② 产物必须被认成"真产出"：改名落盘 + 验证结论不误判
pj_files=$(ls "$PJ"/out/*.docx 2>/dev/null | wc -l)
PJ_OUT=$("$PY" - "$PJ/报告.xlsx" <<'PYEOF'
import sys, openpyxl
rows = list(openpyxl.load_workbook(sys.argv[1])["验证结论"].iter_rows(values_only=True))[1:]
print("ROWS=%d NOTPROD=%d STALE=%d L1BAD=%d L2BAD=%d" % (
    len(rows),
    sum(1 for r in rows if "未产出" in str(r[4] or "")),
    sum(1 for r in rows if "已有同名旧文件" in str(r[4] or "")),
    sum(1 for r in rows if r[1] != "通过"),
    sum(1 for r in rows if r[2] != "通过"),
))
PYEOF
)
if [ "$pj_files" = "1" ] && [ -f "$PJ/out/T-选择矩阵.docx" ] \
   && [ ! -f "$PJ/out/T-选项矩阵.docx" ] \
   && [ "$PJ_OUT" = "ROWS=1 NOTPROD=0 STALE=0 L1BAD=0 L2BAD=0" ]; then
  ok "冲突文件仍算产出：产物已改名（T-选择矩阵.docx），验证结论未误判"
else
  bad "产物判据不对：输出 $pj_files 个 / $PJ_OUT —— 见 $PJ/run.log"
fi

# ③ 干净一批（有改动、无冲突、无残留）退出码必须是 0
PJC=$WORK/product-judge-clean; mkdir -p "$PJC/out"
cp "$SAMPLES/T-选项矩阵.docx" "$PJC/x.docx"
"$WREPL" apply "$PJC/x.docx" --rule "saddow=>S" --out "$PJC/out" --verify-after \
  > "$PJC/run.log" 2>&1
clean_rc=$?
if [ "$clean_rc" = "0" ] && grep -q "自动验证：1 个文件　通过 1 / 不通过 0" "$PJC/run.log"; then
  ok "全部通过时退出码 0"
else
  bad "全部通过时退出码不是 0（实际 $clean_rc）—— 见 $PJC/run.log"
fi

# ④ 有文件处理失败时退出码必须是 2（损坏的 docx 真值触发 ERROR）
PJB=$WORK/product-judge-broken; mkdir -p "$PJB/out"
cp "$SAMPLES/T-选项矩阵.docx" "$PJB/ok.docx"
printf 'this is not a zip file' > "$PJB/broken.docx"
"$WREPL" apply "$PJB" --rule "saddow=>S" --out "$PJB/out" > "$PJB/run.log" 2>&1
brk_rc=$?
if [ "$brk_rc" = "2" ] && grep -q "状态：ERROR" "$PJB/run.log"; then
  ok "有文件报错时退出码 2（实际 $brk_rc）"
else
  bad "有文件报错时退出码应为 2，实际 $brk_rc —— 见 $PJB/run.log"
fi

# ───── 19. 本轮修复的三条语义：CR 编码 / 跨段残留 / scan 裁决 ─────
#
# 这三条都是「静默退化型」的：坏了不报错，只是悄悄改错字节、或报出不存在的残留。
# 逻辑全在 regress-group19.py 里（Python 侧做 docx 夹具与断言，免得 shell 引号打架），
# 这里只负责跑它、把输出转出来、把它的计数并进总表。
head1 "19. 修复回归：CR 编码、跨段假命中、scan 与 apply 裁决一致"
g19_log="$WORK/g19.log"
"$PY" "$SRC/regress-group19.py" "$WREPL" "$WORK/g19" > "$g19_log" 2>&1
sed 's/^/  /' "$g19_log"
p19=$(grep -oE '^PASS [0-9]+' "$g19_log" | tail -1 | awk '{print $2}')
f19=$(grep -oE '^FAIL [0-9]+' "$g19_log" | tail -1 | awk '{print $2}')
if [ -n "${p19:-}" ]; then
  PASS=$((PASS + p19))
  FAIL=$((FAIL + ${f19:-0}))
else
  FAIL=$((FAIL + 1))
  printf '  \033[31m✗\033[0m 第 19 组没有输出计数行（脚本崩了？）—— 见 %s\n' "$g19_log"
fi

# ───────────────────── 汇总 ─────────────────────
printf '\n\033[1m══════════ 汇总：通过 %d ／ 失败 %d ══════════\033[0m\n' "$PASS" "$FAIL"
if [ "$FAIL" -ne 0 ]; then
  printf '\033[31m有失败项，中间产物留在 %s 便于排查\033[0m\n' "$WORK"
  exit 1
fi
printf '\033[32m全部通过。\033[0m\n'
printf '\n本机环境备忘：\n'
printf '  · C: 盘禁止任何非系统进程写入/执行（含 sandbox 之外）——构建必须放 D:。\n'
printf '  · 工具链 stable-x86_64-pc-windows-gnu（本机无 MSVC Build Tools）。\n'
printf '  · 构建一律加 --offline（在线会卡在 registry 锁）。\n'
