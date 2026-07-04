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

$env:PATH = @(
    (Split-Path -Parent $zigPath),
    (Split-Path -Parent $cmakePath),
    (Split-Path -Parent $ninjaPath),
    $env:PATH
) -join ';'

$env:CMAKE_GENERATOR = 'Ninja'

rustup target add $target

cargo zigbuild --release --bin wotoha-app --target $target --target-dir $targetDir

Remove-Item $packageRoot -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $packageRoot | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $packageRoot 'bin') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $packageRoot 'deploy') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $packageRoot 'docs') | Out-Null

Copy-Item (Join-Path $targetDir "$target\release\wotoha-app") (Join-Path $packageRoot 'bin\wotoha-app')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha.service') (Join-Path $packageRoot 'deploy\wotoha.service')
Copy-Item (Join-Path $repoRoot 'deploy\install-ubuntu.sh') (Join-Path $packageRoot 'install-ubuntu.sh')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha-update.sh') (Join-Path $packageRoot 'wotoha-update.sh')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha.env.example') (Join-Path $packageRoot 'deploy\wotoha.env.example')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha-update.env.example') (Join-Path $packageRoot 'deploy\wotoha-update.env.example')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha-update.service') (Join-Path $packageRoot 'deploy\wotoha-update.service')
Copy-Item (Join-Path $repoRoot 'deploy\wotoha-update.timer') (Join-Path $packageRoot 'deploy\wotoha-update.timer')
Copy-Item (Join-Path $repoRoot 'docs\ubuntu-deploy.md') (Join-Path $packageRoot 'docs\ubuntu-deploy.md')

$binaryHash = (Get-FileHash (Join-Path $packageRoot 'bin\wotoha-app') -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -Path (Join-Path $packageRoot 'SHA256SUMS.txt') -Value "$binaryHash  bin/wotoha-app"
Set-Content -Path (Join-Path $packageRoot 'RELEASE_VERSION') -Value 'manual'

Remove-Item $archivePath -Force -ErrorAction SilentlyContinue
tar -czf $archivePath -C $distRoot 'wotoha-ubuntu-x86_64-musl'

Write-Output "binary: $(Join-Path $targetDir "$target\release\wotoha-app")"
Write-Output "package: $packageRoot"
Write-Output "archive: $archivePath"
