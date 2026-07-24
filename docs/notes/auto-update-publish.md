# 自动更新发布手册

> 适用于大禹磁盘管理器（Tauri 2）的自动更新 + 七牛托管流程。

## 一次性准备

### 1. 生成 Tauri 更新签名密钥

```bash
pnpm tauri signer generate -w ~/.tauri/dayu-disk-manager.key
```

- 私钥文件 `~/.tauri/dayu-disk-manager.key`（生成时可设密码）→ **绝不进仓库**
- 公钥文件 `~/.tauri/dayu-disk-manager.key.pub`（公开，可分享）

### 2. 填写本地配置

- `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey` ← 粘贴 `.key.pub` 全文
- `src-tauri/tauri.conf.json` 的 `plugins.updater.endpoints` ← 把 `<替换为你的七牛域名>` 换成真实域名
- 本地手测：`cp .qiniu.local.json.example .qiniu.local.json` 并按注释填好

> `latest.json` 中的下载 `url` 由 `scripts/upload-qiniu.js` 运行时用 `QINIU_BUCKET_DOMAIN` 自动拼接，无需手动填写。

### 3. 配置 GitHub Secrets

| Secret | 值 |
|--------|----|
| `TAURI_SIGNING_PRIVATE_KEY` | `.key` 文件内容 |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 生成时设置的密码（未设则留空） |
| `QINIU_ACCESS_KEY` / `QINIU_SECRET_KEY` | 七牛密钥 |
| `QINIU_BUCKET` | 七牛空间名 |
| `QINIU_BUCKET_DOMAIN` | 七牛域名（不带 `https://`） |
| `QINIU_ZONE` | 存储区域，如 `z2`（华南）、`z0`（华东）、`z1`（华北） |
| `QINIU_ACCELERATE_UPLOADING` | 可选；仅已开通七牛传输加速时设为 `true`，默认关闭 |

用于上传的七牛密钥还必须具备该域名的 CDN URL 刷新权限。每次发布完成清单上传后，脚本会刷新 `latest.json`，避免 CDN 的长缓存把旧版本清单返回给客户端。

## 发布流程

1. 三处版本号保持一致：`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`package.json`
2. 提交改动后打 tag 并推送：
   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
   如需在 Actions 页面重新发布，必须填写一个已存在的同名 tag。
3. GitHub Actions 自动执行：
   - `tauri-action` 用 `TAURI_SIGNING_PRIVATE_KEY` 构建 NSIS 包并生成 `.sig`
   - 上传 `.msi` / `.exe` / `.sig` 到 GitHub Release（手动下载备用）
   - `scripts/upload-qiniu.js` 上传 `*-setup.exe` / `.sig` / `latest.json` 到七牛固定路径 `dayu-disk-manager/win/x64/`

## 端到端验证

1. **本地构建签名产物**
   设置环境变量后构建，确认签名文件生成：
   ```bash
   # PowerShell
   $env:TAURI_SIGNING_PRIVATE_KEY = Get-Content ~/.tauri/dayu-disk-manager.key -Raw
   $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = "你的密码"
   pnpm tauri build
   ```
   预期 `src-tauri/target/release/bundle/nsis/` 下同时存在 `dayu-disk-manager_<ver>_x64-setup.exe` 与 `dayu-disk-manager_<ver>_x64-setup.exe.sig`。

2. **本地跑上传脚本**
   ```bash
   node scripts/upload-qiniu.js <版本> src-tauri/target/release/bundle
   ```
   预期上传 exe / sig / latest.json 后输出「已刷新 CDN 缓存」，七牛空间出现对应文件。

3. **清单可达**
   浏览器打开 `tauri.conf.json` 中配置的 endpoint URL，应返回 200，JSON 含 `platforms.windows-x86_64.signature` 与 `.url`。

4. **App 更新闭环**
   - 安装旧版本
   - 发布新版本（推 tag），等待 CI 上传七牛完成
   - 重启旧版 App → 启动静默检查 → 弹「发现新版本」对话框 → 点「立即更新」
   - 下载安装完成后自动重启，版本号更新为新版

## 故障排查

- **CI 未生成 `.sig`**：检查 `TAURI_SIGNING_PRIVATE_KEY` Secret 是否设置、密码是否匹配；`tauri.conf.json` 的 `bundle.createUpdaterArtifacts` 是否为 `true`。
- **App 检查不到更新**：确认 endpoint 域名已替换占位符、`latest.json` 的 `version` 高于当前版本、`pubkey` 与签名私钥是同一密钥对。
- **七牛上传报 zone 错误**：核对 `QINIU_ZONE` / `.qiniu.local.json` 的 zone 值在 `z0/z1/z2/na0/as0` 之内。
- **CDN 刷新失败**：确认七牛密钥有该域名的 URL 刷新权限；此错误会中止发布，避免客户端持续读取旧清单。
