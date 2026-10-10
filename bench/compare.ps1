# 用同一套参数跑「基线 exe」与「新 exe」，逐字节比对全部产物。
#
#   pwsh -NoProfile -File bench\compare.ps1
#
# 覆盖：写副本 / 完整镜像 / 就地替换 / 改名 / 验证 / 报告 / 链式 /
#       最长匹配优先 / scan / dump / inspect / probe / rules / verify --batch。
#
# 两次运行**用同一个输出路径**（跑完再改名搬走），这样报告里记的「输入 / 输出」
# 路径也完全一致 —— 只剩「报告生成时间」这一个天然变量，交给 xlsx_diff.py 忽略。

$ErrorActionPreference = 'Stop'
$root   = 'D:\test\wrepl'
$new    = "$root\target\release\wrepl.exe"
$old    = "$root\bench\baseline\wrepl-baseline.exe"
$rules  = "$root\bench\rules.txt"
$corpus = 'E:\test\docx'
$small  = "$root\bench\small"
$work   = "$root\bench\cmp"
# 跑 xlsx_diff.py 用的解释器。默认用 PATH 里的 python；本机可设 WREPL_PY 指到具体解释器。
$py     = if ($env:WREPL_PY) { $env:WREPL_PY } else { 'python' }
# 本机控制台是 GBK：不指定的话，`xlsx_diff.py` 里的中文会以 GBK 打出来，读起来全是乱码
$env:PYTHONIOENCODING = 'utf-8'

# 「小文件语料」是从真实语料里挑出来的 <1 MB 那批（用来自查更细的路径，
# 也避免把 8.8 MB 的大文件反复复制）。没有就现做。
if (-not (Test-Path $small)) {
    New-Item -ItemType Directory -Force -Path $small | Out-Null
    Get-ChildItem $corpus -Recurse -Filter *.docx -File |
        Where-Object { $_.Length -lt 1MB } | Copy-Item -Destination $small
}

if (Test-Path $work) { Remove-Item $work -Recurse -Force }
New-Item -ItemType Directory -Path $work | Out-Null

$script:fail = 0

function Hash-Tree([string]$dir) {
    if (-not (Test-Path $dir)) { return @() }
    Get-ChildItem $dir -Recurse -File | Sort-Object FullName | ForEach-Object {
        $rel = $_.FullName.Substring($dir.Length).TrimStart('\')
        if ($rel -like '*.xlsx') { return }      # 报告单独比内容（有时间戳）
        $h = (Get-FileHash $_.FullName -Algorithm SHA256).Hash
        "$rel  $h"
    }
}

# 跑一趟 exe，把 stdout/stderr 都收下来；返回 @{ Out; Err; Code }
function Run-Exe([string]$exe, [string[]]$argv) {
    $o = & $exe @argv 2>&1 | ForEach-Object { "$_" }
    return @{ Out = ($o -join "`n"); Code = $LASTEXITCODE }
}

# 产物类用例：mk 收一个「输出目录」，把产物写进去
function Case([string]$name, [scriptblock]$mk) {
    $live = Join-Path $work "$name`_live"
    $a = Join-Path $work "$name`_old"
    $b = Join-Path $work "$name`_new"

    New-Item -ItemType Directory -Force -Path $live | Out-Null
    & $mk $old $live | Out-Null
    Move-Item $live $a
    New-Item -ItemType Directory -Force -Path $live | Out-Null
    & $mk $new $live | Out-Null
    Move-Item $live $b

    $ha = (Hash-Tree $a) -join "`n"
    $hb = (Hash-Tree $b) -join "`n"
    if ($ha -eq $hb) {
        "  OK   $name"
    } else {
        $script:fail++
        "  FAIL $name（产物字节不同）"
        Compare-Object ($ha -split "`n") ($hb -split "`n") | Select-Object -First 20 |
            ForEach-Object { "        $($_.SideIndicator) $($_.InputObject)" }
    }

    foreach ($x in @(Get-ChildItem $a -Filter *.xlsx -ErrorAction SilentlyContinue)) {
        $y = Join-Path $b $x.Name
        if (-not (Test-Path $y)) { $script:fail++; "  FAIL $name / $($x.Name)：新版本没生成"; continue }
        $r = & $py "$root\bench\xlsx_diff.py" $x.FullName $y
        if ($LASTEXITCODE -ne 0) {
            $script:fail++
            "  FAIL $name / $($x.Name)"
            $r | ForEach-Object { "        $_" }
        } else {
            "  OK   $name / $($x.Name)"
        }
    }
}

# 就地类用例：两次运行分别在各自目录里改（输入路径不同，但都不产报告）
function InPlace-Case([string]$name, [string[]]$extra) {
    $a = Join-Path $work "$name`_old"
    $b = Join-Path $work "$name`_new"
    Copy-Item $small $a -Recurse
    Copy-Item $small $b -Recurse
    $ra = Run-Exe $old (@('apply', $a, '--rules-file', $rules, '--threads', '1') + $extra)
    $rb = Run-Exe $new (@('apply', $b, '--rules-file', $rules, '--threads', '1') + $extra)
    # stdout 里的路径不同是必然的，抹掉路径后再比
    $ta = $ra.Out.Replace($a, '<IN>')
    $tb = $rb.Out.Replace($b, '<IN>')
    $ha = (Hash-Tree $a) -join "`n"
    $hb = (Hash-Tree $b) -join "`n"
    if ($ha -eq $hb -and $ta -eq $tb -and $ra.Code -eq $rb.Code) {
        "  OK   $name（产物 + stdout + 退出码）"
    } else {
        $script:fail++
        "  FAIL $name"
        if ($ha -ne $hb) {
            Compare-Object ($ha -split "`n") ($hb -split "`n") | Select-Object -First 20 |
                ForEach-Object { "        产物 $($_.SideIndicator) $($_.InputObject)" }
        }
        if ($ta -ne $tb) {
            Compare-Object ($ta -split "`r?`n") ($tb -split "`r?`n") | Select-Object -First 20 |
                ForEach-Object { "        输出 $($_.SideIndicator) $($_.InputObject)" }
        }
        if ($ra.Code -ne $rb.Code) { "        退出码 $($ra.Code) vs $($rb.Code)" }
    }
}

# 纯 stdout 用例（并行下完成顺序本就不定，一律 --threads 1）
function Text-Case([string]$name, [scriptblock]$mk) {
    $argv = & $mk
    $a = Run-Exe $old $argv
    $b = Run-Exe $new $argv
    if ($a.Out -eq $b.Out -and $a.Code -eq $b.Code) { "  OK   $name" }
    else {
        $script:fail++
        "  FAIL $name"
        Compare-Object ($a.Out -split "`r?`n") ($b.Out -split "`r?`n") | Select-Object -First 20 |
            ForEach-Object { "        $($_.SideIndicator) $($_.InputObject)" }
        if ($a.Code -ne $b.Code) { "        退出码 $($a.Code) vs $($b.Code)" }
    }
}

"== 产物逐字节（docx 全量 SHA256 比对） =="
Case 'copy'          { param($exe, $out) & $exe apply $corpus --rules-file $rules --out $out --verify-after --report "$out\rep.xlsx" }
Case 'mirror_rename' { param($exe, $out) & $exe apply $corpus --rules-file $rules --out $out --mirror --rename-files --verify-after --report "$out\rep.xlsx" }
Case 'chain'         { param($exe, $out) & $exe apply $corpus --rules-file $rules --out $out --chain --verify-after }
Case 'longest'       { param($exe, $out) & $exe apply $corpus --rules-file $rules --out $out --longest-first --verify-after }
Case 'small'         { param($exe, $out) & $exe apply $small --rules-file $rules --out $out --verify-after --report "$out\rep.xlsx" }
Case 'threads1'      { param($exe, $out) & $exe apply $small --rules-file $rules --out $out --threads 1 --verify-after --mirror }
Case 'dry'           { param($exe, $out) & $exe apply $small --rules-file $rules --out $out --dry-run }

"== 就地替换 =="
InPlace-Case 'inplace'        @()
InPlace-Case 'inplace_verify' @('--verify-after')
InPlace-Case 'inplace_bak'    @('--backup', '--verify-after')
InPlace-Case 'inplace_rename' @('--rename-files', '--verify-after')

"== stdout 一致性 =="
Text-Case 'scan'         { @('scan', $small, '--rules-file', $rules, '--threads', '1') }
Text-Case 'dump'         { @('dump', (Get-ChildItem $small -Filter *.docx | Select-Object -First 1).FullName, '--limit', '3') }
Text-Case 'inspect'      { @('inspect', (Get-ChildItem $small -Filter *.docx | Select-Object -First 1).FullName) }
Text-Case 'probe'        { @('probe', $small) }
Text-Case 'rules_dump'   { @('rules', 'dump', '--rules-file', $rules) }
Text-Case 'rules_check'  { @('rules', 'check', '--rules-file', $rules) }
Text-Case 'apply_dry'    { @('apply', $small, '--rules-file', $rules, '--dry-run', '--threads', '1') }
Text-Case 'verify_batch' { @('verify', (Join-Path $work 'copy_old'), (Join-Path $work 'mirror_rename_old'), '--batch') }
Text-Case 'verify_pair'  { @('verify', (Get-ChildItem $small -Filter *.docx | Select-Object -First 1).FullName, (Get-ChildItem (Join-Path $work 'copy_old') -Filter *.docx | Select-Object -First 1).FullName) }

""
if ($fail -eq 0) { "全部一致 ✓"; exit 0 }
"不一致：$fail 项"
exit 1
