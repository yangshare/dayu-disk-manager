@echo off
setlocal enabledelayedexpansion

echo ====================================
echo dayu-disk-manager - Release Script
echo 推送 Tag 后 GitHub Actions 会自动构建 Windows 安装包并发布到 Releases
echo ====================================
echo.

set /p version="请输入版本号 (例如 0.1.0): "

if "%version%"=="" (
    echo 错误：版本号不能为空
    pause
    exit /b 1
)

echo.
echo 提示：请确认已在以下文件中更新版本号为 %version%：
echo   - src-tauri/tauri.conf.json  (^"version^" 字段)
echo   - src-tauri/Cargo.toml       (version 字段)
echo   - package.json               (version 字段)
echo 并已 git add ^& git commit 这些改动。
echo.

set /p confirm=确认要发布 v%version% 吗？(y/N):
if /i not "%confirm%"=="y" (
    echo 已取消
    pause
    exit /b 0
)

echo.
git tag v%version%
if errorlevel 1 (
    echo 错误：创建 Git Tag 失败（可能 v%version% 已存在）
    pause
    exit /b 1
)

echo Git Tag v%version% 创建成功
echo.

git push origin v%version%
if errorlevel 1 (
    echo 错误：推送 Tag 到远程仓库失败
    pause
    exit /b 1
)

echo.
echo ====================================
echo 发布已触发！版本 v%version% 已推送到远程仓库
echo GitHub Actions 将自动构建并发布到 Releases
echo 查看构建进度：Actions 标签页
echo ====================================
pause
