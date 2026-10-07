# Sets up the Windows toolchain for Agent Wiki's Rust code (developer machines only):
#   - Visual Studio Build Tools 2026 with the C++ workload (MSVC linker + Windows SDK); one UAC prompt
#   - Rust (rustup, stable, MSVC target) in %LOCALAPPDATA%\rustup and %LOCALAPPDATA%\cargo, not ~/.rustup and ~/.cargo
#
# Run it from a NORMAL terminal (Win+R, cmd), not from inside the Claude or ChatGPT desktop apps or Store
# PowerShell: those are app packages, and Windows would redirect what they install under AppData.
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev\setup-rust-windows.ps1
# Re-running is safe: finished steps are skipped.

$ErrorActionPreference = 'Stop'
$local = [Environment]::GetFolderPath('LocalApplicationData')
$rustupHome = Join-Path $local 'rustup'
$cargoHome = Join-Path $local 'cargo'

Write-Host "`n[1] Visual Studio Build Tools 2026 (C++ workload, Windows SDK)"
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vc = if (Test-Path $vswhere) { & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath } else { $null }
if ($vc) { Write-Host "    already installed: $vc" }
else {
  Write-Host '    installing (a few GB; approve the UAC prompt; this takes a while)...'
  winget install --id Microsoft.VisualStudio.BuildTools --source winget --exact --accept-package-agreements --accept-source-agreements `
    --override '--quiet --wait --norestart --nocache --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
  if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 3010) { throw "winget exited with $LASTEXITCODE" }
  $vc = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
  if (-not $vc) { throw 'The C++ build tools are still missing after the install.' }
  Write-Host "    installed: $vc"
}

Write-Host "`n[2] Rust in $rustupHome and $cargoHome"
[Environment]::SetEnvironmentVariable('RUSTUP_HOME', $rustupHome, 'User')
[Environment]::SetEnvironmentVariable('CARGO_HOME', $cargoHome, 'User')
$env:RUSTUP_HOME = $rustupHome
$env:CARGO_HOME = $cargoHome
$cargo = Join-Path $cargoHome 'bin\cargo.exe'
if (Test-Path $cargo) { Write-Host '    rustup already installed; updating stable' ; & (Join-Path $cargoHome 'bin\rustup.exe') update stable }
else {
  $init = Join-Path $env:TEMP 'rustup-init.exe'
  Invoke-WebRequest -UseBasicParsing 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe' -OutFile $init
  & $init -y --default-toolchain stable --profile minimal --component clippy,rustfmt
  if ($LASTEXITCODE -ne 0) { throw "rustup-init exited with $LASTEXITCODE" }
  Remove-Item $init -Force
}
# rustup adds %CARGO_HOME%\bin to the user PATH; make sure it is there.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not ($userPath -split ';' | Where-Object { $_ -eq "$cargoHome\bin" })) {
  [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ";$cargoHome\bin"), 'User')
}
$env:Path = "$cargoHome\bin;$env:Path"

Write-Host "`n[3] Check"
& (Join-Path $cargoHome 'bin\rustc.exe') --version
& $cargo --version
Write-Host "`nDone. Open a new terminal (or restart the apps) to pick up RUSTUP_HOME, CARGO_HOME and PATH."
