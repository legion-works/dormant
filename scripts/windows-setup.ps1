#Requires -Version 5.1
<#
.SYNOPSIS
    Build, install, and smoke-test dormant on Windows.

.DESCRIPTION
    There are no published Windows release artifacts (dist-workspace.toml targets
    Linux and macOS only), so this builds from source.

    Phases, each of which stops the script on failure rather than continuing into
    a test that cannot mean anything:

      1. Preflight   — toolchain present, host triple is MSVC
      2. Build       — dormantd + dormantctl, release profile
      3. Install     — into %LOCALAPPDATA%\Programs\dormant\bin, added to PATH
      4. Config      — minimal DDC/CI config written only if none exists
      5. Daemon      — started, then waited on until its named pipe answers
      6. Probes      — doctor config / ddcci / windows-idle, plus status
      7. Hardware    — guarded blank/wake, opt-in and consent-gated

    Idempotent: re-running with -SkipBuild re-tests an existing install without
    recompiling, and an existing config is never overwritten.

.PARAMETER RepoPath
    Path to a dormant checkout. Defaults to the repository this script lives in.

.PARAMETER SkipBuild
    Re-test an existing install without recompiling.

.PARAMETER SkipHardwareTest
    Run the read-only probes but never blank the display.

.PARAMETER WithWebUi
    Build the web dashboard too. Requires Node.js, because the SPA is compiled
    into the binary and the release build refuses to embed the placeholder.

.PARAMETER RegisterLogonTask
    Register a Scheduled Task that starts dormantd at logon. Task Scheduler
    rather than a Windows service on purpose: a service runs in session 0 with
    no interactive desktop, which breaks DDC/CI monitor enumeration.

.PARAMETER SafetyWakeSeconds
    How long the independent wake process waits before firing during the
    hardware test. It always fires; see the comment at Invoke-HardwareTest.

.EXAMPLE
    git clone git@github.com:legion-works/dormant.git
    cd dormant
    .\scripts\windows-setup.ps1

.EXAMPLE
    .\scripts\windows-setup.ps1 -SkipBuild -SkipHardwareTest
#>
[CmdletBinding()]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute(
    'PSAvoidUsingWriteHost', '',
    Justification = 'Interactive operator console script: the coloured status output IS the product, and it must not be capturable or redirectable as pipeline data.')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute(
    'PSReviewUnusedParameter', '',
    Justification = 'Script-scoped parameters are read inside the functions below; the analyzer does not follow script scope into function bodies.')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute(
    'PSUseShouldProcessForStateChangingFunctions', '',
    Justification = 'Start-Daemon starts a process the operator explicitly asked this script to start; a -WhatIf path would be a second, untested code path through the same logic.')]
param(
    [string]$RepoPath,
    [switch]$SkipBuild,
    [switch]$SkipHardwareTest,
    [switch]$WithWebUi,
    [switch]$RegisterLogonTask,
    [ValidateRange(5, 300)]
    [int]$SafetyWakeSeconds = 25
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ── Output helpers ──────────────────────────────────────────────────────────

$script:Results = [System.Collections.Generic.List[object]]::new()

function Write-Phase {
    param([string]$Text)
    Write-Host ''
    Write-Host "── $Text " -NoNewline -ForegroundColor Cyan
    Write-Host ('─' * [Math]::Max(0, 74 - $Text.Length)) -ForegroundColor DarkCyan
}

function Write-Step { param([string]$Text) Write-Host "  $Text" }

function Write-Pass {
    param([string]$Name, [string]$Detail = '')
    Write-Host '  ' -NoNewline
    Write-Host 'PASS' -NoNewline -ForegroundColor Green
    Write-Host "  $Name$(if ($Detail) { " — $Detail" })"
    $script:Results.Add([pscustomobject]@{ Status = 'PASS'; Name = $Name; Detail = $Detail })
}

function Write-Fail {
    param([string]$Name, [string]$Detail = '')
    Write-Host '  ' -NoNewline
    Write-Host 'FAIL' -NoNewline -ForegroundColor Red
    Write-Host "  $Name$(if ($Detail) { " — $Detail" })"
    $script:Results.Add([pscustomobject]@{ Status = 'FAIL'; Name = $Name; Detail = $Detail })
}

function Write-Warn {
    param([string]$Name, [string]$Detail = '')
    Write-Host '  ' -NoNewline
    Write-Host 'WARN' -NoNewline -ForegroundColor Yellow
    Write-Host "  $Name$(if ($Detail) { " — $Detail" })"
    $script:Results.Add([pscustomobject]@{ Status = 'WARN'; Name = $Name; Detail = $Detail })
}

function Show-BlockerAndExit {
    param([string]$Problem, [string[]]$Fix)
    Write-Host ''
    Write-Host "STOPPED: $Problem" -ForegroundColor Red
    if ($Fix) {
        Write-Host ''
        Write-Host '  To fix:' -ForegroundColor Yellow
        foreach ($line in $Fix) { Write-Host "    $line" }
    }
    Write-Host ''
    exit 1
}

# ── Phase 1 — preflight ─────────────────────────────────────────────────────

function Invoke-Preflight {
    Write-Phase 'Preflight'

    # Both, not just cargo: a half-installed or PATH-shadowed toolchain would
    # otherwise reach the `rustc -vV` call below and die with a raw PowerShell
    # "term is not recognized" error instead of this message.
    foreach ($tool in @('cargo', 'rustc')) {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
            Show-BlockerAndExit -Problem "$tool is not on PATH." -Fix @(
                'Install Rust from https://rustup.rs (the default host triple is correct),'
                'then open a NEW terminal so PATH is picked up.'
            )
        }
    }

    # The MSVC toolchain is what CI builds and what `ddc-winapi` links against.
    # A `-gnu` host will usually still compile, but it is not what any Windows
    # verification in this project has covered, so say so rather than imply it.
    $hostTriple = (& rustc -vV | Select-String -Pattern '^host:\s*(.+)$').Matches.Groups[1].Value.Trim()
    if ($hostTriple -notmatch 'windows-msvc$') {
        Write-Warn 'Host toolchain' "$hostTriple — CI verifies x86_64-pc-windows-msvc; results here are not comparable"
    }
    else {
        Write-Pass 'Host toolchain' $hostTriple
    }

    $rustcVersion = (& rustc --version)
    Write-Pass 'Rust' $rustcVersion

    # These drive every install and config path below. They are always set on a
    # normal Windows logon, but a stripped service context or a non-Windows host
    # leaves them empty, and the failure lands many lines later as an opaque
    # "Cannot bind argument to parameter 'Path' because it is null".
    foreach ($var in @('APPDATA', 'LOCALAPPDATA')) {
        if (-not [Environment]::GetEnvironmentVariable($var)) {
            Show-BlockerAndExit -Problem "%$var% is not set." -Fix @(
                'Run this from a normal interactive Windows session.'
                'This script does not support running as a service account or on non-Windows hosts.'
            )
        }
    }
    Write-Pass 'Environment' 'APPDATA and LOCALAPPDATA set'

    if ($WithWebUi -and -not (Get-Command npm -ErrorAction SilentlyContinue)) {
        Show-BlockerAndExit -Problem '-WithWebUi needs Node.js, and npm is not on PATH.' -Fix @(
            'Install Node.js from https://nodejs.org, or drop -WithWebUi.'
            'The dashboard is optional; the daemon and CLI do not need it.'
        )
    }
}

# ── Repo + paths ────────────────────────────────────────────────────────────

function Resolve-RepoPath {
    if ($RepoPath) {
        if (-not (Test-Path (Join-Path $RepoPath 'Cargo.toml'))) {
            Show-BlockerAndExit -Problem "No Cargo.toml under -RepoPath '$RepoPath'." -Fix @(
                'Point -RepoPath at a dormant checkout, or run this script from inside one.'
            )
        }
        return (Resolve-Path $RepoPath).Path
    }

    # This script lives in <repo>\scripts, so the repo is its parent.
    $candidate = Split-Path -Parent $PSScriptRoot
    if (Test-Path (Join-Path $candidate 'Cargo.toml')) { return $candidate }

    Show-BlockerAndExit -Problem 'Could not locate the dormant repository.' -Fix @(
        'git clone git@github.com:legion-works/dormant.git'
        'cd dormant'
        '.\scripts\windows-setup.ps1'
    )
}

# ── Phase 2 — build ─────────────────────────────────────────────────────────

function Invoke-Build {
    param([string]$Repo)

    Write-Phase 'Build'
    if ($SkipBuild) { Write-Step 'skipped (-SkipBuild)'; return }

    Push-Location $Repo
    try {
        if ($WithWebUi) {
            Write-Step 'npm ci + build (web dashboard)'
            Push-Location (Join-Path $Repo 'crates\dormant-web\webui')
            try {
                & npm ci --silent
                if ($LASTEXITCODE -ne 0) { Show-BlockerAndExit -Problem 'npm ci failed.' }
                & npm run build --silent
                if ($LASTEXITCODE -ne 0) { Show-BlockerAndExit -Problem 'npm run build failed.' }
            }
            finally { Pop-Location }
        }

        # `render` is deliberately absent: it is a Wayland layer-shell sink with
        # no Windows implementation. `--all-features` would pull it and fail.
        $cargoArgs = @('build', '--release', '-p', 'dormantd', '-p', 'dormantctl')
        if ($WithWebUi) { $cargoArgs += @('--features', 'web-ui') }

        Write-Step "cargo $($cargoArgs -join ' ')"
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) {
            Show-BlockerAndExit -Problem 'cargo build failed.' -Fix @(
                'If the error mentions a linker (link.exe), install the Visual Studio'
                'Build Tools with the "Desktop development with C++" workload:'
                '  https://visualstudio.microsoft.com/visual-cpp-build-tools/'
                'rustup normally offers this during install.'
            )
        }
        Write-Pass 'Build' 'dormantd + dormantctl (release)'
    }
    finally { Pop-Location }
}

# ── Phase 3 — install ───────────────────────────────────────────────────────

function Install-Binary {
    param([string]$Repo)

    Write-Phase 'Install'

    $binDir = Join-Path $env:LOCALAPPDATA 'Programs\dormant\bin'
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null

    # Ask cargo where it put them rather than assuming target\release — a
    # CARGO_TARGET_DIR or a workspace-level override moves it, and copying a
    # stale binary from a guessed path is a defect this project has already hit.
    $targetDir = $null
    try {
        $meta = & cargo metadata --format-version 1 --no-deps --manifest-path (Join-Path $Repo 'Cargo.toml') 2>$null | ConvertFrom-Json
        if ($meta -and $meta.target_directory) { $targetDir = $meta.target_directory }
    }
    catch { $targetDir = $null }
    if (-not $targetDir) { $targetDir = Join-Path $Repo 'target' }

    $releaseDir = Join-Path $targetDir 'release'
    $installed = @()
    foreach ($exe in @('dormantd.exe', 'dormantctl.exe')) {
        $src = Join-Path $releaseDir $exe
        if (-not (Test-Path $src)) {
            Show-BlockerAndExit -Problem "Built binary not found: $src" -Fix @(
                'Run without -SkipBuild, or check the build output above.'
            )
        }
        Copy-Item -Force $src (Join-Path $binDir $exe)
        $installed += $exe
    }
    Write-Pass 'Installed' "$($installed -join ', ') → $binDir"

    # Prepend for this session so the rest of the script uses the new build
    # regardless of what else is on PATH.
    $env:PATH = "$binDir;$env:PATH"

    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    if ($userPath -notlike "*$binDir*") {
        [Environment]::SetEnvironmentVariable('PATH', "$binDir;$userPath", 'User')
        Write-Pass 'PATH' 'added for your user (new terminals will see it)'
    }
    else {
        Write-Pass 'PATH' 'already present'
    }

    return $binDir
}

# ── Phase 4 — config ────────────────────────────────────────────────────────

function Initialize-Config {
    $configDir = Join-Path $env:APPDATA 'dormant'
    $configPath = Join-Path $configDir 'config.toml'

    Write-Phase 'Config'

    if (Test-Path $configPath) {
        Write-Pass 'Config' "$configPath (existing — not modified)"
        return $configPath
    }

    New-Item -ItemType Directory -Force -Path $configDir | Out-Null

    # Deliberately minimal: one manual-only DDC/CI display and nothing else.
    # A display no rule references responds only to `dormantctl blank/wake`,
    # which is exactly the hardware question being tested, and it means no
    # sensor, broker, or zone has to exist for this to validate.
    @'
# Generated by scripts\windows-setup.ps1 — a starting point, not a finished
# config. The display below is "manual-only": no rule drives it, so it responds
# only to `dormantctl blank monitor` / `dormantctl wake monitor`.
#
# Add sensors, zones, and rules once DDC/CI is confirmed working.
# Reference: https://github.com/legion-works/dormant/blob/dev/examples/config.toml

config_version = 1

[daemon]
log_level = "info"

[displays.monitor]
controllers = ["ddcci"]
blank_mode = "power_off"
'@ | Set-Content -Path $configPath -Encoding UTF8

    Write-Pass 'Config' "$configPath (created — minimal DDC/CI starting point)"
    return $configPath
}

# ── Phase 5 — daemon ────────────────────────────────────────────────────────

function Start-Daemon {
    Write-Phase 'Daemon'

    & dormantctl.exe validate
    if ($LASTEXITCODE -ne 0) {
        Show-BlockerAndExit -Problem 'Config did not validate.' -Fix @(
            "Edit $(Join-Path $env:APPDATA 'dormant\config.toml') and re-run with -SkipBuild."
        )
    }
    Write-Pass 'Config validates'

    # An already-running daemon answers status; that is the check, rather than
    # looking for a process by name, because only one can hold the pipe anyway.
    & dormantctl.exe status *> $null
    if ($LASTEXITCODE -eq 0) {
        Write-Pass 'Daemon' 'already running'
        return $null
    }

    Write-Step 'starting dormantd (hidden window)'
    $proc = Start-Process -FilePath 'dormantd.exe' -WindowStyle Hidden -PassThru

    # Poll for the pipe rather than sleeping a fixed amount: startup time varies
    # with how many displays have to be enumerated over DDC/CI.
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        & dormantctl.exe status *> $null
        if ($LASTEXITCODE -eq 0) {
            Write-Pass 'Daemon' "responding on the named pipe (pid $($proc.Id))"
            return $proc
        }
        if ($proc.HasExited) {
            Show-BlockerAndExit -Problem "dormantd exited immediately (code $($proc.ExitCode))." -Fix @(
                'Run it in the foreground to see why:'
                '  dormantd.exe'
            )
        }
    }

    Show-BlockerAndExit -Problem 'dormantd started but never answered on its pipe within 30s.' -Fix @(
        'Run it in the foreground to see what it is doing:'
        '  dormantd.exe'
    )
}

# ── Phase 6 — read-only probes ──────────────────────────────────────────────

function Invoke-Probe {
    Write-Phase 'Probes (read-only)'

    # DDC/CI first and on its own: it is the only local blanking path on Windows,
    # so if it cannot enumerate, every later result is about something else.
    Write-Step 'doctor ddcci'
    $ddc = & dormantctl.exe doctor ddcci 2>&1 | Out-String
    Write-Host ($ddc.TrimEnd() -split "`n" | ForEach-Object { "    $_" }) -Separator "`n"
    if ($LASTEXITCODE -eq 0) {
        Write-Pass 'DDC/CI' 'monitor enumerated'
    }
    else {
        Write-Fail 'DDC/CI' 'could not enumerate — this is THE finding; blanking cannot work without it'
    }

    Write-Step 'doctor windows-idle'
    $idle = & dormantctl.exe doctor windows-idle 2>&1 | Out-String
    Write-Host ($idle.TrimEnd() -split "`n" | ForEach-Object { "    $_" }) -Separator "`n"
    if ($LASTEXITCODE -eq 0) { Write-Pass 'Idle clock' 'advancing' }
    else { Write-Fail 'Idle clock' 'frozen or unreadable' }

    Write-Step 'doctor config'
    & dormantctl.exe doctor config *> $null
    if ($LASTEXITCODE -eq 0) { Write-Pass 'Doctor config' } else { Write-Warn 'Doctor config' 'reported problems' }

    Write-Step 'status'
    & dormantctl.exe status
}

# ── Phase 7 — hardware test ─────────────────────────────────────────────────

function Invoke-HardwareTest {
    Write-Phase 'Hardware test (blanks your display)'

    if ($SkipHardwareTest) { Write-Step 'skipped (-SkipHardwareTest)'; return }

    $display = 'monitor'
    Write-Host ''
    Write-Host '  This powers the panel off over DDC/CI and powers it back on.' -ForegroundColor Yellow
    Write-Host "  An INDEPENDENT process will wake it after $SafetyWakeSeconds seconds no matter" -ForegroundColor Yellow
    Write-Host '  what happens to this script — including if you close this window.' -ForegroundColor Yellow
    Write-Host ''
    Write-Host '  Play audio now if you want to check the audio-safety claim.' -ForegroundColor Yellow
    Write-Host ''
    $answer = Read-Host '  Type YES to run the blank/wake test'
    if ($answer -cne 'YES') { Write-Step 'skipped (not confirmed)'; return }

    # Armed BEFORE the blank and never cancelled. Waking an awake display is a
    # no-op, so a redundant wake costs nothing — whereas cancel-on-success logic
    # is itself a thing that can fail, and its failure mode is a dark screen.
    $ctl = (Get-Command dormantctl.exe).Source
    Start-Process -FilePath 'powershell.exe' -WindowStyle Hidden -ArgumentList @(
        '-NoProfile'
        '-NonInteractive'
        '-Command'
        "Start-Sleep -Seconds $SafetyWakeSeconds; & '$ctl' wake $display"
    ) | Out-Null
    Write-Step "safety wake armed (fires in ${SafetyWakeSeconds}s, independent of this script)"

    try {
        Write-Step "blank $display"
        & dormantctl.exe blank $display
        $blankOk = ($LASTEXITCODE -eq 0)

        Start-Sleep -Seconds 3
        Write-Step 'reading state back'
        & dormantctl.exe status

        Write-Step "wake $display"
        & dormantctl.exe wake $display
        $wakeOk = ($LASTEXITCODE -eq 0)

        if ($blankOk) { Write-Pass 'Blank' } else { Write-Fail 'Blank' 'command returned non-zero' }
        if ($wakeOk) { Write-Pass 'Wake' } else { Write-Fail 'Wake' 'command returned non-zero — the safety wake will still fire' }
    }
    finally {
        # Belt and braces: even on Ctrl-C or a mid-test throw, do not leave the
        # panel dark waiting on the timer.
        & dormantctl.exe wake $display *> $null
    }

    Write-Host ''
    Write-Host '  Did the panel actually go dark and come back?' -ForegroundColor Cyan
    Write-Host '  And if audio was playing — did it keep playing while dark?' -ForegroundColor Cyan
    Write-Host '  Those two answers are the point of this test; exit codes are not.' -ForegroundColor DarkGray
}

# ── Optional — logon task ───────────────────────────────────────────────────

function Register-LogonTask {
    param([string]$BinDir)

    Write-Phase 'Logon task'

    $exe = Join-Path $BinDir 'dormantd.exe'
    $taskName = 'dormant'

    $action = New-ScheduledTaskAction -Execute $exe
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
    # Interactive, NOT a service account: DDC/CI enumeration needs the desktop
    # session, which session 0 does not have.
    $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)

    Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
        -Principal $principal -Settings $settings -Force | Out-Null

    Write-Pass 'Scheduled task' "'$taskName' runs dormantd at logon (restart up to 3x)"
    Write-Step "Remove with: Unregister-ScheduledTask -TaskName $taskName -Confirm:`$false"
}

# ── Summary ─────────────────────────────────────────────────────────────────

function Write-Summary {
    Write-Phase 'Summary'

    foreach ($r in $script:Results) {
        $color = switch ($r.Status) { 'PASS' { 'Green' } 'FAIL' { 'Red' } default { 'Yellow' } }
        Write-Host '  ' -NoNewline
        Write-Host $r.Status.PadRight(5) -NoNewline -ForegroundColor $color
        Write-Host " $($r.Name)$(if ($r.Detail) { " — $($r.Detail)" })"
    }

    $failed = @($script:Results | Where-Object { $_.Status -eq 'FAIL' })
    Write-Host ''
    if ($failed.Count -gt 0) {
        Write-Host "$($failed.Count) check(s) failed." -ForegroundColor Red
        Write-Host 'Report at https://github.com/legion-works/dormant/issues/265 — include the'
        Write-Host 'output above and `dormantctl doctor ddcci`.'
        exit 1
    }

    Write-Host 'All checks passed.' -ForegroundColor Green
    Write-Host ''
    Write-Host 'What this did NOT prove — worth knowing before trusting it:' -ForegroundColor DarkGray
    Write-Host '  · that audio survived the blank (only your ears can say)' -ForegroundColor DarkGray
    Write-Host '  · idle detection against real typing, especially from an elevated app' -ForegroundColor DarkGray
    Write-Host '  · anything about sensors — this config has none' -ForegroundColor DarkGray
}

# ── Main ────────────────────────────────────────────────────────────────────

Write-Host ''
Write-Host 'dormant — Windows setup and smoke test' -ForegroundColor White
Write-Host 'Builds from source: there are no published Windows release artifacts.' -ForegroundColor DarkGray

Invoke-Preflight
$repo = Resolve-RepoPath
Write-Step "repo: $repo"
Invoke-Build -Repo $repo
$binDir = Install-Binary -Repo $repo
Initialize-Config | Out-Null
Start-Daemon | Out-Null
Invoke-Probe
Invoke-HardwareTest
if ($RegisterLogonTask) { Register-LogonTask -BinDir $binDir }
Write-Summary
