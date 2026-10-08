# 把 iPlayer 打成一个可解压即用的 Windows 便携目录 / zip。
#
#   pwsh scripts/bundle-windows.ps1            # 产出 dist\iPlayer-win64\
#   pwsh scripts/bundle-windows.ps1 -Zip       # 再打一个 zip
#   pwsh scripts/bundle-windows.ps1 -Zip -Run  # 打完直接启动
#
# 只依赖 cargo 与系统自带的 Compress-Archive；产物是便携目录，不做系统级安装。
# 需要预先准备好 ffmpeg / ffprobe（见 README 快速开发一节）。

param(
    [switch]$Zip,
    [switch]$Run
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $Root

$AppName = "iPlayer"
$Version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"(.*)"').Matches[0].Groups[1].Value
$Triple = (& rustc -vV | Select-String '^host:\s*(\S+)').Matches[0].Groups[1].Value
$Dist = Join-Path $Root "dist"
$Out = Join-Path $Dist "$AppName-win64"

Write-Host "==> 1/4 编译 release（$Triple）"
cargo build --release

Write-Host "==> 2/4 摆好便携目录"
if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
New-Item -ItemType Directory -Path $Out -Force | Out-Null

Copy-Item "target\release\iplayer.exe" (Join-Path $Out "$AppName.exe")

# ffmpeg / ffprobe 放在主程序同级 —— sidecar 查找顺序的第一站。
# 注意必须带 .exe，运行时按 <工具名>-<三元组>.exe 与 <工具名>.exe 依次找。
$Missing = $false
foreach ($tool in @("ffmpeg", "ffprobe")) {
    $src = $null
    foreach ($cand in @("binaries\$tool-$Triple.exe", "binaries\$tool.exe")) {
        if (Test-Path $cand) { $src = $cand; break }
    }
    if ($src) {
        Copy-Item $src (Join-Path $Out "$tool.exe")
        $mb = [math]::Round((Get-Item $src).Length / 1MB, 1)
        Write-Host "    $tool  -> $tool.exe  ($mb MB)"
    } else {
        $Missing = $true
        Write-Warning "找不到 $tool（期望 binaries\$tool-$Triple.exe），装出来的目录没有播放能力"
    }
}

if (Test-Path "icons\icon.ico") {
    Copy-Item "icons\icon.ico" (Join-Path $Out "icon.ico")
} else {
    Write-Warning "没有 icons\icon.ico，快捷方式会显示成默认图标"
}

Write-Host "==> 3/4 写版本说明与启动脚本"
@"
iPlayer $Version (win64, $Triple)

直接运行：  $($AppName).exe
创建桌面快捷方式：右键 $($AppName).exe -> 发送到 -> 桌面快捷方式
"@ | Set-Content -Encoding UTF8 (Join-Path $Out "README.txt")

Write-Host "==> 4/4 收尾"
if ($Zip) {
    $zipPath = Join-Path $Dist "$AppName-$Version-win64.zip"
    if (Test-Path $zipPath) { Remove-Item -Force $zipPath }
    Compress-Archive -Path $Out -DestinationPath $zipPath
    Write-Host "    zip -> $zipPath"
}

$size = [math]::Round(((Get-ChildItem $Out -Recurse | Measure-Object Length -Sum).Sum / 1MB), 1)
Write-Host ""
Write-Host "完成：$Out  ($size MB)"
if ($Missing) { Write-Warning "缺少 ffmpeg/ffprobe，装出来的目录没有播放能力。" }
Write-Host "首次运行若被 SmartScreen 拦下：更多信息 -> 仍要运行。"

if ($Run) { Start-Process (Join-Path $Out "$AppName.exe") }
