# 大禹磁盘管理器 · 自动更新（Tauri 2 + 七牛托管）设计

- 日期：2026-07-24
- 参考项目：`E:\开源项目\百度网盘批量转存\溜溜网盘-BMAD\LiuliuCloudStorage`（Electron + electron-builder + 七牛）
- 本项目技术栈：Tauri 2（Rust）+ Vue 3 + pnpm

## Context（背景与目标）

国内网络访问 GitHub 不稳定，现有 `release.yml` 只把安装包发到 GitHub Release，已安装的用户无法就地更新，新用户下载也常失败。本设计为项目接入**自动更新**能力，并把更新分发的所有产物（安装包、签名、版本清单）托管到**七牛云**，彻底绕开 GitHub 做更新检查与下载。GitHub Release 仅保留为手动下载的备用渠道。

参考项目是 Electron，其七牛上传脚本面向 electron-builder 产物（`*-Setup.exe` + `*.blockmap` + `latest.yml`）；本项目是 Tauri 2，更新产物与签名机制完全不同（`*_x64-setup.exe` + `.sig` + `latest.json`，私钥签名/公钥校验），因此不能照搬参考脚本，需按 Tauri 规范改造。

## 已确认的关键决策

| 决策点 | 选择 |
|--------|------|
| 更新源（endpoint） | 完全用七牛，不回退 GitHub |
| 自动更新安装包格式 | NSIS `.exe`（msi 仍构建并挂 Release，仅作手动下载备用） |
| 更新 UI | Tauri 原生流程（`@tauri-apps/plugin-updater`，`downloadAndInstall` 触发 nsis 安装器，不自绘进度条） |
| `latest.json` 生成方式 | 脚本本地组装（读 `.sig` 内容，按 Tauri 规范拼 JSON，url 写死七牛直链） |
| 前端检查时机 | App 启动静默检查一次 + 设置页「检查更新」按钮 |
| 签名密钥 | 用户本地 `tauri signer generate` 生成；公钥入 `tauri.conf.json`，私钥入 GitHub Secret |

## 数据流

```
git tag v0.2.0 → GitHub Actions
   ├─ tauri-action 构建 nsis .exe（TAURI_SIGNING_PRIVATE_KEY 签名 → 生成 .sig）
   ├─ 发 GitHub Release（保留，手动下载备用）
   ├─ node scripts/upload-qiniu.js "<版本>" "<bundle 目录>"
   │     ├─ 上传 dayu-disk-manager_<ver>_x64-setup.exe      → 七牛 .../win/x64/
   │     ├─ 上传 dayu-disk-manager_<ver>_x64-setup.exe.sig  → 七牛 .../win/x64/
   │     └─ 组装并上传 latest.json（url 指向七牛 .exe）     → 七牛 .../win/x64/
   └─ 完成

App 启动 → GET https://<七牛域名>/dayu-disk-manager/win/x64/latest.json
   → 比版本 → 下载 .exe → 用公钥校验 .sig → 弹 nsis 安装器 → 重启
```

## 改动清单

### 1. Tauri 后端：启用 updater 能力

- `src-tauri/Cargo.toml`：`[dependencies]` 增加
  ```toml
  tauri-plugin-updater = "2"
  ```
- `src-tauri/src/lib.rs`：在 `run()` 内 `tauri::Builder` 链上注册插件（与现有 `.plugin(...)`/`.manage(state)` 同级）
  ```rust
  .plugin(tauri_plugin_updater::Builder::new().build())
  ```
- `src-tauri/tauri.conf.json`：
  - `bundle` 内增加 `"createUpdaterArtifacts": true`
  - 顶层增加
    ```json
    "plugins": {
      "updater": {
        "pubkey": "<待填：.key.pub 内容>",
        "endpoints": ["https://<待填：七牛域名>/dayu-disk-manager/win/x64/latest.json"],
        "windows": { "installMode": "passive" }
      }
    }
    ```
- `src-tauri/capabilities/default.json`：`permissions` 数组增加
  - `"updater:default"`（检查与下载安装的基础能力，必加）
  - `"dialog:default"`、`"dialog:allow-ask"`（前端启动时弹"是否更新"询问框，必加）
  - `"process:allow-restart"`（安装完成后 `relaunch()` 重启 App，必加）
  - 三项一次性补齐，避免"先缺后补"的来回。

### 2. 前端：检查更新 UI

- `package.json`：`dependencies` 增加 `@tauri-apps/plugin-updater`、`@tauri-apps/plugin-dialog`
- 新建 `src/composables/useUpdater.ts`：封装
  ```ts
  import { check } from '@tauri-apps/plugin-updater'
  import { ask, message } from '@tauri-apps/plugin-dialog'

  // silent=true：启动静默检查，无更新不提示；silent=false：设置页手动触发，任何状态都给反馈
  export async function checkForUpdates(silent: boolean) { /* check → ask 是否更新 → downloadAndInstall → relaunch */ }
  ```
  - 下载进度：`update.downloadAndInstall(onEvent)` 回调可读 progress；本设计不自绘进度条，依赖 nsis 安装器（passive 模式自带进度窗）。如后续要前端进度条，再扩展。
- `src/App.vue`：`onMounted` 调一次 `checkForUpdates(true)`（容错：检查失败静默吞掉，绝不阻塞主界面）
- `src/views/SettingsView.vue`：新增「检查更新」`settings-section`（沿用现有三栏 grid 与 `settings-icon`/`button` 样式），点击调 `checkForUpdates(false)`，结果写进现有 `error`/`saved` 风格的反馈区

### 3. 七牛上传脚本（新建 `scripts/upload-qiniu.js`）

复用参考项目 `LiuliuCloudStorage/scripts/upload-qiniu.js` 的成熟实现：
- 配置加载（`.qiniu.local.json` 本地优先 / `QINIU_*` 环境变量 CI 兜底）
- `ZONE_MAP`、分片 `ResumeUploader`、`accelerateUploading`、进度上报、并发上传二进制 + 最后传清单的顺序

**改造点**（面向 Tauri 产物）：
- 上传文件集合：`<productName>_<ver>_x64-setup.exe` + `<productName>_<ver>_x64-setup.exe.sig` + 自组装 `latest.json`
  - `productName` = `dayu-disk-manager`（来自 `tauri.conf.json`）
  - 从 bundle 目录递归查找 `nsis` 子目录下的 `*-setup.exe` 与同名 `.sig`（参考脚本的 `findFile` 支持子目录嵌套）
- `REMOTE_PREFIX = 'dayu-disk-manager/win/x64'`
- **自己组装 `latest.json`**（决策 B），字段遵循 Tauri 2 规范：
  ```json
  {
    "version": "<ver>",
    "notes": "大禹磁盘管理器 <ver>",
    "pub_date": "<ISO8601 构建时间>",
    "platforms": {
      "windows-x86_64": {
        "signature": "<.sig 文件文本内容>",
        "url": "https://<七牛域名>/dayu-disk-manager/win/x64/<exe 文件名>"
      }
    }
  }
  ```
  - 必填字段仅 `version`、`platforms.<target>.url`、`platforms.<target>.signature`，其余可选
- **`latest.json` 的 `url` 字段由脚本运行时从 `QINIU_BUCKET_DOMAIN` 自动拼接**（`https://${bucketDomain}/${REMOTE_PREFIX}/${exeName}`），不写入仓库、不需人工填——与参考项目一致
- 上传顺序：先并发传 `.exe` + `.sig`，**最后**传 `latest.json`（保证清单指向的文件已就位，避免用户拉到清单但下不到包）
- 新增 devDependency：`qiniu`

### 4. `release.yml` 改造（`.github/workflows/release.yml`）

- `tauri-apps/tauri-action@v0` 步骤：
  - `env` 增补 `TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}`
  - `with` 增补 `updaterJsonPreferNsis: true`
- 在 tauri-action 之后新增「上传七牛」步骤：
  ```yaml
  - name: Upload to Qiniu
    shell: bash
    run: node scripts/upload-qiniu.js "$APP_VERSION" src-tauri/target/x86_64-pc-windows-msvc/release/bundle
    env:
      QINIU_ACCESS_KEY: ${{ secrets.QINIU_ACCESS_KEY }}
      QINIU_SECRET_KEY: ${{ secrets.QINIU_SECRET_KEY }}
      QINIU_BUCKET: ${{ secrets.QINIU_BUCKET }}
      QINIU_BUCKET_DOMAIN: ${{ secrets.QINIU_BUCKET_DOMAIN }}
      QINIU_ZONE: ${{ secrets.QINIU_ZONE }}
  ```
- GitHub Release 上传由 tauri-action 原样保留（手动下载备用）

### 5. 签名密钥（用户本地生成）

用户在本地执行：
```bash
pnpm tauri signer generate -w ~/.tauri/dayu-disk-manager.key
```
- `.key`（私钥，配密码）→ 存 GitHub Secrets `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`，**绝不进仓库**
- `.key.pub`（公钥）→ 用户提供后写入 `tauri.conf.json` 的 `pubkey`

## GitHub Secrets 清单

`TAURI_SIGNING_PRIVATE_KEY`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`、`QINIU_ACCESS_KEY`、`QINIU_SECRET_KEY`、`QINIU_BUCKET`、`QINIU_BUCKET_DOMAIN`、`QINIU_ZONE`

## 用户自助配置项（对齐参考项目：占位符 + Secrets，无需向开发流程提供任何信息）

实现会在代码里留好占位符和 Secret 引用，用户按下表自助填好即可发布，不需要把任何值发给开发流程：

| 配置项 | 填在哪里 | 性质 | 说明 |
|--------|----------|------|------|
| 七牛 AK / SK / bucket / domain / zone | GitHub Secrets（CI）+ 本地 `.qiniu.local.json`（手测） | 敏感，绝不进仓库 | 与参考项目 `release.yml` 118–129 完全一致，运行时注入 |
| 七牛域名 | `tauri.conf.json` 的 `endpoints` 占位符 | 非敏感（公开访问域名） | 发布前把 `https://<七牛域名>/...` 里的占位符替换为自己的域名。Tauri 要求编译期确定 endpoint，故必须明文入配置；参考项目（electron）无此约束 |
| 七牛存储区域 | 本地 `.qiniu.local.json` + CI `QINIU_ZONE` Secret | 非敏感 | 脚本运行时读取，决定上传 zone |
| Tauri 更新公钥 | `tauri.conf.json` 的 `pubkey` 占位符 | 公开（本就可分享） | 用户本地 `pnpm tauri signer generate` 后自行粘贴；私钥仅入 GitHub Secret。Tauri 独有，electron 无此物 |

> 关键：`latest.json` 中的下载 `url` 由上传脚本运行时用 `QINIU_BUCKET_DOMAIN` 自动拼接，不在仓库出现、不需人工填写——与参考项目一致。

## 验证方式（端到端）

1. **本地构建签名产物**：设置 `TAURI_SIGNING_PRIVATE_KEY` 环境变量后 `pnpm tauri build`，确认 `src-tauri/target/.../bundle/nsis/` 下生成 `*_x64-setup.exe` 与 `.sig`
2. **本地跑上传脚本**：填好 `.qiniu.local.json` 后 `node scripts/upload-qiniu.js <ver> <bundle目录>`，确认七牛空间出现 exe/sig/latest.json
3. **手测 latest.json 可达**：浏览器 `GET <endpoint>` 返回 200 且 JSON 字段正确；`.sig`/exe 直链可下载
4. **CI 全流程**：推一个测试 tag（如 `v0.1.0-test`），观察 Actions：构建→签名→发 Release→上传七牛全绿
5. **App 端更新闭环**：装旧版 → 推新版 tag → 七牛 latest.json 更新 → 重启旧版 App，启动静默检查弹 dialog → 同意 → 下载安装 → 重启后版本号更新
6. **前端单元测试**：`useUpdater` 用 vitest mock `@tauri-apps/plugin-updater`，覆盖「无更新/有更新/检查失败」三条路径（沿用项目现有 vitest 模式，参考 `ScanView.test.ts`）

## 范围与非目标（YAGNI）

- 不做 macOS/Linux 构建（现有 workflow 仅 Windows，保持不变）
- 不做多架构（仅 x86_64）
- 不自绘下载进度条（依赖 nsis passive 安装器）
- 不做后台定时轮询（仅启动时一次 + 手动按钮）
- 不做灰度/分批发布（latest.json 为静态全量）
