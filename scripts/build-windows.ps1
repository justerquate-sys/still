param([ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')][string]$Target = 'x86_64-pc-windows-msvc')
$ErrorActionPreference = 'Stop'
Push-Location (Split-Path $PSScriptRoot -Parent)
try {
    cargo build --release --locked --target $Target --bin Still
    if ($LASTEXITCODE -ne 0) { throw 'Cargo build failed' }
    $arch = if ($Target.StartsWith('aarch64')) { 'arm64' } else { 'x64' }
    $name = "Still-2.4.0-windows-$arch"
    $stage = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
        Copy-Item "target/$Target/release/Still.exe" "$stage/Still.exe"
        Copy-Item 'Resources/Windows-Readme.txt' "$stage/Readme.txt"
        Copy-Item 'Resources/ThirdPartyNotices.txt' "$stage/ThirdPartyNotices.txt"
        $metadata = cargo metadata --locked --format-version 1 --filter-platform $Target | ConvertFrom-Json
        if ($LASTEXITCODE -ne 0) { throw 'Could not gather dependency licenses' }
        $included = @{}
        foreach ($node in $metadata.resolve.nodes) { $included[$node.id] = $true }
        foreach ($package in $metadata.packages) {
            if (-not $package.source -or -not $included.ContainsKey($package.id)) { continue }
            $directory = Split-Path $package.manifest_path -Parent
            $licenses = Get-ChildItem -LiteralPath $directory -File |
                Where-Object { $_.Name -match '^(LICENSE|LICENCE|COPYING|COPYRIGHT)' }
            $destination = "$stage/Licenses/$($package.name)-$($package.version)"
            New-Item -ItemType Directory -Force -Path $destination | Out-Null
            $package.license | Set-Content -Encoding utf8 "$destination/SPDX.txt"
            foreach ($license in $licenses) { Copy-Item -LiteralPath $license.FullName -Destination $destination }
        }
        $sysroot = rustc --print sysroot
        if ($LASTEXITCODE -ne 0) { throw 'Could not locate Rust runtime notices' }
        $rustNotices = Join-Path $sysroot 'share/doc/rust'
        New-Item -ItemType Directory -Force -Path "$stage/Licenses/Rust" | Out-Null
        Copy-Item "$rustNotices/COPYRIGHT-library.html" "$stage/Licenses/Rust/"
        Copy-Item "$rustNotices/licenses" "$stage/Licenses/Rust/" -Recurse
        New-Item -ItemType Directory -Force -Path 'dist' | Out-Null
        $archive = Join-Path (Get-Location) "dist/$name.zip"
        Compress-Archive -Path "$stage/*" -DestinationPath $archive -Force
        (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower() + "  $name.zip" |
            Set-Content -Encoding ascii "$archive.sha256"
        Write-Output "Packaged $archive"
    } finally {
        Remove-Item -LiteralPath $stage -Recurse -Force
    }
} finally {
    Pop-Location
}
