#!/usr/bin/env bash
# ── 回归语料配置：示例 ───────────────────────────────────────────────────
#
# 用法：复制成 regress-samples.local.sh（那份 .gitignore 已排除），改成本机值。
#
#   cp regress-samples.example.sh regress-samples.local.sh
#
# 为什么这么绕：regress.sh 跑的是**真实交付包**，里面的项目编号与客户名属于
# 商业信息，不适合随源码公开；可脚本又必须靠这些字面量去 cp 文件、grep 期望值。
# 于是把语料相关的东西全部提成变量，脱敏占位写在 regress.sh 里，真实值放这儿。
#
# 没有这个文件也能跑 —— 语法没毛病，但 cp 找不到文件、grep 对不上期望值，
# 会整片报红。这是设计如此，不是坏了。

# ── 需要的环境 ─────────────────────────────────────────────────────────
# 1. 一份待替换的 docx 目录（回归里的"真实交付包"）
# 2. 该目录里**真实存在**的项目编号 / 客户名，以及要把它们换成什么
# 3. 若干单文件样本（passthrough / 选项矩阵 / 最长匹配那几组要用）
#    文件名：<P_OLD 形态>-OQ-模板-*.docx、S-全坑样本.docx、T-选项矩阵.docx、
#            A-URS.docx、B-点检表.docx、C-电气图档.docx
# 4. 端到端用的规则文件 <RUN_DIR>/rules-<P_NEW>.txt

SRC="D:/test/wrepl"          # 仓库根目录
PY="python3"                 # 需要 openpyxl

CORPUS="./corpus"            # 待替换的 docx 目录
CORPUS_BASELINE=""           # 历史基线产物目录；留空则跳过"零漂移"那一项
RUN_DIR="./corpus/run"
# 端到端的规则文件。**显式给出**更保险 —— 默认是按 `<RUN_DIR>/rules-<P_NEW>.txt`
# 推的，而实际文件名常常不带年份前缀（如 rules-001CE.txt），推不出来。
RULES_FILE="./corpus/run/rules-001CE.txt"
SAMPLES="./samples"
MATRIX="./run_option_matrix.py"

SAMPLE_REAL="REAL-2025-001AUTc-OQ-模板-2026-06-22"

# 语料里的字面量
P_OLD="2026-001CE"
P_NEW="2026-002CE"
P_ALT="2022-007CE"
C_OLD="某某制药有限公司"
C_NEW="某某生物"
C_OLD_EN="Xxx Pharmaceutical Co., Ltd."
C_NEW_EN="Xxx Biology"
