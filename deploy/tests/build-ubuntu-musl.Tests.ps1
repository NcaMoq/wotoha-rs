$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$packagerPath = Join-Path (Join-Path $repoRoot 'deploy') 'build-ubuntu-musl.ps1'
$content = [System.IO.File]::ReadAllText($packagerPath)
$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $packagerPath,
    [ref]$tokens,
    [ref]$parseErrors
)
if ($parseErrors.Count -ne 0) {
    throw "PowerShell packager has parse errors: $($parseErrors -join '; ')"
}

foreach ($forbidden in @(
    'releases/download',
    'yt-dlp_linux',
    'deno-x86_64-unknown-linux-gnu.zip',
    'Expand-Archive',
    'curl.exe',
    'gpg.exe'
)) {
    if ($content.Contains($forbidden)) {
        throw "PowerShell packager still downloads or packages a third-party runtime: $forbidden"
    }
}

foreach ($required in @(
    'wotoha-ubuntu-x86_64-musl.tar.gz',
    'wotoha-linux-x86_64-musl.tar.gz',
    'LICENSE',
    'THIRD_PARTY_NOTICES.md',
    'third-party\rust\Cargo.lock',
    'third-party\rust\license-inventory.json',
    'third-party\rust\THIRD_PARTY_LICENSES.html',
    'third-party\rust\THIRD_PARTY_ATTRIBUTIONS.txt',
    'third-party\neural-models\NOTICE.txt',
    'third-party\neural-models\LICENSE.beat-this-rs.txt',
    'third-party\neural-models\LICENSE.beat-this-original.txt',
    'a5f8d39d989f31859454ba27afe61c5317ca95e4d9373e6853e5361b8937172f',
    'fdd59e65c515331308e4c8841edf99972deca646bdf6197744c2a5b7755e3de9',
    'Wotoha third-party Rust attributions',
    '^(COPYRIGHT|NOTICE)([.-].*)?$',
    'cargo about generate --frozen --fail --workspace',
    'cargo-about 0.9.1 is required',
    'Write-InternalChecksums',
    "Assert-NativeSuccess 'cargo zigbuild'",
    'New-LinuxArchive -Root $packageRoot',
    'New-LinuxArchive -Root $portableRoot',
    'update-index --chmod=+x',
    'config core.autocrlf false',
    'config core.attributesFile'
)) {
    if (-not $content.Contains($required)) {
        throw "PowerShell packager is missing the release contract: $required"
    }
}

$encodingDefinition = $content.IndexOf('$utf8WithoutBom =')
$inventoryWrite = $content.IndexOf("(Join-Path `$rustNotices 'license-inventory.json')")
if ($encodingDefinition -lt 0 -or $inventoryWrite -lt 0 -or $encodingDefinition -gt $inventoryWrite) {
    throw 'UTF-8 encoder must be initialized before writing the license inventory.'
}

# Execute only the side-effect-free archive helper definitions against a
# disposable fixture. This catches Windows tar mode regressions without
# running the actual Rust build.
foreach ($functionName in @('Assert-NativeSuccess', 'Write-InternalChecksums', 'New-LinuxArchive')) {
    $definition = $ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
            $node.Name -eq $functionName
    }, $true) | Select-Object -First 1
    if ($null -eq $definition) {
        throw "PowerShell packager helper is missing: $functionName"
    }
    Invoke-Expression $definition.Extent.Text
}
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('wotoha-packager-test-' + [guid]::NewGuid().ToString('N'))
$distRoot = Join-Path $testRoot 'dist'
$fixtureRoot = Join-Path $distRoot 'wotoha-linux-x86_64-musl'
$gitPath = (Get-Command git -ErrorAction Stop).Source
New-Item -ItemType Directory -Path (Join-Path $fixtureRoot 'bin') -Force | Out-Null
try {
    [System.IO.File]::WriteAllText(
        (Join-Path $fixtureRoot 'bin\wotoha-app'),
        "#!/usr/bin/env bash`nexit 0`n",
        (New-Object System.Text.UTF8Encoding($false))
    )
    [System.IO.File]::WriteAllText(
        (Join-Path $fixtureRoot 'LICENSE'),
        "fixture`n",
        (New-Object System.Text.UTF8Encoding($false))
    )
    Write-InternalChecksums $fixtureRoot
    $fixtureArchive = Join-Path $distRoot 'fixture.tar.gz'
    New-LinuxArchive -Root $fixtureRoot -Archive $fixtureArchive -ExecutablePaths @('bin/wotoha-app')
    $listing = & tar -tvzf $fixtureArchive
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to inspect the PowerShell packager fixture archive.'
    }
    $appListing = @($listing | Where-Object { $_ -match 'bin/wotoha-app$' })
    if ($appListing.Count -ne 1 -or $appListing[0] -notmatch '^-rwx') {
        throw 'PowerShell packager did not preserve an executable Linux application mode.'
    }
    $extractRoot = Join-Path $testRoot 'extract'
    New-Item -ItemType Directory -Path $extractRoot | Out-Null
    & tar -xzf $fixtureArchive -C $extractRoot
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to extract the PowerShell packager fixture archive.'
    }
    $extractedPackage = Join-Path $extractRoot 'wotoha-linux-x86_64-musl'
    $checksumLines = @(Get-Content -LiteralPath (Join-Path $extractedPackage 'SHA256SUMS.txt'))
    $regularFiles = @(Get-ChildItem -LiteralPath $extractedPackage -Recurse -File | Where-Object {
        $_.Name -ne 'SHA256SUMS.txt'
    })
    if ($checksumLines.Count -ne $regularFiles.Count) {
        throw 'PowerShell archive internal checksums do not cover every regular file.'
    }
    foreach ($line in $checksumLines) {
        if ($line -notmatch '^([0-9a-f]{64})  ([^\s]+)$') {
            throw "Malformed PowerShell archive checksum: $line"
        }
        $filePath = Join-Path $extractedPackage ($Matches[2].Replace('/', [System.IO.Path]::DirectorySeparatorChar))
        $actualHash = (Get-FileHash -LiteralPath $filePath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne $Matches[1]) {
            throw "PowerShell archive checksum mismatch: $($Matches[2])"
        }
    }
}
finally {
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}

Write-Output 'ok - PowerShell release packager is runtime-free and produces both archive contracts'
