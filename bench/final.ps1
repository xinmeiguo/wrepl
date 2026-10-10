$ErrorActionPreference = 'Stop'
$root = 'D:\test\wrepl'
$old  = "$root\bench\baseline\wrepl-baseline.exe"
$new  = "$root\target\release\wrepl.exe"
$rules = "$root\bench\rules.txt"
$corpus = 'E:\test\docx'
$run   = "$root\bench\run"
$big   = (Get-ChildItem 'E:\test\docx' -Recurse -Filter *.docx -File | Sort-Object Length -Descending)[0].FullName
$bigdir = "$run\onebig"
$small = "$root\bench\small"

$env:PYTHONIOENCODING = 'utf-8'

# <1 MB 的那批样本：没有就现从语料里挑。
# （8.8 MB 那个大文件混进来只会盖住小文件的信号，所以单列一组。）
if (-not (Test-Path $small)) {
    New-Item -ItemType Directory -Force -Path $small | Out-Null
    Get-ChildItem $corpus -Recurse -Filter *.docx -File |
        Where-Object { $_.Length -lt 1MB } | Copy-Item -Destination $small
}

if (Test-Path $run) { Remove-Item $run -Recurse -Force }
New-Item -ItemType Directory -Path $run | Out-Null
New-Item -ItemType Directory -Path $bigdir | Out-Null
Copy-Item $big $bigdir

# 交错跑 A/B，各自取最小值 —— 单侧连着跑会被磁盘/杀软的时序漂移带偏
function Best2([string[]]$argv, [int]$runs = 6) {
    $ba = [double]::MaxValue; $bb = [double]::MaxValue
    for ($i = 0; $i -lt $runs; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew(); & $old @argv 2>&1 | Out-Null; $sw.Stop()
        if ($sw.Elapsed.TotalMilliseconds -lt $ba) { $ba = $sw.Elapsed.TotalMilliseconds }
        $sw = [System.Diagnostics.Stopwatch]::StartNew(); & $new @argv 2>&1 | Out-Null; $sw.Stop()
        if ($sw.Elapsed.TotalMilliseconds -lt $bb) { $bb = $sw.Elapsed.TotalMilliseconds }
    }
    return @($ba, $bb)
}

# 进程启动 + clap 解析的本底开销（两侧都含，扣掉它才是真正的"干活时间"）
$base = Best2 @('--version') 10
$floor = [Math]::Min($base[0], $base[1])
"进程启动本底（--version）：$([Math]::Round($floor)) ms   ← 下面的数字都含它"
""
"{0,-46} {1,9} {2,9} {3,8} {4,12} {5,12}" -f '场景', '基线 ms', '新版 ms', '加速', '净耗时 基线', '净耗时 新版'
"{0,-46} {1,9} {2,9} {3,8} {4,12} {5,12}" -f ('-' * 46), ('-' * 9), ('-' * 9), ('-' * 8), ('-' * 12), ('-' * 12)

function Row([string]$label, [string[]]$argv, [int]$runs = 6) {
    $r = Best2 $argv $runs
    $va = $r[0]; $vb = $r[1]
    $na = [Math]::Max(0, $va - $floor); $nb = [Math]::Max(0, $vb - $floor)
    $sp = if ($vb -gt 0) { $va / $vb } else { 0 }
    $spn = if ($nb -gt 0.5) { '{0:N2}x' -f ($na / $nb) } else { '—' }
    "{0,-46} {1,9:N0} {2,9:N0} {3,7:N2}x {4,9:N0} ms {5,9:N0} ms  {6}" -f $label, $va, $vb, $sp, $na, $nb, $spn
}

'── 全语料 26 个文件（11.2 MB）──'
Row 'scan（只扫不落盘）'                  @('scan', $corpus, '--rules-file', $rules)
Row 'apply --dry-run'                     @('apply', $corpus, '--rules-file', $rules, '--dry-run')
Row 'apply --out'                         @('apply', $corpus, '--rules-file', $rules, '--out', "$run\a")
Row 'apply --out --verify-after'          @('apply', $corpus, '--rules-file', $rules, '--out', "$run\b", '--verify-after')
Row 'apply --out --verify --report'       @('apply', $corpus, '--rules-file', $rules, '--out', "$run\c", '--verify-after', '--report', "$run\c.xlsx")
Row 'apply --out --mirror --verify --report' @('apply', $corpus, '--rules-file', $rules, '--out', "$run\d", '--mirror', '--verify-after', '--report', "$run\d.xlsx")
''
'── 单个 8.8 MB 文件（含大图，媒体型）──'
Row 'scan'                                @('scan', $bigdir, '--rules-file', $rules)
Row 'apply --out'                         @('apply', $bigdir, '--rules-file', $rules, '--out', "$run\ba")
Row 'apply --out --verify-after'          @('apply', $bigdir, '--rules-file', $rules, '--out', "$run\bb", '--verify-after')
Row 'apply --out --verify --report'       @('apply', $bigdir, '--rules-file', $rules, '--out', "$run\bc", '--verify-after', '--report', "$run\bc.xlsx")
''
'── 25 个小文件（~2.7 MB，文本型，最贴近真实交付包）──'
Row 'apply --out --verify-after'          @('apply', $small, '--rules-file', $rules, '--out', "$run\sm", '--verify-after')
Row 'apply --out --verify --report'       @('apply', $small, '--rules-file', $rules, '--out', "$run\sm2", '--verify-after', '--report', "$run\sm2.xlsx")
