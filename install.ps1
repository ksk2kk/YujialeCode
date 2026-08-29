# YujialeCode 一键安装（Windows，PowerShell 5.1+）
# 用法：
#   irm https://raw.githubusercontent.com/ksk2kk/YujialeCode/main/install.ps1 | iex
$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$RepoUrl = "https://github.com/ksk2kk/YujialeCode.git"
$ProtoVersion = "25.3"
$Home8 = [Environment]::GetFolderPath("UserProfile")

function Info($m)  { Write-Host "==> $m" -ForegroundColor Green }
function Warn($m)  { Write-Host "警告: $m" -ForegroundColor Yellow }
function Die($m)   { Write-Host "错误: $m" -ForegroundColor Red; exit 1 }

# ── 1) 定位仓库 ──────────────────────────────────────────────────────────
$RepoDir = Join-Path $Home8 "YujialeCode"
if (Test-Path (Join-Path $RepoDir "Cargo.toml")) {
    Info "仓库已存在：$RepoDir"
    git -C $RepoDir pull --ff-only 2>$null | Out-Null
} else {
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        Die "缺少 git。请先: winget install --id Git.Git -e --source winget"
    }
    Info "克隆仓库到 $RepoDir …"
    git clone --depth 1 $RepoUrl $RepoDir
}
Set-Location $RepoDir

# ── 2) MSVC 构建工具检测 ────────────────────────────────────────────────
$HasMsvc = Get-Command cl.exe -ErrorAction SilentlyContinue
if (-not $HasMsvc) {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $HasMsvc = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
    }
}
if (-not $HasMsvc) {
    Warn "未检测到 MSVC 编译器。没有它无法编译 Rust。"
    $reply = Read-Host "现在用 winget 安装 VS Build Tools？（约 2-5GB，y=安装 / n=跳过）"
    if ($reply -eq "y") {
        winget install --id Microsoft.VisualStudio.2022.BuildTools -e --source winget --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
        Warn "Build Tools 安装完成。请关闭本窗口，在『Developer PowerShell for VS』里重跑本脚本"
        exit 0
    } else {
        Die "缺少 MSVC，无法继续"
    }
}

# ── 3) Rust 工具链 ──────────────────────────────────────────────────────
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Info "未检测到 Rust，自动安装 stable（rustup）…"
    $tmp = New-TemporaryFile | Rename-Item -NewName { "rustup-init.exe" } -PassThru
    Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $tmp.FullName
    & $tmp.FullName -y --default-toolchain stable --profile minimal | Out-Null
    $env:PATH = "$Home8\.cargo\bin;$env:PATH"
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Die "Rust 安装失败" }
Info "Rust: $(cargo --version)"

# 确保 ~/.cargo/bin 在用户 PATH（rustup 一般会写；双保险）
$cargoBin = Join-Path $Home8 ".cargo\bin"
$userPath0 = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath0 -notlike "*$cargoBin*") -and (Test-Path $cargoBin)) {
    [Environment]::SetEnvironmentVariable("Path", "$cargoBin;$userPath0", "User")
    Warn "已把 $cargoBin 加入用户 PATH（新终端生效）"
}

# ── 4) protoc ────────────────────────────────────────────────────────────
$ProtoDir = Join-Path $Home8 ".local\bin"
$ProtoBin = Join-Path $ProtoDir "protoc.exe"
if (-not (Test-Path $ProtoBin)) {
    New-Item -ItemType Directory -Force -Path $ProtoDir | Out-Null
    Info "安装 protoc $ProtoVersion …"
    $zip = Join-Path $env:TEMP "protoc-win64.zip"
    Invoke-WebRequest -Uri "https://github.com/protocolbuffers/protobuf/releases/download/v$ProtoVersion/protoc-$ProtoVersion-win64.zip" -OutFile $zip
    Expand-Archive -Path $zip -DestinationPath "$env:TEMP\protoc-extract" -Force
    Copy-Item "$env:TEMP\protoc-extract\bin\protoc.exe" $ProtoBin -Force
}
$env:PATH = "$ProtoDir;$env:PATH"

# ── 5) 构建 ──────────────────────────────────────────────────────────────
Info "构建 yjlcoder（1/2）…"
cargo build --release
if ($LASTEXITCODE -ne 0) { Die "yjlcoder 构建失败" }

Info "构建 Grok UI（2/2，首次编译约 6-15 分钟）…"
Push-Location "vendor\grok-build"
cargo build --release -p xai-grok-pager-bin
Pop-Location
if ($LASTEXITCODE -ne 0) { Die "Grok UI 构建失败（缺 protoc？内存不足？）" }

# ── 6) 安装 ycode 命令（~/bin，自动追加用户 PATH）────────────────────────
$BinDir = Join-Path $Home8 "bin"
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$Tui = Join-Path $RepoDir "vendor\grok-build\target\release\xai-grok-pager.exe"
@"
@echo off
"$Tui" %*
"@ | Out-File -Encoding ascii (Join-Path $BinDir "ycode.cmd")
Copy-Item "target\release\yjlcoder.exe" (Join-Path $BinDir "yjlcoder.exe") -Force

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$BinDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$BinDir;$userPath", "User")
    Warn "已把 $BinDir 加入用户 PATH；新开终端后生效"
}

Info "安装完成！"
Write-Host "  启动: ycode（新开终端；Grok UI，首次进入自动弹配置向导）"
Write-Host "  QQ 桥接守护: yjlcoder --qq-only"
Write-Host "  旧 TUI: yjlcoder --legacy-tui"
Write-Host "  目录: $RepoDir"
