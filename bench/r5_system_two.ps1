<#
.SYNOPSIS
  R5: does System Two (plan + rescue) cut the number of Jev calls per task without
  hurting the success rate? Runs the same goal N times with and without --two,
  alternating, and appends every RunSummary to bench/R5-system-two.jsonl.

.EXAMPLE
  # Notepad window handle from: Get-Process notepad | ? MainWindowHandle -ne 0
  .\bench\r5_system_two.ps1 -Goal 'Wpisz „hello r5” w edytorze tekstu' -Hwnd 23662268 -N 4 -Act

.NOTES
  Every run with --two is a paid OpenRouter call (unless the model is :free). The
  script prints the plan before the first paid run and needs -Confirm:$false to skip
  the prompt. Numbers in docs come only from the JSONL this script writes.
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [Parameter(Mandatory)] [string] $Goal,
    [Parameter(Mandatory)] [long] $Hwnd,
    [int] $N = 4,
    [switch] $Act,
    [string] $Model = $env:UC_TWO_MODEL,
    [string] $Exe = "$PSScriptRoot\..\target\release\ultracuse.exe",
    [string] $Out = "$PSScriptRoot\R5-system-two.jsonl"
)
$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Exe)) { throw "no exe at $Exe (cargo build --release)" }
$modelArgs = @()
if ($Model) { $modelArgs = @('--two-model', $Model) }
if (-not $PSCmdlet.ShouldProcess("$N runs with System Two ($(if ($Model) { $Model } else { 'default model' })) + $N without, goal '$Goal'", 'run the paid benchmark')) { return }

$rows = @()
for ($i = 0; $i -lt $N; $i++) {
    foreach ($two in @($false, $true)) {
        $args = @('run', $Goal, '--hwnd', $Hwnd, '--json', '--delay', '0')
        if ($Act) { $args += '--act' }
        if ($two) { $args += '--two'; $args += $modelArgs }
        $lines = & $Exe @args 2>&1 | Where-Object { $_ -is [string] -and $_.StartsWith('{') }
        $summary = $lines | Select-Object -Last 1
        if (-not $summary) { Write-Warning "run $i two=$two: no summary"; continue }
        $s = $summary | ConvertFrom-Json
        $row = [ordered]@{
            ts = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds(); i = $i; two = $two; goal = $Goal
            outcome = ($s.outcome | ConvertTo-Json -Compress); steps = $s.steps; jev_calls = $s.jev_calls
            cost_usd = $s.cost_usd; elapsed_ms = [math]::Round($s.elapsed_ms); two_model = $s.two_model
            two_calls = $s.two_calls; two_cost_usd = $s.two_cost_usd; plan = $s.plan; ledger = $s.ledger
        }
        $rows += [pscustomobject]$row
        ($row | ConvertTo-Json -Compress) | Add-Content -Path $Out -Encoding UTF8
        "{0,2} two={1,-5} {2,-28} steps {3,2} jev {4,2} {5,6} ms `${6:N5} two {7} `${8:N4}" -f $i, $two, $row.outcome, $s.steps, $s.jev_calls, $row.elapsed_ms, $s.cost_usd, $s.two_calls, $s.two_cost_usd
        Start-Sleep -Milliseconds 800
    }
}
"`n--- means ---"
foreach ($two in @($false, $true)) {
    $g = $rows | Where-Object two -eq $two
    if (-not $g) { continue }
    $done = ($g | Where-Object { $_.outcome -like '*Done*' }).Count
    "two={0,-5} n={1} done {2}/{1}  jev calls {3:N2}  steps {4:N2}  jev `${5:N5}  two `${6:N4}  {7:N0} ms" -f $two, $g.Count, $done,
        ($g | Measure-Object jev_calls -Average).Average, ($g | Measure-Object steps -Average).Average,
        ($g | Measure-Object cost_usd -Average).Average, ($g | Measure-Object two_cost_usd -Average).Average,
        ($g | Measure-Object elapsed_ms -Average).Average
}
