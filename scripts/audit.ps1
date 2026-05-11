# scripts/audit.ps1 -- nightdrive single-command discipline gate.
#
# Sections (numbered [N/7] so honesty-auditor and roadmap-tracker can
# quote counts verbatim):
#   [1/7] build         -- cargo check --workspace, error count
#   [2/7] test          -- cargo test --workspace, pass count
#   [3/7] stub inventory-- todo!() / unimplemented!() / warn!("not yet implemented") / // TODO(nightdrive)
#   [4/7] stage witnesses- count // stage: N markers under tests/witnesses/
#   [5/7] bench freshness- last row in BENCH_LEDGER.md vs. 7-day rule
#   [6/7] schema drift  -- HANDOFF.md section 7 schema vs. 20260510000000_init.sql column names
#   [7/7] summary       -- "OK - audit clean" or "FAIL - <list>"
#
# Pure PowerShell -- no aether-audit.exe equivalent (yet). Emits final exit
# code 0 on clean, 1 on any FAIL section, 2 on script error.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$failures = @()

# --- [1/7] build ----------------------------------------------------------
Write-Host "==> [1/7] build"
$prev = $ErrorActionPreference; $ErrorActionPreference = "Continue"
$buildOutput = & cargo check --workspace --quiet 2>&1
$buildErrors = ($buildOutput | Select-String -Pattern "^error" -CaseSensitive).Count
$ErrorActionPreference = $prev
Write-Host "    cargo check errors: $buildErrors"
if ($buildErrors -gt 0) { $failures += "build:$buildErrors" }

# --- [2/7] test -----------------------------------------------------------
Write-Host "==> [2/7] test"
$ErrorActionPreference = "Continue"
$testOutput = & cargo test --workspace --quiet 2>&1
$testFails = ($testOutput | Select-String -Pattern "^test result: FAILED" -CaseSensitive).Count
$ErrorActionPreference = $prev
Write-Host "    cargo test failed-suites: $testFails"
if ($testFails -gt 0) { $failures += "test:$testFails" }

# --- [3/7] stub inventory ------------------------------------------------
Write-Host "==> [3/7] stub inventory"
$stubPatterns = @(
    'todo!\(',
    'unimplemented!\(',
    'unreachable!\(',
    'warn!\("not yet implemented"',
    '// TODO\(nightdrive\)'
)
$stubCount = 0
$stubByFile = @{}
if (Test-Path "crates") {
    $rsFiles = Get-ChildItem -Path crates -Recurse -Filter "*.rs" -ErrorAction SilentlyContinue
    foreach ($file in $rsFiles) {
        $content = Get-Content -Path $file.FullName -Raw -ErrorAction SilentlyContinue
        if ($null -eq $content) { continue }
        foreach ($pat in $stubPatterns) {
            $hits = [regex]::Matches($content, $pat).Count
            if ($hits -gt 0) {
                $stubCount += $hits
                $rel = $file.FullName.Substring($root.Length + 1)
                if (-not $stubByFile.ContainsKey($rel)) { $stubByFile[$rel] = 0 }
                $stubByFile[$rel] += $hits
            }
        }
    }
}
Write-Host "    stubs: $stubCount across $($stubByFile.Count) file(s)"
foreach ($k in $stubByFile.Keys) { Write-Host "      $k : $($stubByFile[$k])" }

# --- [4/7] stage witnesses -----------------------------------------------
Write-Host "==> [4/7] stage witnesses"
$witnessByStage = @{}
$witnessTotal = 0
if (Test-Path "tests/witnesses") {
    $witnessFiles = Get-ChildItem -Path tests/witnesses -Recurse -Filter "*.rs" -ErrorAction SilentlyContinue
    foreach ($file in $witnessFiles) {
        $content = Get-Content -Path $file.FullName -ErrorAction SilentlyContinue
        if ($null -eq $content) { continue }
        $head = $content | Select-Object -First 10
        foreach ($line in $head) {
            $m = [regex]::Match($line, '//\s*stage:\s*(\d+)')
            if ($m.Success) {
                $stage = [int]$m.Groups[1].Value
                if (-not $witnessByStage.ContainsKey($stage)) { $witnessByStage[$stage] = 0 }
                $witnessByStage[$stage] += 1
                $witnessTotal += 1
            }
        }
    }
}
Write-Host "    witnesses: $witnessTotal across stages: $(($witnessByStage.Keys | Sort-Object) -join ',')"

# --- [5/7] bench freshness -----------------------------------------------
Write-Host "==> [5/7] bench freshness"
$benchStale = $false
if (Test-Path "docs/BENCH_LEDGER.md") {
    $benchLines = Get-Content "docs/BENCH_LEDGER.md" -ErrorAction SilentlyContinue
    $dataRows = $benchLines | Where-Object { $_ -match '^\| 20\d{2}-' }
    if ($dataRows.Count -eq 0) {
        Write-Host "    no bench rows yet (ok during scaffold)"
    } else {
        $lastRow = $dataRows[-1]
        $lastDateMatch = [regex]::Match($lastRow, '\| (20\d{2}-\d{2}-\d{2}) ')
        if ($lastDateMatch.Success) {
            $lastDate = [datetime]::ParseExact($lastDateMatch.Groups[1].Value, "yyyy-MM-dd", $null)
            $age = ([datetime]::Now - $lastDate).Days
            Write-Host "    last bench row: $($lastDateMatch.Groups[1].Value) ($age days old)"
            if ($age -gt 7 -and $witnessTotal -ge 7) {
                $benchStale = $true
                $failures += "bench-stale:$($age)d"
            }
        }
    }
} else {
    Write-Host "    BENCH_LEDGER.md missing (ok before first bench run)"
}

# --- [6/7] schema drift --------------------------------------------------
Write-Host "==> [6/7] schema drift (HANDOFF.md section 7 vs 20260510000000_init.sql)"
$driftFindings = @()
if ((Test-Path "HANDOFF.md") -and (Test-Path "20260510000000_init.sql")) {
    $sqlContent = Get-Content "20260510000000_init.sql" -Raw
    # Pull column names from the tracks table CREATE statement.
    $tracksMatch = [regex]::Match($sqlContent, 'CREATE TABLE IF NOT EXISTS tracks \((.*?)\);', 'Singleline')
    if ($tracksMatch.Success) {
        $sqlCols = [regex]::Matches($tracksMatch.Groups[1].Value, '^\s+(\w+)\s+', 'Multiline') |
            ForEach-Object { $_.Groups[1].Value } |
            Where-Object { $_ -notin @('CREATE','PRIMARY','FOREIGN','REFERENCES','DEFAULT') }
        # Pull column names from HANDOFF.md section 7's tracks block.
        $handoffContent = Get-Content "HANDOFF.md" -Raw
        $section7Match = [regex]::Match($handoffContent, 'CREATE TABLE tracks \((.*?)\);', 'Singleline')
        if ($section7Match.Success) {
            $handoffCols = [regex]::Matches($section7Match.Groups[1].Value, '^\s+(\w+)\s+', 'Multiline') |
                ForEach-Object { $_.Groups[1].Value } |
                Where-Object { $_ -notin @('CREATE','PRIMARY','FOREIGN','REFERENCES','DEFAULT') }
            $sqlOnly = $sqlCols | Where-Object { $handoffCols -notcontains $_ }
            $handoffOnly = $handoffCols | Where-Object { $sqlCols -notcontains $_ }
            if ($sqlOnly.Count -gt 0) { $driftFindings += "sql-only: $($sqlOnly -join ',')" }
            if ($handoffOnly.Count -gt 0) { $driftFindings += "handoff-only: $($handoffOnly -join ',')" }
        }
    }
}
if ($driftFindings.Count -gt 0) {
    Write-Host "    DRIFT: $($driftFindings -join ' | ')"
    $failures += "drift"
} else {
    Write-Host "    no schema drift"
}

# --- [7/7] summary --------------------------------------------------------
Write-Host "==> [7/7] summary"
if ($failures.Count -eq 0) {
    Write-Host "OK - audit clean (build:$buildErrors test:$testFails stubs:$stubCount witnesses:$witnessTotal)"
    exit 0
} else {
    Write-Host "FAIL - $($failures -join ', ')"
    exit 1
}
