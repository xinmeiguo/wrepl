$ErrorActionPreference = 'Stop'
$root   = 'D:\test\wrepl'
$exe    = "$root\target\release\wrepl.exe"
$corpus = 'E:\test\docx'
$rules  = "$root\bench\rules.txt"
$scratch = "$root\bench\out"

function Time-It ([string]$label, [scriptblock]$body) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    & $body | Out-Null
    $sw.Stop()
    "{0,-34} {1,8:N0} ms" -f $label, $sw.Elapsed.TotalMilliseconds
}

if (Test-Path $scratch) { Remove-Item $scratch -Recurse -Force }
New-Item -ItemType Directory -Path $scratch | Out-Null

# --- 启动开销 ---
Time-It 'startup: --version'      { & $exe --version }
Time-It 'startup: --help'         { & $exe --help }

# --- scan / dry-run ---
Time-It 'scan 26 files'           { & $exe scan $corpus --rules-file $rules }
Time-It 'apply --dry-run'         { & $exe apply $corpus --rules-file $rules --dry-run }

# --- 写副本 ---
Time-It 'apply --out'             { & $exe apply $corpus --rules-file $rules --out "$scratch\a" }
Time-It 'apply --out --rename'    { & $exe apply $corpus --rules-file $rules --out "$scratch\b" --rename-files }
Time-It 'apply --out --verify'    { & $exe apply $corpus --rules-file $rules --out "$scratch\c" --verify-after }
Time-It 'apply --out --mirror'    { & $exe apply $corpus --rules-file $rules --out "$scratch\d" --mirror }
Time-It 'apply --out all opts'    { & $exe apply $corpus --rules-file $rules --out "$scratch\e" --mirror --rename-files --verify-after }
