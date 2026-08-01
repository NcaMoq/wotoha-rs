$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$target = 'x86_64-unknown-linux-musl'
$targetDir = Join-Path $repoRoot 'target\ubuntu-musl'
$distRoot = Join-Path $repoRoot 'dist'
$packageRoot = Join-Path $distRoot 'wotoha-ubuntu-x86_64-musl'
$archivePath = Join-Path $distRoot 'wotoha-ubuntu-x86_64-musl.tar.gz'

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

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)]
        [string]$FilePath,
        [Parameter(Mandatory = $true)]
        [string[]]$ArgumentList
    )

    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath failed with exit code $LASTEXITCODE."
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

$curlPath = Resolve-Tool -Name 'curl.exe' -Candidates @(
    (Get-Command curl.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -First 1)
)

$gpgPath = Resolve-Tool -Name 'gpg.exe' -Candidates @(
    (Get-Command gpg.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -First 1),
    (Join-Path $env:ProgramFiles 'Git\usr\bin\gpg.exe')
)

$gitGpgPath = Join-Path $env:ProgramFiles 'Git\usr\bin\gpg.exe'
$usingGitForWindowsGpg = (Resolve-Path $gpgPath).Path -eq (Resolve-Path $gitGpgPath -ErrorAction SilentlyContinue).Path
function Convert-ToGpgPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    $fullPath = [System.IO.Path]::GetFullPath($Path)
    if (-not $usingGitForWindowsGpg) {
        return $fullPath
    }
    $drive = $fullPath.Substring(0, 1).ToLowerInvariant()
    $remainder = $fullPath.Substring(3).Replace('\', '/')
    return "/$drive/$remainder"
}

$env:PATH = @(
    (Split-Path -Parent $zigPath),
    (Split-Path -Parent $cmakePath),
    (Split-Path -Parent $ninjaPath),
    $env:PATH
) -join ';'

$env:CMAKE_GENERATOR = 'Ninja'

rustup target add $target

cargo zigbuild --release --bin wotoha-app --bin wotoha-youtube-js-worker --target $target --target-dir $targetDir

Remove-Item $packageRoot -Recurse -Force -ErrorAction SilentlyContinue
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
$ytDlp = Join-Path $thirdParty 'yt-dlp'
$ytDlpSums = Join-Path $thirdParty 'SHA2-256SUMS'
$ytDlpSignature = Join-Path $thirdParty 'SHA2-256SUMS.sig'
$allowedYtDlpRepositories = @('yt-dlp/yt-dlp', 'yt-dlp/yt-dlp-nightly-builds')
if ($versions.YTDLP_REPOSITORY -notin $allowedYtDlpRepositories) {
    throw 'YTDLP_REPOSITORY must name an official yt-dlp release repository.'
}
$ytDlpBase = "https://github.com/$($versions.YTDLP_REPOSITORY)/releases/download/$($versions.YTDLP_VERSION)"
Invoke-Checked $curlPath @('--fail', '--silent', '--show-error', '--location', '--retry', '3', '--max-filesize', '134217728', '--remove-on-error', "$ytDlpBase/yt-dlp_linux", '--output', $ytDlp)
Invoke-Checked $curlPath @('--fail', '--silent', '--show-error', '--location', '--retry', '3', '--max-filesize', '262144', '--remove-on-error', "$ytDlpBase/SHA2-256SUMS", '--output', $ytDlpSums)
Invoke-Checked $curlPath @('--fail', '--silent', '--show-error', '--location', '--retry', '3', '--max-filesize', '65536', '--remove-on-error', "$ytDlpBase/SHA2-256SUMS.sig", '--output', $ytDlpSignature)

$gpgHome = Join-Path $distRoot '.yt-dlp-gnupg'
Remove-Item $gpgHome -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $gpgHome | Out-Null
try {
    $publicKey = Join-Path $repoRoot 'deploy\yt-dlp-public.key'
    $gpgHomeArgument = Convert-ToGpgPath $gpgHome
    $publicKeyArgument = Convert-ToGpgPath $publicKey
    $ytDlpSignatureArgument = Convert-ToGpgPath $ytDlpSignature
    $ytDlpSumsArgument = Convert-ToGpgPath $ytDlpSums
    Invoke-Checked $gpgPath @('--batch', '--homedir', $gpgHomeArgument, '--import', $publicKeyArgument)
    $fingerprintOutput = & $gpgPath --batch --homedir $gpgHomeArgument --with-colons --fingerprint
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to read the imported yt-dlp signing key fingerprint.'
    }
    $fingerprint = $fingerprintOutput | Where-Object { $_ -like 'fpr:*' } | Select-Object -First 1
    $fingerprint = ($fingerprint -split ':')[9]
    if ($fingerprint -ne 'AC0CBBE6848D6A873464AF4E57CF65933B5A7581') {
        throw "Unexpected yt-dlp signing key fingerprint: $fingerprint"
    }
    Invoke-Checked $gpgPath @('--batch', '--homedir', $gpgHomeArgument, '--verify', $ytDlpSignatureArgument, $ytDlpSumsArgument)
}
finally {
    Remove-Item $gpgHome -Recurse -Force -ErrorAction SilentlyContinue
}

$ytDlpChecksumMatches = @(Get-Content $ytDlpSums | Where-Object { $_ -match '^([0-9a-f]{64})  yt-dlp_linux$' })
if ($ytDlpChecksumMatches.Count -ne 1) {
    throw 'The signed yt-dlp checksum file did not contain exactly one yt-dlp_linux entry.'
}
$expectedYtDlpHash = ([regex]::Match($ytDlpChecksumMatches[0], '^([0-9a-f]{64})')).Groups[1].Value
$actualYtDlpHash = (Get-FileHash $ytDlp -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualYtDlpHash -ne $expectedYtDlpHash) {
    throw 'yt-dlp did not match its signed checksum.'
}

$denoZip = Join-Path $thirdParty 'deno.zip'
$denoUnpacked = Join-Path $thirdParty 'deno-unpacked'
$denoUrl = "https://github.com/denoland/deno/releases/download/v$($versions.DENO_VERSION)/deno-x86_64-unknown-linux-gnu.zip"
Invoke-Checked $curlPath @('--fail', '--silent', '--show-error', '--location', '--retry', '3', '--max-filesize', '134217728', '--remove-on-error', $denoUrl, '--output', $denoZip)
$actualDenoHash = (Get-FileHash $denoZip -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualDenoHash -ne $versions.DENO_X86_64_LINUX_GNU_SHA256) {
    throw 'Deno archive did not match the pinned checksum.'
}
Expand-Archive -LiteralPath $denoZip -DestinationPath $denoUnpacked -Force
Move-Item (Join-Path $denoUnpacked 'deno') (Join-Path $thirdParty 'deno')
Remove-Item $denoZip -Force
Remove-Item $denoUnpacked -Recurse -Force

Copy-Item (Join-Path $targetDir "$target\release\wotoha-app") (Join-Path $packageRoot 'bin\wotoha-app')
Copy-Item (Join-Path $targetDir "$target\release\wotoha-youtube-js-worker") (Join-Path $packageRoot 'bin\wotoha-youtube-js-worker')
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
Copy-Item (Join-Path $repoRoot 'deploy\youtube-clients.json') (Join-Path $packageRoot 'deploy\youtube-clients.json')
Copy-Item (Join-Path $repoRoot 'deploy\YOUTUBE_WORKER_SEQUENCE') (Join-Path $packageRoot 'deploy\YOUTUBE_WORKER_SEQUENCE')
Copy-Item (Join-Path $repoRoot 'deploy\yt-dlp-public.key') (Join-Path $packageRoot 'deploy\yt-dlp-public.key')
Copy-Item (Join-Path $repoRoot 'deploy\third-party-versions.env') (Join-Path $packageRoot 'deploy\third-party-versions.env')
Copy-Item (Join-Path $repoRoot 'docs\ubuntu-deploy.md') (Join-Path $packageRoot 'docs\ubuntu-deploy.md')

$utf8WithoutBom = New-Object System.Text.UTF8Encoding($false)
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

$binaryHash = (Get-FileHash (Join-Path $packageRoot 'bin\wotoha-app') -Algorithm SHA256).Hash.ToLowerInvariant()
$workerHash = (Get-FileHash (Join-Path $packageRoot 'bin\wotoha-youtube-js-worker') -Algorithm SHA256).Hash.ToLowerInvariant()
$denoHash = (Get-FileHash (Join-Path $packageRoot 'third-party\deno') -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -Path (Join-Path $packageRoot 'SHA256SUMS.txt') -Value @(
    "$binaryHash  bin/wotoha-app"
    "$workerHash  bin/wotoha-youtube-js-worker"
    "$actualYtDlpHash  third-party/yt-dlp"
    "$denoHash  third-party/deno"
) -Encoding ascii
Set-Content -Path (Join-Path $packageRoot 'RELEASE_VERSION') -Value 'manual' -Encoding ascii

Remove-Item $archivePath -Force -ErrorAction SilentlyContinue
tar -czf $archivePath -C $distRoot 'wotoha-ubuntu-x86_64-musl'

Write-Output "binary: $(Join-Path $targetDir "$target\release\wotoha-app")"
Write-Output "worker: $(Join-Path $targetDir "$target\release\wotoha-youtube-js-worker")"
Write-Output "package: $packageRoot"
Write-Output "archive: $archivePath"
