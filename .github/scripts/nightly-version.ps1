# 计算 nightly 版本号：<cargo 版本>-nightly.<yyyymmdd>.<当日第 n 次>
#
# 序号从远端已存在的同名 tag 推导，因此重跑同一分支不会覆盖历史构建；
# 并发由 workflow 的 concurrency 组挡在入口，这里不额外做加锁。
# 正式版本的 tag（v0.1.0）由 release-please 产生，前缀不同，不会互相干扰。
$ErrorActionPreference = 'Stop'

if (-not $env:GITHUB_OUTPUT) { throw 'GITHUB_OUTPUT is required' }
if (-not $env:GH_REPO) { throw 'GH_REPO is required' }

# 版本取 version.txt：那是 release-please 的更新目标，Cargo.toml 由 cargo-workspace 同步
$version = (Get-Content -Raw 'version.txt').Trim()
if ($version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw 'version.txt does not declare a semver version' }

# 北京时间：构建编号对使用者而言是"今天第几次"，用 UTC 会在晚上 8 点后串到次日
$today = [DateTime]::UtcNow.AddHours(8).ToString(
    'yyyyMMdd',
    [System.Globalization.CultureInfo]::InvariantCulture
)
$prefix = "$version-nightly.$today."

$refs = gh api --paginate "repos/$env:GH_REPO/git/matching-refs/tags/$prefix" --jq '.[].ref'
if ($LASTEXITCODE -ne 0) { throw 'Cannot list existing nightly tags' }

$highest = 0
foreach ($ref in @($refs)) {
    if ($ref -match ('^refs/tags/' + [regex]::Escape($prefix) + '(\d+)$')) {
        $sequence = [int]$Matches[1]
        if ($sequence -gt $highest) { $highest = $sequence }
    }
}

$package = "$prefix$($highest + 1)"

"version=$version" >> $env:GITHUB_OUTPUT
"package=$package" >> $env:GITHUB_OUTPUT
"date=$today" >> $env:GITHUB_OUTPUT

Write-Host "nightly version: $package (base $version, build $($highest + 1) on $today)"
