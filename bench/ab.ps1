$ErrorActionPreference = 'Stop'
$root = 'D:\test\wrepl'
$old  = "$root\bench\baseline-target\release\examples\profile.exe"
$new  = "$root\target\release\examples\profile.exe"
$rules = "$root\bench\rules.txt"

$big = (Get-ChildItem 'E:\test\docx' -Recurse -Filter *.docx -File | Sort-Object Length -Descending)[0].FullName
$mid = (Get-ChildItem 'E:\test\docx' -Recurse -Filter *.docx -File | Where-Object { $_.Length -gt 200KB -and $_.Length -lt 400KB })[0].FullName

# 从 profile 输出里抓 (标签 -> 最小值)
function Collect([string]$exe, [string]$file, [int]$runs = 5) {
    $best = @{}
    for ($i = 0; $i -lt $runs; $i++) {
        & $exe $file $rules 2>&1 | ForEach-Object {
            if ($_ -match '^\s{2}(.+?)\s{2,}([\d.]+) ms\s*$') {
                $k = $Matches[1].Trim() -replace ' #\d+$', ''
                $v = [double]$Matches[2]
                if (-not $best.ContainsKey($k) -or $v -lt $best[$k]) { $best[$k] = $v }
            }
        }
    }
    return $best
}

foreach ($pair in @(@('MID 278KB', $mid), @('BIG 8.8MB', $big))) {
    $name = $pair[0]; $file = $pair[1]
    "===== $name ====="
    $a = Collect $old $file
    $b = Collect $new $file
    $keys = $a.Keys + $b.Keys | Sort-Object -Unique
    "{0,-52} {1,9} {2,9} {3,8}" -f 'step', 'baseline', 'new', 'speedup'
    foreach ($k in $keys) {
        $va = if ($a.ContainsKey($k)) { $a[$k] } else { [double]::NaN }
        $vb = if ($b.ContainsKey($k)) { $b[$k] } else { [double]::NaN }
        $sp = if ($va -gt 0 -and $vb -gt 0) { '{0:N2}x' -f ($va / $vb) } else { '' }
        "{0,-52} {1,9:N1} {2,9:N1} {3,8}" -f $k, $va, $vb, $sp
    }
    ""
}
