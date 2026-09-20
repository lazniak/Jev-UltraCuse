param(
    [string]$Exe = "$PSScriptRoot\..\target\release\ultracuse.exe",
    [int]$MinIdle = 45,
    [int]$Steps = 4,
    [string]$Goal = "Stwórz na pulpicie nowy plik tekstowy o nazwie r6test.txt"
)
# R6 (ADR-004): a goal started from the WRONG window (a Notepad window on top), preview mode —
# nothing is injected, Switch/ShowDesktop are shown but not performed. Idle-gated: refuses to run
# while the user is active (GetLastInputInfo), because the run brings Notepad to the front.
# Expected ladder in the output: step 1 uncertain or `target none` -> widening -> survey ->
# `switch to ...` / `show desktop` as the preview action. Ledger lands in runs/.
Add-Type @"
using System; using System.Runtime.InteropServices;
public class R6Win {
  [StructLayout(LayoutKind.Sequential)] public struct LII { public uint cbSize; public uint dwTime; }
  [DllImport("user32.dll")] public static extern bool GetLastInputInfo(ref LII l);
  public static uint Idle() { var l = new LII(); l.cbSize = 8; GetLastInputInfo(ref l); return ((uint)Environment.TickCount - l.dwTime) / 1000; }
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
}
"@
$idle = [R6Win]::Idle(); "idle_s=$idle"; if ($idle -lt $MinIdle) { "user active - abort"; exit 3 }
if (Get-Process ultracuse -ErrorAction SilentlyContinue) { "ultracuse already running - abort"; exit 4 }
$ws = New-Object -ComObject WScript.Shell
$prev = [R6Win]::GetForegroundWindow(); $prevPid = 0; [void][R6Win]::GetWindowThreadProcessId($prev, [ref]$prevPid)

$np = Get-Process notepad -ErrorAction SilentlyContinue | Where-Object MainWindowHandle -ne 0 | Select-Object -First 1
$started = $false
if (-not $np) {
    $np = Start-Process notepad -PassThru; $started = $true
    for ($i = 0; $i -lt 30 -and $np.MainWindowHandle -eq 0; $i++) { Start-Sleep -Milliseconds 200; $np.Refresh() }
    $np = Get-Process notepad -ErrorAction SilentlyContinue | Where-Object MainWindowHandle -ne 0 | Select-Object -First 1
}
if (-not $np) { "no notepad window"; exit 5 }
$h = $np.MainWindowHandle
[void][R6Win]::ShowWindow($h, 9); Start-Sleep -Milliseconds 300
[void][R6Win]::SetForegroundWindow($h); [void]$ws.AppActivate($np.Id); Start-Sleep -Milliseconds 400
"notepad hwnd=$h fg=" + ([R6Win]::GetForegroundWindow() -eq $h)

"--- preview run from Notepad: $Goal ($Steps steps) ---"
& $Exe run $Goal --hwnd ([long]$h) --delay 0 --max-steps $Steps 2>&1 | ForEach-Object { "$_" }
"exit=$LASTEXITCODE"

if ($started) { Stop-Process -Id $np.Id -Force -ErrorAction SilentlyContinue } else { [void][R6Win]::ShowWindow($h, 6) }
if ($prevPid -ne 0) { [void]$ws.AppActivate([int]$prevPid) }
"restored pid $prevPid"
