$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'Installer smoke test requires an isolated GitHub-hosted runner.'
}
$root = Split-Path -Parent $PSScriptRoot
$installer = Join-Path $root 'dist\LLMLocalSearch-0.1.6-setup.exe'
$work = Join-Path $env:RUNNER_TEMP 'llmlocalsearch-installer-smoke'
$program = Join-Path $work 'program'
$collision = Join-Path $work 'collision'
$local = Join-Path $work 'local'
$scratch = Join-Path $work 'temp'
if (-not (Test-Path -LiteralPath $installer -PathType Leaf) -or
    (Test-Path -LiteralPath $work) -or $work -match '\s' -or
    (Test-Path -LiteralPath 'HKCU:\Software\LLMLocalSearch')) {
    throw 'Installer test preconditions were not met; no changes were made.'
}
New-Item -ItemType Directory -Path $collision,$local,$scratch | Out-Null
$env:LOCALAPPDATA = $local
$env:TEMP = $scratch
$env:TMP = $scratch

function Invoke-Setup([string]$Executable, [string[]]$Arguments, [int]$Expected = 0) {
    $process = Start-Process -FilePath $Executable -ArgumentList $Arguments -WindowStyle Hidden -Wait -PassThru
    if ($process.ExitCode -ne $Expected) {
        throw "Installer returned $($process.ExitCode); expected $Expected."
    }
}

function Wait-Uninstalled {
    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (-not (Test-Path -LiteralPath 'HKCU:\Software\LLMLocalSearch') -and
            -not (Test-Path -LiteralPath (Join-Path $program 'llmlocalsearch.exe'))) { return }
        Start-Sleep -Milliseconds 200
    }
    throw 'Uninstaller did not finish.'
}

$sentinel = Join-Path $collision 'README.md'
[IO.File]::WriteAllText($sentinel, 'synthetic sentinel')
Invoke-Setup $installer @('/S', "/D=$collision") 1
if ([IO.File]::ReadAllText($sentinel) -ne 'synthetic sentinel' -or
    (Test-Path -LiteralPath 'HKCU:\Software\LLMLocalSearch')) { throw 'Collision protection failed.' }
Write-Output 'Existing-file collision protection: PASS'

Invoke-Setup $installer @('/S', "/D=$program")
if (-not (Test-Path -LiteralPath (Join-Path $program 'llmlocalsearch.exe')) -or
    -not (Test-Path -LiteralPath (Join-Path $program 'runtime\tika-app.jar')) -or
    (Get-ItemPropertyValue -LiteralPath 'HKCU:\Software\LLMLocalSearch' -Name InstallDir) -ne $program) {
    throw 'Custom-directory installation failed.'
}
Write-Output 'Custom-directory installation: PASS'

$index = Join-Path $program 'Index'
New-Item -ItemType Directory -Path $index,(Join-Path $local 'LLMLocalSearch') | Out-Null
$database = Join-Path $index 'index.db'
python -c "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute('PRAGMA application_id=1280070488'); c.execute('CREATE TABLE files(id INTEGER)'); c.commit(); c.close()" $database
if ($LASTEXITCODE -ne 0) { throw 'Synthetic database creation failed.' }
[IO.File]::WriteAllText((Join-Path $index 'user-owned.txt'), 'preserve')
[IO.File]::WriteAllText((Join-Path $program 'unrelated.txt'), 'preserve')
$settings = @{ index_dir = $index; exclusions = @(); extra_roots = @(); close_hint_seen = $false; autostart = $false; old_index_dirs = @() } | ConvertTo-Json
$settingsFile = Join-Path $local 'LLMLocalSearch\settings.json'
[IO.File]::WriteAllText($settingsFile, $settings, [Text.UTF8Encoding]::new($false))
$preserved = @($database, $settingsFile, (Join-Path $index 'user-owned.txt'), (Join-Path $program 'unrelated.txt'))
$original = @{}
foreach ($file in $preserved) { $original[$file] = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash }
function Assert-Preserved {
    foreach ($file in $preserved) {
        if (-not (Test-Path -LiteralPath $file -PathType Leaf) -or
            (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash -ne $original[$file]) {
            throw 'Upgrade or default uninstall changed synthetic user data.'
        }
    }
}
Invoke-Setup $installer @('/S', "/D=$program")
Assert-Preserved
Write-Output 'Upgrade preserves index and unrelated files: PASS'

Invoke-Setup (Join-Path $program 'uninstall.exe') @('/S')
Wait-Uninstalled
Assert-Preserved
Write-Output 'Default uninstall preserves data: PASS'

Invoke-Setup $installer @('/S', "/D=$program")
Invoke-Setup (Join-Path $program 'uninstall.exe') @('/S', '/REMOVEINDEX')
Wait-Uninstalled
if ((Test-Path -LiteralPath $database) -or
    -not (Test-Path -LiteralPath (Join-Path $index 'user-owned.txt')) -or
    -not (Test-Path -LiteralPath $settingsFile) -or
    -not (Test-Path -LiteralPath (Join-Path $program 'unrelated.txt'))) { throw 'Explicit uninstall deleted the wrong files.' }
foreach ($file in $preserved | Where-Object { $_ -ne $database }) {
    if ((Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash -ne $original[$file]) {
        throw 'Explicit uninstall changed unrelated synthetic data.'
    }
}
Write-Output 'Explicit uninstall removes only owned index: PASS'
