# Downloads the Windows Docker CLI bundled by the installer.
#
# The Chocolatey docker-cli package repackages Docker's official static build
# (https://download.docker.com/win/static/stable/x86_64/docker-<version>.zip;
# see legal\VERIFICATION.txt inside the package). Both the package and the
# docker.exe it contains are pinned by SHA256.
#
# Usage, from the repository root:
#   pwsh packaging\windows\fetch-docker-cli.ps1 [-OutDir dist\docker-cli]
# Writes docker.exe, LICENSE.txt and NOTICE.txt to OutDir; pass that directory
# to makensis as /DDOCKER_CLI_DIR=<absolute path>.

param(
    [string]$OutDir = (Join-Path $PWD 'dist\docker-cli')
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Version = '29.9.3'
$PackageUrl = "https://community.chocolatey.org/api/v2/package/docker-cli/$Version"
$PackageSha256 = '0644c4a47b88af435a1cdbb9d35f08587d2f8612bd1c880c81fd0284256be194'
$DockerExeSha256 = '82b011a6c253c6c6a273891133fbf8c76f0a909483f5c34e6dcb7ebc98b91dd5'

function Assert-Sha256([string]$Path, [string]$Expected) {
    $actual = (Get-FileHash -Algorithm SHA256 $Path).Hash
    if ($actual -ne $Expected) {
        throw "SHA256 mismatch for ${Path}: expected $Expected, got $actual"
    }
}

$exe = Join-Path $OutDir 'docker.exe'
if ((Test-Path $exe) -and (Get-FileHash -Algorithm SHA256 $exe).Hash -eq $DockerExeSha256 `
        -and (Test-Path (Join-Path $OutDir 'LICENSE.txt')) -and (Test-Path (Join-Path $OutDir 'NOTICE.txt'))) {
    Write-Host "Docker CLI $Version already in $OutDir"
    return
}

$work = Join-Path ([IO.Path]::GetTempPath()) "rusker-docker-cli-$([Guid]::NewGuid())"
New-Item -ItemType Directory -Force $work | Out-Null
try {
    $package = Join-Path $work 'docker-cli.nupkg'
    Write-Host "Downloading $PackageUrl"
    Invoke-WebRequest -Uri $PackageUrl -OutFile $package -MaximumRetryCount 5 -RetryIntervalSec 5
    Assert-Sha256 $package $PackageSha256

    # A .nupkg is a zip archive; Expand-Archive insists on the .zip extension.
    $zip = Join-Path $work 'docker-cli.zip'
    Move-Item $package $zip
    $extracted = Join-Path $work 'pkg'
    Expand-Archive -Path $zip -DestinationPath $extracted
    Assert-Sha256 (Join-Path $extracted 'bin\docker.exe') $DockerExeSha256

    New-Item -ItemType Directory -Force $OutDir | Out-Null
    Copy-Item (Join-Path $extracted 'bin\docker.exe') $exe -Force
    Copy-Item (Join-Path $extracted 'legal\LICENSE.txt') (Join-Path $OutDir 'LICENSE.txt') -Force
    Copy-Item (Join-Path $extracted 'legal\NOTICE.txt') (Join-Path $OutDir 'NOTICE.txt') -Force
    Write-Host "Docker CLI $Version written to $OutDir"
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
