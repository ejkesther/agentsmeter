<#
Sets up Clawdmeter on Windows in one go:

  1. the app, from this project's latest GitHub release (checked against
     its SHA256SUMS), or a local build with -AppExe;
  2. the agents it tracks, each optional: Claude Code, Codex and Cursor,
     installed with winget and signed in;
  3. their hooks (clawdmeter --install-hooks) and Start at login;
  4. a usage check, then the app, which flashes and pairs the board.

Run it again any time: each step skips what's already done. Nothing needs
admin rights. Sign-ins happen in your browser; this script never sees them.

  From a clone:  setup.cmd
  Without one:   powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/ejkesther/agentsmeter/main/tools/setup/windows.ps1 | iex"

Options (with -File): -Repo owner/name, -AppExe path\to\clawdmeter.exe, -Yes.
Plain ASCII on purpose: Windows PowerShell 5.1 reads BOM-less scripts as ANSI.
#>
param(
    # GitHub repository whose latest release holds the app.
    [string]$Repo = 'ejkesther/agentsmeter',
    # Use this clawdmeter.exe (a local build) instead of the release installer.
    [string]$AppExe = '',
    # Answer yes to every question.
    [switch]$Yes
)

$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 may default to TLS 1.0; GitHub needs 1.2.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
# The progress bar slows Invoke-WebRequest down a lot in 5.1.
$ProgressPreference = 'SilentlyContinue'

$StateDir = Join-Path $env:LOCALAPPDATA 'Clawdmeter'
$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\Clawdmeter'
$CursorExe = Join-Path $env:LOCALAPPDATA 'Programs\cursor\Cursor.exe'

function Step([string]$text) {
    Write-Host ''
    Write-Host "== $text" -ForegroundColor Cyan
}

function Say([string]$text) { Write-Host "   $text" }

function Warn([string]$text) { Write-Host "   $text" -ForegroundColor Yellow }

function Ask([string]$question, [bool]$default = $true) {
    if ($Yes) { return $true }
    $hint = if ($default) { '[Y/n]' } else { '[y/N]' }
    $answer = Read-Host "   $question $hint"
    if ([string]::IsNullOrWhiteSpace($answer)) { return $default }
    return $answer.Trim().ToLower().StartsWith('y')
}

function Have([string]$command) {
    return [bool](Get-Command $command -ErrorAction SilentlyContinue)
}

# winget installs land on PATH for new shells only.
function Update-Path {
    $env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
        [Environment]::GetEnvironmentVariable('Path', 'User')
}

function Install-Package([string]$id, [string]$name) {
    if (-not (Have 'winget')) {
        throw "winget isn't available. Install 'App Installer' from the Microsoft Store, then run setup again."
    }
    Say "Installing $name with winget..."
    # -Yes takes winget's agreements as read too; otherwise winget asks.
    $accept = if ($Yes) { @('--accept-package-agreements', '--accept-source-agreements') } else { @() }
    winget install --id $id -e @accept
    # -1978335189: already installed with no newer version.
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne -1978335189) {
        throw "winget couldn't install $name (exit code $LASTEXITCODE)."
    }
    Update-Path
}

# The app is a windowed program: PowerShell waits for it, and gets its
# output and exit code, only when its output is piped.
function Invoke-App {
    & $script:App @args | ForEach-Object { "$_" }
}

function Get-AppVersion([string]$exe) {
    if (-not (Test-Path $exe)) { return $null }
    $out = (& $exe --version | Out-String).Trim()
    if ($out -match '(\d+\.\d+\.\d+)') { return $Matches[1] }
    return $out
}

function Install-App {
    Step 'The Clawdmeter app'
    if ($AppExe) {
        $script:App = (Resolve-Path $AppExe).Path
        Say "Using $script:App"
        return
    }
    $headers = @{ 'User-Agent' = 'clawdmeter-setup'; 'Accept' = 'application/vnd.github+json' }
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
    } catch {
        throw "Couldn't find a release of $Repo on GitHub ($($_.Exception.Message)). For a local build: -AppExe path\to\clawdmeter.exe"
    }
    $version = $release.tag_name.TrimStart('v')
    $script:App = Join-Path $InstallDir 'clawdmeter.exe'
    $have = Get-AppVersion $script:App
    if ($have -eq $version) {
        Say "Clawdmeter $version is installed."
        return
    }
    $msi = $release.assets | Where-Object { $_.name -like '*-windows-x86_64.msi' } | Select-Object -First 1
    $sums = $release.assets | Where-Object { $_.name -eq 'SHA256SUMS' } | Select-Object -First 1
    if (-not $msi -or -not $sums) { throw "Release $($release.tag_name) has no Windows installer or SHA256SUMS." }
    # ${} because PowerShell reads "$version?" as a variable named "version?".
    $what = if ($have) { "Update Clawdmeter $have to ${version}?" } else { "Install Clawdmeter ${version}?" }
    if (-not (Ask $what)) { throw 'Setup needs the app.' }

    $dir = Join-Path $env:TEMP 'clawdmeter-setup'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $file = Join-Path $dir $msi.name
    Say "Downloading $($msi.name)..."
    Invoke-WebRequest -Uri $msi.browser_download_url -OutFile $file -UseBasicParsing
    $sumsText = (Invoke-WebRequest -Uri $sums.browser_download_url -UseBasicParsing).Content
    if ($sumsText -is [byte[]]) { $sumsText = [Text.Encoding]::ASCII.GetString($sumsText) }
    $line = ($sumsText -split "`n") | Where-Object { $_ -match ([regex]::Escape($msi.name) + '\s*$') } | Select-Object -First 1
    $want = if ($line) { ($line -split '\s+')[0].ToLower() } else { '' }
    $got = (Get-FileHash -Algorithm SHA256 -Path $file).Hash.ToLower()
    if (-not $want -or $want -ne $got) { throw "The download doesn't match the release's SHA256SUMS; not installing it." }
    Say 'Checksum OK.'

    # The installer replaces clawdmeter.exe, so the running app has to stop.
    $running = Get-Process -Name 'clawdmeter' -ErrorAction SilentlyContinue
    if ($running) {
        Say 'Closing the running Clawdmeter...'
        $running | Stop-Process -Force
        Start-Sleep -Seconds 2
    }
    Say 'Installing (per user, no admin)...'
    $p = Start-Process -FilePath 'msiexec.exe' -ArgumentList @('/i', "`"$file`"", '/passive', '/norestart') -Wait -PassThru
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { throw "The installer stopped with code $($p.ExitCode)." }
    Say "Installed Clawdmeter $version in $InstallDir."
    $old = Join-Path $env:LOCALAPPDATA 'Programs\Agentsmeter'
    if (Test-Path (Join-Path $old 'clawdmeter.exe')) {
        Say "An older hand-copied app is in $old. The hooks move to the new one below; you can delete that folder."
    }
}

# Windows PowerShell 5.1 turns a redirected stderr line into an error, which
# 'Stop' would throw: the sign-in checks relax that locally.
function Test-ClaudeSignIn {
    if (-not (Have 'claude')) { return $false }
    $ErrorActionPreference = 'Continue'
    try { return [bool]((claude auth status 2>$null | Out-String | ConvertFrom-Json).loggedIn) } catch { return $false }
}

function Setup-Claude {
    Step 'Claude Code'
    if (-not (Have 'claude')) {
        Say 'Clawdmeter reads Claude usage with Claude Code''s own sign-in. The Claude desktop app keeps its'
        Say 'sign-in to itself, so Claude Code is needed on its own even if you use the desktop app.'
        if (-not (Ask 'Install Claude Code?')) { Say 'Skipped.'; return }
        Install-Package 'Anthropic.ClaudeCode' 'Claude Code'
        if (-not (Have 'claude')) { Warn 'Claude Code installed, but this window can''t find it yet. Run setup again in a new window.'; return }
    }
    if (Test-ClaudeSignIn) { Say 'Claude Code is signed in.'; return }
    if (-not (Ask 'Sign in to Claude Code now? Your browser opens; use the account you want on the device.')) { Say 'Skipped.'; return }
    claude auth login --claudeai
    if (Test-ClaudeSignIn) { Say 'Signed in.' } else { Warn 'Not signed in yet. Run "claude auth login" later.' }
}

function Test-CodexSignIn {
    if (-not (Have 'codex')) { return $false }
    $ErrorActionPreference = 'Continue'
    $out = codex login status 2>&1 | Out-String
    return ($LASTEXITCODE -eq 0 -and $out -match 'Logged in')
}

function Setup-Codex {
    Step 'Codex'
    if (-not (Have 'codex')) {
        if (-not (Ask 'Install Codex (OpenAI''s coding agent)?' $false)) { Say 'Skipped.'; return }
        Install-Package 'OpenAI.Codex' 'Codex'
        if (-not (Have 'codex')) { Warn 'Codex installed, but this window can''t find it yet. Run setup again in a new window.'; return }
    }
    if (Test-CodexSignIn) { Say 'Codex is signed in.'; return }
    Say 'Sign in with ChatGPT: an API key reports no usage limits.'
    if (-not (Ask 'Sign in to Codex now? Your browser opens.')) { Say 'Skipped.'; return }
    codex login
    if (Test-CodexSignIn) { Say 'Signed in.' } else { Warn 'Not signed in yet. Run "codex login" later.' }
}

function Open-CursorSignIn {
    Start-Process -FilePath $CursorExe
    if (-not $Yes) { Read-Host '   Sign in inside Cursor, then press Enter here' | Out-Null }
}

# Cursor signs in only inside the editor. A new install opens it here; an
# existing one is checked by the usage check below.
function Setup-Cursor {
    Step 'Cursor'
    if (Test-Path $CursorExe) { Say 'Cursor is installed.'; return }
    if (-not (Ask 'Install Cursor (the AI code editor)?' $false)) { Say 'Skipped.'; return }
    Install-Package 'Anysphere.Cursor' 'Cursor'
    if (-not (Test-Path $CursorExe)) { Warn "Cursor isn't where it installs by default ($CursorExe)."; return }
    if (Ask 'Open Cursor to sign in?') { Open-CursorSignIn }
}

function Install-Hooks {
    Step 'Hooks (Clawd reacts to each agent)'
    Invoke-App --install-hooks | ForEach-Object { Say $_ }
    if ($LASTEXITCODE -ne 0) { Warn 'Some hooks weren''t installed: open the app''s Hooks page to see why.' }
}

function Set-StartAtLogin {
    Step 'Start at login'
    if (Ask 'Start Clawdmeter when you sign in to Windows?') {
        Invoke-App --start-at-login on | ForEach-Object { Say $_ }
    } else {
        Say 'Skipped.'
    }
}

# One read of every agent, as the app does it (`--once`: no Bluetooth).
function Show-Usage {
    Step 'Usage check'
    $lines = Invoke-App --once | Where-Object { $_ -notmatch '^\{' }
    foreach ($l in $lines) {
        $l = $l -replace [regex]::Escape($env:USERPROFILE), '~'
        $l = $l -replace '~\\AppData\\Roaming\\Cursor\\User\\globalStorage\\state\.vscdb', 'Cursor'
        $l = $l -replace '~\\\.codex', 'Codex' -replace '~\\\.claude', 'Claude Code'
        # Cursor's two allowances, as its cards on the device name them.
        if ($l -like 'Cursor:*') { $l = $l -replace 'session', 'Auto' -replace 'week', 'API' }
        Say $l
    }
    if ($lines -match 'no rate limits') { Say 'Codex shows usage after its first reply on this computer.' }
    if ($lines -match "Cursor editor isn't signed in" -and (Ask 'Open Cursor to sign in? The app picks it up within 15 seconds.')) {
        Open-CursorSignIn
    }
}

function Start-App {
    Step 'Your board'
    if (-not (Get-Process -Name 'clawdmeter' -ErrorAction SilentlyContinue)) {
        Start-Process -FilePath $script:App
        Say 'Clawdmeter is running (look for Clawd in the tray).'
    }
    if (-not (Test-Path (Join-Path $StateDir 'ble-address'))) {
        Say 'First run: the app opens its setup to flash the board over USB and pair it over Bluetooth.'
    } elseif (Ask 'Open the app''s Flash page to install or update the board''s firmware?' $false) {
        Start-Process -FilePath $script:App -ArgumentList @('--window', 'flash')
    }
}

function Invoke-Setup {
    Write-Host 'Clawdmeter setup for Windows' -ForegroundColor Cyan
    Install-App
    Setup-Claude
    Setup-Codex
    Setup-Cursor
    Install-Hooks
    Set-StartAtLogin
    Show-Usage
    Start-App
    Write-Host ''
    Write-Host 'Done. Tap the top-left corner of the board to switch between agents.' -ForegroundColor Green
    if (Have 'codex') { Say 'Codex runs new hooks once you trust them: type /hooks in Codex.' }
}

Invoke-Setup
