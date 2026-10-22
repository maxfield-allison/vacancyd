# Import the installed x64 MSVC toolchain without third-party setup actions.
$ErrorActionPreference = 'Stop'
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path $vswhere)) { throw 'Visual Studio Build Tools are not installed' }
$installation = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ([string]::IsNullOrWhiteSpace($installation)) { throw 'MSVC x64 tools are missing' }
$command = Join-Path $installation 'Common7\Tools\VsDevCmd.bat'
$environment = & cmd.exe /d /s /c "`"$command`" -no_logo -arch=x64 -host_arch=x64 && set"
if ($LASTEXITCODE -ne 0) { throw 'Could not initialize MSVC environment' }
foreach ($line in $environment) {
    if ($line -match '^([^=]+)=(.*)$') {
        [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
    }
}
if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) { throw 'MSVC linker is missing' }
