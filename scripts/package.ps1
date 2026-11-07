param(
    [Parameter(Mandatory=$true)][string]$TikaJar,
    [Parameter(Mandatory=$true)][string]$JavaZip,
    [Parameter(Mandatory=$true)][string]$MakeNsis,
    [string]$OutputFile = 'dist\LLMLocalSearch-0.1.5-setup.exe'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root
if ((Get-FileHash -LiteralPath $TikaJar -Algorithm SHA512).Hash -ne '3C6E62E864E32AF234832EC6D928BBC58692D3DDD60564CD8C80DEE7E161653D6EFCBA373B6CC9468C778010BEA92BDEE5CA75AB24F01923480BEC1B29B573FD') { throw 'Tika checksum mismatch' }
if ((Get-FileHash -LiteralPath $JavaZip -Algorithm SHA256).Hash -ne '2332C0B4CFF6BED722DA3567C91D5E47568BCBA863FA4945D0A8D6FC76158628') { throw 'Java checksum mismatch' }
if (-not (Test-Path -LiteralPath $MakeNsis -PathType Leaf)) { throw 'NSIS compiler not found' }
$output = if ([IO.Path]::IsPathRooted($OutputFile)) { $OutputFile } else { Join-Path $root $OutputFile }
if (Test-Path -LiteralPath $output) { throw 'Output exists; refusing to overwrite' }
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $output) | Out-Null
$id = [guid]::NewGuid().ToString('N')
$stage = Join-Path $root "dist\staging-$id"
$unpack = Join-Path $root "dist\jre-$id"
New-Item -ItemType Directory -Path $stage | Out-Null
& cargo build --locked --release --bins
if ($LASTEXITCODE -ne 0) { throw 'Cargo build failed' }
Expand-Archive -LiteralPath $JavaZip -DestinationPath $unpack
$jre = @(Get-ChildItem -LiteralPath $unpack -Directory)
if ($jre.Count -ne 1 -or -not (Test-Path -LiteralPath (Join-Path $jre[0].FullName 'bin\java.exe'))) { throw 'Unexpected Java archive layout' }
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'runtime') | Out-Null
Copy-Item -LiteralPath $jre[0].FullName -Destination (Join-Path $stage 'runtime\java') -Recurse
Copy-Item -LiteralPath $TikaJar -Destination (Join-Path $stage 'runtime\tika-app.jar')
Copy-Item -LiteralPath 'assets\tika-config.xml' -Destination (Join-Path $stage 'runtime\tika-config.xml')
Copy-Item -LiteralPath 'assets\icon.ico','README.md','LICENSE','THIRD_PARTY_NOTICES.md' -Destination $stage
foreach ($bin in @('llmlocalsearch.exe','index-worker.exe','index-maintenance.exe')) {
    Copy-Item -LiteralPath (Join-Path 'target\release' $bin) -Destination $stage
}
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'licenses\tika'),(Join-Path $stage 'licenses\rust'),(Join-Path $stage 'licenses\upstream') | Out-Null
Get-ChildItem -LiteralPath 'third-party\licenses' -File | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $stage 'licenses\upstream') }
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::OpenRead((Resolve-Path -LiteralPath $TikaJar).Path)
try {
    foreach ($name in @('META-INF/LICENSE','META-INF/NOTICE','META-INF/jsoup/LICENSE')) {
        $entry = $zip.GetEntry($name)
        if ($null -eq $entry) { throw "Missing Tika legal entry: $name" }
        $target = Join-Path $stage ('licenses\tika\' + ($name -replace '/', '-'))
        [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $target)
    }
} finally { $zip.Dispose() }
$meta = (& cargo metadata --locked --format-version 1 --filter-platform x86_64-pc-windows-msvc | Out-String) | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed' }
$lines = New-Object 'System.Collections.Generic.List[string]'
foreach ($pkg in $meta.packages) {
    if ($pkg.name -eq 'llmlocalsearch') { continue }
    if ([string]::IsNullOrWhiteSpace($pkg.license)) { throw "Missing SPDX license: $($pkg.name)" }
    $lines.Add("$($pkg.name) $($pkg.version) : $($pkg.license)")
    $dir = Split-Path -Parent $pkg.manifest_path
    $licenses = @(Get-ChildItem -LiteralPath $dir -File | Where-Object { $_.Name -match '^(?i:LICENSE|LICENCE|COPYING|NOTICE)' })
    if ($licenses.Count -eq 0 -and $pkg.name -notmatch '^(accesskit|clipboard-win|ecolor|eframe|egui|egui-wgpu|egui-winit|egui_glow|emath|epaint|epaint_default_fonts|gl_generator|khronos_api|profiling)$') { throw "No license text found for $($pkg.name)" }
    if ($licenses.Count -gt 0) {
        $dest = Join-Path $stage ("licenses\rust\" + $pkg.name + '-' + $pkg.version)
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        foreach ($license in $licenses) { Copy-Item -LiteralPath $license.FullName -Destination $dest }
    }
    if ($pkg.name -eq 'epaint_default_fonts') {
        $dest = Join-Path $stage 'licenses\rust\epaint-default-fonts'
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        Get-ChildItem -LiteralPath (Join-Path $dir 'fonts') -Filter '*.txt' -File | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $dest }
    }
}
[IO.File]::WriteAllLines((Join-Path $stage 'licenses\rust\dependencies.txt'), $lines, [Text.UTF8Encoding]::new($false))
$files = @(Get-ChildItem -LiteralPath $stage -Recurse -File)
$install = New-Object 'System.Collections.Generic.List[string]'
$remove = New-Object 'System.Collections.Generic.List[string]'
$collisions = New-Object 'System.Collections.Generic.List[string]'
foreach ($file in $files) {
    $relative = $file.FullName.Substring($stage.Length + 1)
    if ($relative -notmatch '^[A-Za-z0-9_.+\-\\]+$') { throw "Unexpected staging filename: $relative" }
    $folder = Split-Path -Parent $relative
    if ($folder) { $install.Add('SetOutPath "$INSTDIR\' + $folder + '"') } else { $install.Add('SetOutPath "$INSTDIR"') }
    $install.Add('File "${STAGING_DIR}\' + $relative + '"')
    $collisions.Add('IfFileExists "$INSTDIR\' + $relative + '" install_conflict')
    $remove.Add('Delete "$INSTDIR\' + $relative + '"')
}
$dirs = @(Get-ChildItem -LiteralPath $stage -Recurse -Directory | Sort-Object { $_.FullName.Length } -Descending)
foreach ($dir in $dirs) {
    $relative = $dir.FullName.Substring($stage.Length + 1)
    if ($relative -notmatch '^[A-Za-z0-9_.+\-\\]+$') { throw "Unexpected staging directory: $relative" }
    $remove.Add('RMDir "$INSTDIR\' + $relative + '"')
}
[IO.File]::WriteAllLines((Join-Path $stage 'install-files.nsh'), $install, [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllLines((Join-Path $stage 'uninstall-files.nsh'), $remove, [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllLines((Join-Path $stage 'check-collisions.nsh'), $collisions, [Text.UTF8Encoding]::new($false))
& $MakeNsis -INPUTCHARSET UTF8 "-DSTAGING_DIR=$stage" "-DOUTPUT_FILE=$output" (Join-Path $root 'installer\setup.nsi')
if ($LASTEXITCODE -ne 0) { throw 'NSIS build failed' }
Write-Output "Installer: $output"
