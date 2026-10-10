$ErrorActionPreference = 'Stop'
$root = 'D:\test\wrepl'
$old  = "$root\bench\baseline\wrepl-baseline.exe"
$new  = "$root\target\release\wrepl.exe"
$rules = "$root\bench\rules.txt"
$corpus = 'E:\test\docx'
$run   = "$root\bench\run"
$big   = (Get-ChildItem 'E:\test\docx' -Recurse -Filter *.docx -File | Sort-Object Length -Descending)[0].FullName
$bigdir = "$run\onebig"

if (Test-Path $run) { Remove-Item $run -Recurse -Force }
New-Item -ItemType Directory -Path $run | Out-Null
New-Item -ItemType Directory -Path $bigdir | Out-Null
Copy-Item $big $bigdir

function Best([string]$exe, [string[]]$argv, [int]$runs = 4) {
    $best = [double]::MaxValue
    for ($i = 0; $i -lt $runs; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        & $exe @argv 2>&1 | Out-Null
        $sw.Stop()
        if ($sw.Elapsed.TotalMilliseconds -lt $best) { $best = $sw.Elapsed.TotalMilliseconds }
    }
    return $best
}

function Row([string]$label, [string[]]$a1, [string[]]$a2) {
    if (-not $a2) { $a2 = $a1 }
    $va = Best $old $a1
    $vb = Best $new $a2
    "{0,-48} {1,8:N0} → {2,7:N0} ms   {3,5:N2}x" -f $label, $va, $vb, ($va / $vb)
}

"== 全语料 26 个文件（11.2 MB）=="
Row 'scan'                                @('scan', $corpus, '--rules-file', $rules)
Row 'apply --dry-run'                     @('apply', $corpus, '--rules-file', $rules, '--dry-run')
Row 'apply --out'                         @('apply', $corpus, '--rules-file', $rules, '--out', "$run\a")
Row 'apply --out --verify-after'          @('apply', $corpus, '--rules-file', $rules, '--out', "$run\b", '--verify-after')
Row 'apply --out --verify --report'       @('apply', $corpus, '--rules-file', $rules, '--out', "$run\c", '--verify-after', '--report', "$run\c.xlsx")
Row 'apply --out --mirror --verify --report' @('apply', $corpus, '--rules-file', $rules, '--out', "$run\d", '--mirror', '--verify-after', '--report', "$run\d.xlsx")

""
"== 单个 8.8 MB 文件 =="
Row 'scan'                                @('scan', $bigdir, '--rules-file', $rules)
Row 'apply --out'                         @('apply', $bigdir, '--rules-file', $rules, '--out', "$run\ba")
Row 'apply --out --verify-after'          @('apply', $bigdir, '--rules-file', $rules, '--out', "$run\bb", '--verify-after')
Row 'apply --out --verify --report'       @('apply', $bigdir, '--rules-file', $rules, '--out', "$run\bc", '--verify-after', '--report', "$run\bc.xlsx")

""
"== 25 个小文件（~2.7 MB）=="
Row 'apply --out --verify-after'          @('apply', "$root\bench\small", '--rules-file', $rules, '--out', "$run\sm", '--verify-after')
Row 'apply --out --verify --report'       @('apply', "$root\bench\small", '--rules-file', $rules, '--out', "$run\sm2", '--verify-after', '--report', "$run\sm2.xlsx")
