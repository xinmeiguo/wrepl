$ErrorActionPreference = 'Stop'
$root = 'D:\test\wrepl'
$old  = "$root\bench\baseline\wrepl-baseline.exe"
$new  = "$root\target\release\wrepl.exe"
$rules = "$root\bench\rules.txt"
$small = "$root\bench\small"
$work  = "$root\bench\ip"
$big   = (Get-ChildItem 'E:\test\docx' -Recurse -Filter *.docx -File | Sort-Object Length -Descending)[0].FullName

function Prep([string]$d) {
    if (Test-Path $d) { Remove-Item $d -Recurse -Force }
    Copy-Item $small $d -Recurse
    return $d
}

function Best([string]$exe, [string]$dir, [string[]]$extra, [int]$runs = 5) {
    $best = [double]::MaxValue
    for ($i = 0; $i -lt $runs; $i++) {
        Prep $dir | Out-Null
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        & $exe apply $dir --rules-file $rules --threads 4 @extra 2>&1 | Out-Null
        $sw.Stop()
        if ($sw.Elapsed.TotalMilliseconds -lt $best) { $best = $sw.Elapsed.TotalMilliseconds }
    }
    return $best
}

"== 就地替换：25 个真实文件（~2.7 MB）=="
foreach ($case in @(
    @('in-place',                 @()),
    @('in-place --verify-after',  @('--verify-after'))
)) {
    $label = $case[0]; $extra = $case[1]
    $a = Best $old "$work\o" $extra
    $b = Best $new "$work\n" $extra
    "{0,-30} baseline {1,8:N0} ms   new {2,8:N0} ms   {3:N2}x" -f $label, $a, $b, ($a / $b)
}

""
"== 就地替换：单个 8.8 MB 文件 =="
$one = "$work\big"
if (Test-Path $one) { Remove-Item $one -Recurse -Force }
New-Item -ItemType Directory -Force -Path $one | Out-Null
Copy-Item $big $one
foreach ($case in @(
    @('in-place',                @()),
    @('in-place --verify-after', @('--verify-after'))
)) {
    $label = $case[0]; $extra = $case[1]
    $a = [double]::MaxValue; $b = [double]::MaxValue
    for ($i = 0; $i -lt 5; $i++) {
        Copy-Item $big $one -Force
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        & $old apply $one --rules-file $rules @extra 2>&1 | Out-Null
        $sw.Stop(); if ($sw.Elapsed.TotalMilliseconds -lt $a) { $a = $sw.Elapsed.TotalMilliseconds }
        Copy-Item $big $one -Force
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        & $new apply $one --rules-file $rules @extra 2>&1 | Out-Null
        $sw.Stop(); if ($sw.Elapsed.TotalMilliseconds -lt $b) { $b = $sw.Elapsed.TotalMilliseconds }
    }
    "{0,-30} baseline {1,8:N0} ms   new {2,8:N0} ms   {3:N2}x" -f $label, $a, $b, ($a / $b)
}
