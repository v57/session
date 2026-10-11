# Installs the latest `session` release on Windows:
#
#   powershell -c "irm v57.dev/install/session.ps1|iex"
#
# Options (environment variables):
#   SESSION_VERSION      a release tag such as v0.1.0 (default: the latest release)
#   SESSION_INSTALL_DIR  where to put session.exe (default: %LOCALAPPDATA%\Programs\session\bin)
#   SESSION_DOWNLOAD_URL where the release assets live (for mirrors and testing)
#
# Works in Windows PowerShell 5.1 and PowerShell 7. Everything runs inside one
# script block, so a download cut short never runs half of it.

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is far slower with the progress bar
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

    $repo = 'v57/session'
    $version = if ($env:SESSION_VERSION) { $env:SESSION_VERSION } else { 'latest' }
    $binDir = if ($env:SESSION_INSTALL_DIR) { $env:SESSION_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\session\bin' }

    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }  # 32-bit PowerShell on 64-bit Windows
    switch ($arch) {
        'AMD64' { }
        'ARM64' { Write-Host 'No ARM64 build yet: installing the x86_64 build, which Windows on ARM runs under emulation.' }
        default { throw "session install: unsupported CPU: $arch" }
    }

    $asset = 'session-x86_64-pc-windows-msvc.zip'
    $base = if ($env:SESSION_DOWNLOAD_URL) {
        $env:SESSION_DOWNLOAD_URL
    } elseif ($version -eq 'latest') {
        "https://github.com/$repo/releases/latest/download"
    } else {
        "https://github.com/$repo/releases/download/$version"
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading $asset ($version)"
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile (Join-Path $tmp $asset)
        Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')

        $expected = Get-Content (Join-Path $tmp 'SHA256SUMS') |
            ForEach-Object { $f = $_ -split '\s+'; if ($f[1] -eq $asset -or $f[1] -eq "*$asset") { $f[0] } } |
            Select-Object -First 1
        if (-not $expected) { throw "session install: SHA256SUMS has no entry for $asset" }
        $actual = (Get-FileHash -Algorithm SHA256 -Path (Join-Path $tmp $asset)).Hash.ToLowerInvariant()
        if ($actual -ne $expected.ToLowerInvariant()) {
            throw "session install: checksum mismatch for $asset (expected $expected, got $actual)"
        }
        Write-Host 'Checksum OK'

        Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath (Join-Path $tmp 'x') -Force
        New-Item -ItemType Directory -Force -Path $binDir | Out-Null
        $exe = Join-Path $binDir 'session.exe'
        # A running daemon locks session.exe, but Windows lets a running exe be
        # renamed: move it aside so the new one can take its place.
        if (Test-Path $exe) {
            $old = "$exe.old"
            Remove-Item -Force $old -ErrorAction SilentlyContinue
            if (Test-Path $old) { $old = "$exe.$([Guid]::NewGuid().ToString('N')).old" }
            Move-Item -Force $exe $old
            Remove-Item -Force $old -ErrorAction SilentlyContinue
        }
        Copy-Item -Force (Join-Path $tmp 'x\session.exe') $exe
        Write-Host "Installed $(& $exe --version) to $exe"
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }

    if (Get-Process -Name session -ErrorAction SilentlyContinue) {
        Write-Host ''
        Write-Host 'A session daemon is already running and keeps its version until it exits.'
        Write-Host 'If session reports a version mismatch, end your sessions and run: Stop-Process -Name session'
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $onPath = ($env:Path -split ';') + ($userPath -split ';') | Where-Object { $_.TrimEnd('\') -ieq $binDir.TrimEnd('\') }
    if (-not $onPath) {
        Write-Host ''
        Write-Host "$binDir is not on your PATH. To add it for your user, run:"
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"$binDir;`" + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
        Write-Host 'then open a new terminal.'
    }
}
