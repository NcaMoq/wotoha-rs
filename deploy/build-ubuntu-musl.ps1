$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$target = 'x86_64-unknown-linux-musl'
$targetDir = Join-Path $repoRoot 'target\ubuntu-musl'
$distRoot = Join-Path $repoRoot 'dist'
$packageRoot = Join-Path $distRoot 'wotoha-ubuntu-x86_64-musl'
$archivePath = Join-Path $distRoot 'wotoha-ubuntu-x86_64-musl.tar.gz'
$portableRoot = Join-Path $distRoot 'wotoha-linux-x86_64-musl'
$portableArchivePath = Join-Path $distRoot 'wotoha-linux-x86_64-musl.tar.gz'

function Resolve-Tool {
    param(
        [AllowNull()]
        [AllowEmptyCollection()]
        [object[]]$Candidates,
        [Parameter(Mandatory = $true)]
        [string]$Name
    )

    foreach ($candidate in $Candidates) {
        $candidatePath = [string]$candidate
        if ([string]::IsNullOrWhiteSpace($candidatePath)) {
            continue
        }
        if (Test-Path $candidatePath) {
            return (Resolve-Path $candidatePath).Path
        }
    }

    throw "$Name was not found."
}

function Assert-NativeSuccess {
    param([Parameter(Mandatory = $true)][string]$Description)
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE."
    }
}

function Write-InternalChecksums {
    param([Parameter(Mandatory = $true)][string]$Root)
    $checksumPath = Join-Path $Root 'SHA256SUMS.txt'
    $lines = @(Get-ChildItem -LiteralPath $Root -Recurse -File | Where-Object {
        $_.FullName -ne $checksumPath
    } | ForEach-Object {
        $relative = $_.FullName.Substring($Root.Length).TrimStart('\', '/').Replace('\', '/')
        $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        [pscustomobject]@{ Relative = $relative; Line = "$hash  $relative" }
    } | Sort-Object Relative | Select-Object -ExpandProperty Line)
    if ($lines.Count -eq 0) {
        throw "No regular files were found beneath $Root."
    }
    [System.IO.File]::WriteAllText(
        $checksumPath,
        (($lines -join "`n") + "`n"),
        (New-Object System.Text.UTF8Encoding($false))
    )
}

function New-LinuxArchive {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$Archive,
        [Parameter(Mandatory = $true)][string[]]$ExecutablePaths
    )
    $stage = Join-Path $distRoot ('.git-archive-' + [guid]::NewGuid().ToString('N'))
    $stageFull = [System.IO.Path]::GetFullPath($stage)
    $distFull = [System.IO.Path]::GetFullPath($distRoot).TrimEnd('\', '/') + [System.IO.Path]::DirectorySeparatorChar
    if (-not $stageFull.StartsWith($distFull, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw 'Refusing to create an archive staging directory outside dist.'
    }
    New-Item -ItemType Directory -Path $stageFull | Out-Null
    try {
        $rootName = Split-Path -Leaf $Root
        Copy-Item -LiteralPath $Root -Destination $stageFull -Recurse
        & $gitPath -C $stageFull init --quiet
        Assert-NativeSuccess 'temporary archive repository initialization'
        & $gitPath -C $stageFull config core.autocrlf false
        Assert-NativeSuccess 'temporary archive line-ending configuration'
        $emptyAttributes = Join-Path $stageFull '.git\info\attributes'
        [System.IO.File]::WriteAllText($emptyAttributes, '', (New-Object System.Text.UTF8Encoding($false)))
        & $gitPath -C $stageFull config core.attributesFile $emptyAttributes
        Assert-NativeSuccess 'temporary archive attribute configuration'
        & $gitPath -C $stageFull add --all -- $rootName
        Assert-NativeSuccess 'temporary archive index creation'
        foreach ($relative in $ExecutablePaths) {
            & $gitPath -C $stageFull update-index --chmod=+x -- "$rootName/$relative"
            Assert-NativeSuccess "archive executable mode for $relative"
        }
        $tree = (& $gitPath -C $stageFull write-tree).Trim()
        Assert-NativeSuccess 'temporary archive tree creation'
        if ($tree -notmatch '^[0-9a-f]{40,64}$') {
            throw 'git write-tree returned an invalid object ID.'
        }
        & $gitPath -C $stageFull archive --format=tar.gz "--output=$Archive" $tree
        Assert-NativeSuccess "archive creation for $rootName"
    }
    finally {
        if (Test-Path -LiteralPath $stageFull) {
            Remove-Item -LiteralPath $stageFull -Recurse -Force
        }
    }
}

$zigPath = Resolve-Tool -Name 'zig.exe' -Candidates @(
    (Get-Command zig.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -First 1),
    (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages\zig.zig_Microsoft.Winget.Source_8wekyb3d8bbwe\zig-x86_64-windows-0.16.0\zig.exe')
)

$cmakePath = Resolve-Tool -Name 'cmake.exe' -Candidates @(
    (Get-Command cmake.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -First 1),
    'C:\Program Files\CMake\bin\cmake.exe'
)

$ninjaPath = Resolve-Tool -Name 'ninja.exe' -Candidates @(
    (Get-Command ninja.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -First 1),
    (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages\Ninja-build.Ninja_Microsoft.Winget.Source_8wekyb3d8bbwe\ninja.exe')
)
$gitPath = (Get-Command git -ErrorAction Stop).Source

$env:PATH = @(
    (Split-Path -Parent $zigPath),
    (Split-Path -Parent $cmakePath),
    (Split-Path -Parent $ninjaPath),
    $env:PATH
) -join ';'

$env:CMAKE_GENERATOR = 'Ninja'

rustup target add $target
Assert-NativeSuccess 'rustup target add'

cargo zigbuild --locked --release --bin wotoha-app --target $target --target-dir $targetDir
Assert-NativeSuccess 'cargo zigbuild'

Remove-Item $packageRoot -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item $portableRoot -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $packageRoot | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $packageRoot 'bin') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $packageRoot 'deploy') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $packageRoot 'docs') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $packageRoot 'third-party') | Out-Null

$versions = @{}
Get-Content (Join-Path $repoRoot 'deploy\third-party-versions.env') | ForEach-Object {
    if ($_ -match '^([A-Z0-9_]+)=(.+)$') {
        $versions[$Matches[1]] = $Matches[2]
    }
}
foreach ($requiredVersion in @('YTDLP_REPOSITORY', 'YTDLP_VERSION', 'DENO_VERSION', 'DENO_X86_64_LINUX_GNU_SHA256')) {
    if (-not $versions.ContainsKey($requiredVersion)) {
        throw "third-party-versions.env is missing $requiredVersion."
    }
}

$thirdParty = Join-Path $packageRoot 'third-party'
$allowedYtDlpRepositories = @('yt-dlp/yt-dlp', 'yt-dlp/yt-dlp-nightly-builds')
if ($versions.YTDLP_REPOSITORY -notin $allowedYtDlpRepositories) {
    throw 'YTDLP_REPOSITORY must name an official yt-dlp release repository.'
}
if ($versions.YTDLP_VERSION -notmatch '^[0-9]{4}[.][0-9]{2}[.][0-9]{2}([.][0-9]{6})?$') {
    throw 'YTDLP_VERSION is not a release tag.'
}
if ($versions.DENO_VERSION -notmatch '^[0-9]+[.][0-9]+[.][0-9]+$') {
    throw 'DENO_VERSION is not a release version.'
}
if ($versions.DENO_X86_64_LINUX_GNU_SHA256 -notmatch '^[0-9a-f]{64}$') {
    throw 'Deno digest is not lowercase SHA-256.'
}

Copy-Item (Join-Path $targetDir "$target\release\wotoha-app") (Join-Path $packageRoot 'bin\wotoha-app')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha.service') (Join-Path $packageRoot 'deploy\wotoha.service')
Copy-Item (Join-Path $repoRoot 'deploy\install-ubuntu.sh') (Join-Path $packageRoot 'install-ubuntu.sh')
Copy-Item (Join-Path $repoRoot 'deploy\install-yt-dlp-bundle.sh') (Join-Path $packageRoot 'install-yt-dlp-bundle.sh')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha-update.sh') (Join-Path $packageRoot 'wotoha-update.sh')
Copy-Item (Join-Path $repoRoot 'deploy\yt-dlp-update.sh') (Join-Path $packageRoot 'yt-dlp-update.sh')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha.env.example') (Join-Path $packageRoot 'deploy\wotoha.env.example')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha-update.env.example') (Join-Path $packageRoot 'deploy\wotoha-update.env.example')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha-update.service') (Join-Path $packageRoot 'deploy\wotoha-update.service')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha-update.timer') (Join-Path $packageRoot 'deploy\wotoha-update.timer')
Copy-Item (Join-Path $repoRoot 'deploy\yt-dlp-update.service') (Join-Path $packageRoot 'deploy\yt-dlp-update.service')
Copy-Item (Join-Path $repoRoot 'deploy\yt-dlp-update.timer') (Join-Path $packageRoot 'deploy\yt-dlp-update.timer')
Copy-Item (Join-Path $repoRoot 'deploy\yt-dlp-public.key') (Join-Path $packageRoot 'deploy\yt-dlp-public.key')
Copy-Item (Join-Path $repoRoot 'deploy\third-party-versions.env') (Join-Path $packageRoot 'deploy\third-party-versions.env')
Copy-Item (Join-Path $repoRoot 'docs\ubuntu-deploy.md') (Join-Path $packageRoot 'docs\ubuntu-deploy.md')
Copy-Item (Join-Path $repoRoot 'docs\youtube-extraction.md') (Join-Path $packageRoot 'docs\youtube-extraction.md')
Copy-Item (Join-Path $repoRoot 'LICENSE') (Join-Path $packageRoot 'LICENSE')
Copy-Item (Join-Path $repoRoot 'THIRD_PARTY_NOTICES.md') (Join-Path $packageRoot 'THIRD_PARTY_NOTICES.md')
$utf8WithoutBom = New-Object System.Text.UTF8Encoding($false)
$rustNotices = Join-Path $thirdParty 'rust'
New-Item -ItemType Directory -Force -Path $rustNotices | Out-Null
Copy-Item (Join-Path $repoRoot 'Cargo.lock') (Join-Path $rustNotices 'Cargo.lock')
$cargoMetadataJson = & cargo metadata --locked --format-version 1
Assert-NativeSuccess 'cargo metadata for the license inventory'
$cargoMetadata = $cargoMetadataJson | ConvertFrom-Json
$licensePackages = @($cargoMetadata.packages | ForEach-Object {
    if ([string]::IsNullOrWhiteSpace($_.license) -and [string]::IsNullOrWhiteSpace($_.license_file)) {
        throw "Cargo package $($_.name) $($_.version) has no declared license information."
    }
    [ordered]@{
        name = $_.name
        version = $_.version
        source = $_.source
        license = $_.license
        license_file = if ($_.license_file) { Split-Path -Leaf $_.license_file } else { $null }
        repository = $_.repository
    }
} | Sort-Object { $_.name }, { $_.version }, { $_.source })
$licenseInventory = [ordered]@{
    schema_version = 1
    generated_from = 'Cargo.lock'
    target = $target
    packages = $licensePackages
} | ConvertTo-Json -Depth 5
[System.IO.File]::WriteAllText(
    (Join-Path $rustNotices 'license-inventory.json'),
    $licenseInventory,
    $utf8WithoutBom
)
$licenseBundlePath = Join-Path $targetDir 'THIRD_PARTY_LICENSES.html'
$cargoAboutVersion = (& cargo about --version | Out-String).Trim()
Assert-NativeSuccess 'cargo-about version check'
if ($cargoAboutVersion -notmatch '(^| )cargo-about 0[.]9[.]1($| )') {
    throw "cargo-about 0.9.1 is required; found: $cargoAboutVersion"
}
& cargo about generate --frozen --fail --workspace `
    --config (Join-Path $repoRoot 'deploy\release-about.toml') `
    --output-file $licenseBundlePath `
    (Join-Path $repoRoot 'deploy\third-party-licenses.hbs')
Assert-NativeSuccess 'cargo-about third-party license generation'
$licenseBundleText = if (Test-Path -LiteralPath $licenseBundlePath) {
    [System.IO.File]::ReadAllText($licenseBundlePath)
} else {
    ''
}
if (-not $licenseBundleText.Contains('Wotoha third-party Rust licenses') -or
    -not $licenseBundleText.Contains('Used by:') -or
    -not $licenseBundleText.Contains('https://crates.io/crates/')) {
    throw 'cargo-about produced an empty or unexpected third-party license bundle.'
}
Copy-Item $licenseBundlePath (Join-Path $rustNotices 'THIRD_PARTY_LICENSES.html')
$attributions = New-Object System.Text.StringBuilder
[void]$attributions.Append("Wotoha third-party Rust attributions`n")
[void]$attributions.Append("Generated from standalone COPYRIGHT and NOTICE files in the locked dependency graph.`n")
foreach ($cargoPackage in @($cargoMetadata.packages | Sort-Object { $_.name }, { $_.version }, { $_.source })) {
    $packageDirectory = Split-Path -Parent $cargoPackage.manifest_path
    if (-not (Test-Path -LiteralPath $packageDirectory -PathType Container)) {
        throw "Cargo package source is unavailable: $($cargoPackage.name) $($cargoPackage.version)"
    }
    foreach ($attributionFile in @(Get-ChildItem -LiteralPath $packageDirectory -File | Where-Object {
        $_.Name -match '^(COPYRIGHT|NOTICE)([.-].*)?$'
    } | Sort-Object Name)) {
        [void]$attributions.Append(
            "`n===== $($cargoPackage.name) $($cargoPackage.version) -- $($attributionFile.Name) =====`n"
        )
        $attributionText = [System.IO.File]::ReadAllText($attributionFile.FullName).Replace("`r`n", "`n").Replace("`r", "`n")
        [void]$attributions.Append($attributionText)
        if (-not $attributionText.EndsWith("`n")) {
            [void]$attributions.Append("`n")
        }
    }
}
[System.IO.File]::WriteAllText(
    (Join-Path $rustNotices 'THIRD_PARTY_ATTRIBUTIONS.txt'),
    $attributions.ToString(),
    $utf8WithoutBom
)

$deploymentTextFiles = @(
    (Join-Path $packageRoot 'install-ubuntu.sh'),
    (Join-Path $packageRoot 'install-yt-dlp-bundle.sh'),
    (Join-Path $packageRoot 'wotoha-update.sh'),
    (Join-Path $packageRoot 'yt-dlp-update.sh')
) + @(Get-ChildItem (Join-Path $packageRoot 'deploy') -File | Select-Object -ExpandProperty FullName)
foreach ($deploymentTextFile in $deploymentTextFiles) {
    $content = [System.IO.File]::ReadAllText($deploymentTextFile).Replace("`r`n", "`n")
    [System.IO.File]::WriteAllText($deploymentTextFile, $content, $utf8WithoutBom)
}

[System.IO.File]::WriteAllText(
    (Join-Path $packageRoot 'RELEASE_VERSION'),
    "manual`n",
    $utf8WithoutBom
)

Remove-Item $archivePath -Force -ErrorAction SilentlyContinue
Remove-Item $portableArchivePath -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $portableRoot | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $portableRoot 'bin') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $portableRoot 'third-party\rust') | Out-Null
Copy-Item (Join-Path $packageRoot 'bin\wotoha-app') (Join-Path $portableRoot 'bin\wotoha-app')
Copy-Item (Join-Path $packageRoot 'LICENSE') (Join-Path $portableRoot 'LICENSE')
Copy-Item (Join-Path $packageRoot 'THIRD_PARTY_NOTICES.md') (Join-Path $portableRoot 'THIRD_PARTY_NOTICES.md')
Copy-Item (Join-Path $packageRoot 'RELEASE_VERSION') (Join-Path $portableRoot 'RELEASE_VERSION')
Copy-Item (Join-Path $rustNotices 'Cargo.lock') (Join-Path $portableRoot 'third-party\rust\Cargo.lock')
Copy-Item (Join-Path $rustNotices 'license-inventory.json') (Join-Path $portableRoot 'third-party\rust\license-inventory.json')
Copy-Item (Join-Path $rustNotices 'THIRD_PARTY_LICENSES.html') (Join-Path $portableRoot 'third-party\rust\THIRD_PARTY_LICENSES.html')
Copy-Item (Join-Path $rustNotices 'THIRD_PARTY_ATTRIBUTIONS.txt') (Join-Path $portableRoot 'third-party\rust\THIRD_PARTY_ATTRIBUTIONS.txt')

Write-InternalChecksums $packageRoot
Write-InternalChecksums $portableRoot
New-LinuxArchive -Root $packageRoot -Archive $archivePath -ExecutablePaths @(
    'bin/wotoha-app',
    'install-ubuntu.sh',
    'install-yt-dlp-bundle.sh',
    'wotoha-update.sh',
    'yt-dlp-update.sh'
)
New-LinuxArchive -Root $portableRoot -Archive $portableArchivePath -ExecutablePaths @(
    'bin/wotoha-app'
)

Write-Output "binary: $(Join-Path $targetDir "$target\release\wotoha-app")"
Write-Output "package: $packageRoot"
Write-Output "archive: $archivePath"
Write-Output "portable package: $portableRoot"
Write-Output "portable archive: $portableArchivePath"
