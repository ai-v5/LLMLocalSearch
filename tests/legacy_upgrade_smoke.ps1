$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'Legacy installer test requires an isolated GitHub-hosted runner.'
}
$root = Split-Path -Parent $PSScriptRoot
$legacy = Join-Path $root '.cache\legacy-source\dist\LLMLocalSearch-0.1.0-setup.exe'
$current = Join-Path $root 'dist\LLMLocalSearch-0.1.5-setup.exe'
$work = Join-Path $env:RUNNER_TEMP 'llmlocalsearch-legacy-upgrade'
$appKey = 'HKCU:\Software\LLMLocalSearch'
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\LLMLocalSearch'
if ((Test-Path -LiteralPath $work) -or $work -match '\s' -or
    (Test-Path -LiteralPath $appKey) -or (Test-Path -LiteralPath $uninstallKey) -or
    -not (Test-Path -LiteralPath $legacy -PathType Leaf) -or
    -not (Test-Path -LiteralPath $current -PathType Leaf)) {
    throw 'Legacy upgrade preconditions failed; no changes were made.'
}
New-Item -ItemType Directory -Path $work,(Join-Path $work 'local'),(Join-Path $work 'temp') | Out-Null
$env:LOCALAPPDATA = Join-Path $work 'local'
$env:TEMP = Join-Path $work 'temp'
$env:TMP = $env:TEMP

function Invoke-Setup([string]$Executable, [string[]]$Arguments, [int]$Expected = 0) {
    $process = Start-Process -FilePath $Executable -ArgumentList $Arguments -WindowStyle Hidden -Wait -PassThru
    if ($process.ExitCode -ne $Expected) { throw "Installer returned $($process.ExitCode); expected $Expected." }
}

function Wait-Uninstalled([string]$Program) {
    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (-not (Test-Path -LiteralPath $appKey) -and
            -not (Test-Path -LiteralPath (Join-Path $Program 'llmlocalsearch.exe'))) { return }
        Start-Sleep -Milliseconds 200
    }
    throw 'Synthetic uninstaller did not finish.'
}

function Invoke-Worker([string]$Scenario, [string]$Mode) {
    $program = Join-Path $Scenario 'program'
    python (Join-Path $PSScriptRoot 'synthetic_upgrade_worker.py') (Join-Path $program 'index-worker.exe') $Scenario $Mode | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -ne 0) { throw 'Synthetic legacy worker or migration failed.' }
}

function New-Scenario([string]$Name) {
    $scenario = Join-Path $work $Name
    $program = Join-Path $scenario 'program'
    New-Item -ItemType Directory -Path $scenario | Out-Null
    Invoke-Setup $legacy @('/S', "/D=$program")
    if ((Get-ItemPropertyValue -LiteralPath $appKey -Name InstallDir) -ne $program -or
        (Get-ItemPropertyValue -LiteralPath $uninstallKey -Name DisplayVersion) -ne '0.1.0') {
        throw 'The actual legacy installer did not register the synthetic installation.'
    }
    Invoke-Worker $scenario 'create'
    $index = Join-Path $program 'Index'
    $settingsDir = Join-Path $env:LOCALAPPDATA 'LLMLocalSearch'
    New-Item -ItemType Directory -Force -Path $settingsDir | Out-Null
    $settings = @{ index_dir = $index; exclusions = @(); extra_roots = @(); close_hint_seen = $false; autostart = $false; old_index_dirs = @() } | ConvertTo-Json
    [IO.File]::WriteAllText((Join-Path $settingsDir 'settings.json'), $settings, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $index 'user-owned.txt'), 'synthetic preserve')
    [IO.File]::WriteAllText((Join-Path $program 'unrelated.txt'), 'synthetic preserve')
    return $scenario
}

function Get-Preserved([string]$Scenario) {
    $program = Join-Path $Scenario 'program'
    return @(
        (Join-Path $program 'Index\index.db'),
        (Join-Path $program 'Index\user-owned.txt'),
        (Join-Path $program 'unrelated.txt'),
        (Join-Path $env:LOCALAPPDATA 'LLMLocalSearch\settings.json')
    )
}

function Get-Hashes([string[]]$Files) {
    $hashes = @{}
    foreach ($file in $Files) { $hashes[$file] = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash }
    return $hashes
}

function Assert-Hashes([hashtable]$Expected) {
    foreach ($file in $Expected.Keys) {
        if (-not (Test-Path -LiteralPath $file -PathType Leaf) -or
            (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash -ne $Expected[$file]) {
            throw 'Synthetic index, settings, or unrelated data changed unexpectedly.'
        }
    }
}

# Registered 0.1.0: direct upgrade preserves the old DB before its real migration.
$scenario = New-Scenario 'registered'
$program = Join-Path $scenario 'program'
$before = Get-Hashes (Get-Preserved $scenario)
Invoke-Setup $current @('/S', "/D=$program")
Assert-Hashes $before
if ((Get-ItemPropertyValue -LiteralPath $uninstallKey -Name DisplayVersion) -ne '0.1.5') {
    throw 'Registered upgrade did not install the new version.'
}
Invoke-Worker $scenario 'migrate'
$after = Get-Hashes (Get-Preserved $scenario)
Invoke-Setup (Join-Path $program 'uninstall.exe') @('/S')
Wait-Uninstalled $program
Assert-Hashes $after
Write-Output 'Registered 0.1.0 upgrade, searchable migration, and default uninstall: PASS'

# Reproduce a missing registry entry while the old program and shortcuts remain.
$scenario = New-Scenario 'missing-registration'
$program = Join-Path $scenario 'program'
$before = Get-Hashes (Get-Preserved $scenario)
if ((Get-ItemPropertyValue -LiteralPath $appKey -Name InstallDir) -ne $program -or
    (Get-ItemPropertyValue -LiteralPath $uninstallKey -Name InstallLocation) -ne $program) {
    throw 'Synthetic registry identity changed; refusing to remove it.'
}
Remove-Item -LiteralPath $appKey -Force
Remove-Item -LiteralPath $uninstallKey -Force
Invoke-Setup $current @('/S', "/D=$program") 1
Assert-Hashes $before
Invoke-Setup (Join-Path $program 'uninstall.exe') @('/S')
Wait-Uninstalled $program
Assert-Hashes $before
Invoke-Setup $current @('/S', "/D=$program")
Assert-Hashes $before
Invoke-Worker $scenario 'migrate'
$after = Get-Hashes (Get-Preserved $scenario)
Invoke-Setup (Join-Path $program 'uninstall.exe') @('/S')
Wait-Uninstalled $program
Assert-Hashes $after
Write-Output 'Missing-registration guard, old uninstall, reinstall, and searchable migration: PASS'
