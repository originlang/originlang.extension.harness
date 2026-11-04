param(
    [Parameter(Mandatory = $true)][string]$Target,
    [ValidateSet('stable', 'nightly')][string]$Channel = 'stable'
)
$ErrorActionPreference = 'Stop'
# 打包是纯文件操作，进度条只会把 CI 日志刷满
$ProgressPreference = 'SilentlyContinue'

# 版本以工作区根为准：version.txt 是 release-please 的更新目标，Cargo.toml 由
# cargo-workspace 插件同步；两者不一致说明版本没跟到位，宁可失败也不要发错号。
$fileVersion = (Get-Content -Raw 'version.txt').Trim()
$cargoVersion = [regex]::Match(
    (Get-Content -Raw 'Cargo.toml'),
    '(?m)^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"'
).Groups[1].Value
if (-not $fileVersion -or -not $cargoVersion) { throw 'Missing version in version.txt or Cargo.toml' }
if ($fileVersion -ne $cargoVersion) {
    throw "version.txt ($fileVersion) and Cargo.toml ($cargoVersion) disagree"
}

if ($Channel -eq 'nightly') {
    if (-not $env:NIGHTLY_VERSION) { throw 'NIGHTLY_VERSION is required for nightly packages' }
    $version = $env:NIGHTLY_VERSION
} else {
    $version = $cargoVersion
}

$name = "harness-$version-$Target"
$stage = Join-Path 'target/package' $name
Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $stage -Force | Out-Null
New-Item -ItemType Directory -Path 'dist' -Force | Out-Null

# 动态库文件名由平台约定决定，包清单的 main 必须与之一致，否则宿主解析不到入口
if ($Target -like '*windows*') {
    $library = 'harness.dll'
} elseif ($Target -like '*apple*') {
    $library = 'libharness.dylib'
} else {
    $library = 'libharness.so'
}
$built = "target/$Target/release/$library"
if (-not (Test-Path -LiteralPath $built)) { throw "Plugin library was not built: $built" }

# 清单随包发布：版本号写包版本，宿主据此区分同一分支的多次构建
$manifest = Get-Content -Raw 'examples/harness/extension.json' | ConvertFrom-Json
$manifest.version = $version
$manifest.main = $library
[System.IO.File]::WriteAllText(
    (Join-Path $stage 'extension.json'),
    ($manifest | ConvertTo-Json -Depth 10) + "`n"
)
Copy-Item -LiteralPath $built -Destination $stage

# 包里的版本号与 crate 版本不同，把两者的对应关系留在包内
$buildInfo = [ordered]@{
    channel   = $Channel
    version   = $version
    cargo     = $cargoVersion
    commit    = $env:GITHUB_SHA
    runId     = $env:GITHUB_RUN_ID
    target    = $Target
} | ConvertTo-Json
[System.IO.File]::WriteAllText((Join-Path $stage 'build-info.json'), $buildInfo)

if ($Target -like '*windows*') {
    $archive = "dist/$name.zip"
    Compress-Archive -Path $stage -DestinationPath $archive -Force
} else {
    $archive = "dist/$name.tar.gz"
    tar -czf $archive -C 'target/package' $name
    if ($LASTEXITCODE -ne 0) { throw 'Archive creation failed' }
}

$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
[System.IO.File]::WriteAllText("$archive.sha256", "$hash  $(Split-Path -Leaf $archive)`n")

Write-Host "packaged $Channel $version -> $archive"
