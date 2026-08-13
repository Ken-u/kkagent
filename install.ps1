param(
    [string]$InstallDir,
    [string]$Version = $(if ($env:KKAGENT_VERSION) { $env:KKAGENT_VERSION } else { "latest" })
)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$repository = if ($env:KKAGENT_REPOSITORY) { $env:KKAGENT_REPOSITORY } else { "Ken-u/kkagent" }
$scriptPath = $MyInvocation.MyCommand.Path
if (-not $InstallDir) {
    if ($env:KKAGENT_INSTALL_DIR) {
        $InstallDir = $env:KKAGENT_INSTALL_DIR
    } elseif ($scriptPath -and (Split-Path $scriptPath -Leaf) -eq "kkagent-update.ps1") {
        $InstallDir = Split-Path $scriptPath -Parent
    } else {
        $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\kkagent"
    }
}
$baseUrl = if ($env:KKAGENT_RELEASE_BASE_URL) {
    $env:KKAGENT_RELEASE_BASE_URL.TrimEnd("/")
} elseif ($Version -eq "latest") {
    "https://github.com/$repository/releases/latest/download"
} else {
    "https://github.com/$repository/releases/download/v$($Version.TrimStart('v'))"
}
$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)
$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$target = switch ($architecture) {
    "X64" { "x86_64-pc-windows-msvc" }
    "Arm64" { "aarch64-pc-windows-msvc" }
    default { throw "Unsupported Windows architecture: $architecture" }
}
$archive = "kkagent-$target.zip"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("kkagent-install-" + [guid]::NewGuid())

try {
    New-Item -ItemType Directory -Force $tempDir | Out-Null
    Write-Host "Downloading kkagent $Version for $target..."
    Invoke-WebRequest -Uri "$baseUrl/$archive" -OutFile (Join-Path $tempDir $archive)
    Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile (Join-Path $tempDir "SHA256SUMS")
    $line = Get-Content (Join-Path $tempDir "SHA256SUMS") | Where-Object { $_ -match "\s+$([regex]::Escape($archive))$" } | Select-Object -First 1
    if (-not $line) { throw "$archive is missing from SHA256SUMS" }
    $expected = ($line -split "\s+")[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tempDir $archive)).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { throw "Checksum verification failed for $archive" }

    $packageDir = Join-Path $tempDir "package"
    Expand-Archive -Path (Join-Path $tempDir $archive) -DestinationPath $packageDir
    New-Item -ItemType Directory -Force $InstallDir | Out-Null
    Copy-Item (Join-Path $packageDir "kkagent.exe") (Join-Path $InstallDir "kkagent.exe.new") -Force
    Move-Item (Join-Path $InstallDir "kkagent.exe.new") (Join-Path $InstallDir "kkagent.exe") -Force
    $kkPath = Join-Path $InstallDir "kk.exe"
    if (Test-Path $kkPath) { Remove-Item -Force $kkPath }
    Copy-Item (Join-Path $InstallDir "kkagent.exe") $kkPath -Force

    # Prefer the installer shipped inside the checksum-verified archive. Older
    # releases do not contain it, so retain the current script when possible or
    # download the canonical copy for `irm ... | iex` installations.
    $packagedInstaller = Join-Path $packageDir "install.ps1"
    if (Test-Path $packagedInstaller) {
        $installerSource = $packagedInstaller
    } elseif ($scriptPath -and (Test-Path $scriptPath -PathType Leaf)) {
        $installerSource = $scriptPath
    } else {
        $installerSource = Join-Path $tempDir "install.ps1"
        $installerUrl = if ($env:KKAGENT_INSTALLER_URL) {
            $env:KKAGENT_INSTALLER_URL
        } else {
            "https://raw.githubusercontent.com/$repository/main/install.ps1"
        }
        Write-Host "Downloading reusable updater..."
        Invoke-WebRequest -Uri $installerUrl -OutFile $installerSource
    }
    $updaterPath = Join-Path $InstallDir "kkagent-update.ps1"
    $newUpdaterPath = "$updaterPath.new"
    Copy-Item $installerSource $newUpdaterPath -Force
    Move-Item $newUpdaterPath $updaterPath -Force

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $parts = @($userPath -split ";" | Where-Object { $_ })
    if ($parts -notcontains $InstallDir) {
        [Environment]::SetEnvironmentVariable("Path", (($parts + $InstallDir) -join ";"), "User")
        Write-Host "Added $InstallDir to the user PATH; open a new terminal to use it."
    }
    Write-Host "Installed kkagent and kk to $InstallDir"
    Write-Host "Installed updater to $updaterPath; run kkagent-update.ps1 to upgrade"
    & (Join-Path $InstallDir "kkagent.exe") --version
}
finally {
    if (Test-Path $tempDir) { Remove-Item -Recurse -Force $tempDir }
}
