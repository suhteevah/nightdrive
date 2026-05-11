# bench/pipeline_one_track/run_all.ps1 -- nightdrive end-to-end pipeline
# bench. Times each stage of one full pipeline run on a fixed track_id and
# appends one row per stage to docs/BENCH_LEDGER.md.
#
# Until Phase N1 ships (see docs/ROADMAP.md), most stages are no-ops and
# the script just verifies its own contract: it runs without ps1 errors
# and writes a "scaffold" row to the ledger. Once a stage's crate ships,
# replace the corresponding placeholder timing block with the real call.
#
# Default config (matches the spec's reproducibility lock):
#   track_id = nd-bench-001
#   seed     = 1010
#   bpm      = 92
#   duration = 240s
#
# Wall-time budget: the full pipeline should complete in under 10 minutes
# on cnc post-P100. If it's slower, audit.ps1 [5/7] will flag it.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $root

$ledger = Join-Path $root "docs\BENCH_LEDGER.md"
if (-not (Test-Path $ledger)) {
    throw "BENCH_LEDGER.md missing at $ledger -- run Task 3 first"
}

$today = (Get-Date).ToString("yyyy-MM-dd")
$sha7 = "(head)"  # replace with `git rev-parse --short HEAD` once nightdrive is a git repo

$trackId = "nd-bench-001"
Write-Host "==> bench/pipeline_one_track on track_id=$trackId"

$stages = @(
    @{ id = 1; name = "llm_spec_gen"     ; crate = "nightdrive-llm"            }
    @{ id = 2; name = "audio_gen"        ; crate = "nightdrive-audio-gen"      }
    @{ id = 3; name = "cover_art"        ; crate = "nightdrive-art"            }
    @{ id = 4; name = "audio_master"     ; crate = "nightdrive-audio-master"   }
    @{ id = 5; name = "visualizer"       ; crate = "nightdrive-visuals"        }
    @{ id = 6; name = "final_encode"     ; crate = "nightdrive-encoder"        }
    @{ id = 7; name = "youtube_upload"   ; crate = "nightdrive-youtube"        }
)

foreach ($s in $stages) {
    $cratePath = Join-Path $root "crates\$($s.crate)\src\lib.rs"
    if (-not (Test-Path $cratePath)) {
        Write-Host "    [stage $($s.id) $($s.name)] crate not yet shipped (skip)"
        $row = "| $today | $sha7 | $($s.id) | $trackId | - | - | nightdrive | crate-not-shipped |"
        Add-Content -Path $ledger -Value $row -Encoding utf8
        continue
    }
    # Real timing logic lands when the crate's ship-task in ROADMAP.md is checked.
    Write-Host "    [stage $($s.id) $($s.name)] crate present; timing not yet implemented"
    $row = "| $today | $sha7 | $($s.id) | $trackId | - | - | nightdrive | timing-not-yet-implemented |"
    Add-Content -Path $ledger -Value $row -Encoding utf8
}

Write-Host "OK - bench complete (skeleton mode); rows appended to $ledger"
