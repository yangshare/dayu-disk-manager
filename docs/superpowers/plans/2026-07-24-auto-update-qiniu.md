# 自动更新 + 七牛托管 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 为大禹磁盘管理器（Tauri 2）接入自动更新，更新分发产物（NSIS exe / .sig / latest.json）全部托管七牛，绕开国内 GitHub 不稳定问题。

**架构：** tag 推送 → `tauri-action` 用 `TAURI_SIGNING_PRIVATE_KEY` 构建签名 nsis 包 → `scripts/upload-qiniu.js` 上传 exe/.sig 并本地组装 `latest.json`（url 由 `QINIU_BUCKET_DOMAIN` 运行时拼）到七牛固定路径 → App 启动时静默 `check()` endpoint，有新版弹原生 dialog 引导 `downloadAndInstall`。GitHub Release 仅作手动下载备用。

**技术栈：** Tauri 2（Rust）+ Vue 3 + TS + pnpm + vitest；`tauri-plugin-updater`（Rust）+ `@tauri-apps/plugin-updater`/`plugin-dialog`（前端）；`qiniu` SDK（Node ESM 脚本）；GitHub Actions。

**设计依据：** `docs/superpowers/specs/2026-07-24-auto-update-qiniu-design.md`（commit b4ab1bc）。参考项目：`E:\开源项目\百度网盘批量转存\溜溜网盘-BMAD\LiuliuCloudStorage`（Electron 版七牛脚本，本项目按 Tauri 产物改造）。

---

## 文件结构

| 文件 | 操作 | 职责 |
|------|------|------|
| `src-tauri/Cargo.toml` | 修改 | 加 `tauri-plugin-updater` 依赖 |
| `src-tauri/src/lib.rs` | 修改 | Builder 链注册 updater 插件 |
| `src-tauri/tauri.conf.json` | 修改 | `createUpdaterArtifacts` + `plugins.updater`（pubkey/endpoints 占位符） |
| `src-tauri/capabilities/default.json` | 修改 | 加 updater/dialog/process 权限 |
| `scripts/upload-qiniu.js` | 创建 | ESM 脚本：找产物→读 .sig→组装 latest.json→分片上传七牛 |
| `scripts/upload-qiniu.test.ts` | 创建 | 纯函数单测（latest.json 组装、url 拼接、产物查找） |
| `.qiniu.local.json.example` | 创建 | 本地七牛配置模板（真实文件 gitignore） |
| `.gitignore` | 修改 | 忽略 `.qiniu.local.json` |
| `package.json` | 修改 | 加 `qiniu`(dev)、`@tauri-apps/plugin-updater`/`plugin-dialog`(dep) |
| `.github/workflows/release.yml` | 修改 | 注入签名密钥 env + 七牛上传步骤 |
| `src/composables/useUpdater.ts` | 创建 | 封装 check→ask→downloadAndInstall→relaunch |
| `src/composables/useUpdater.test.ts` | 创建 | 三路径单测（无更新/有更新/失败） |
| `src/App.vue` | 修改 | `onMounted` 启动静默检查 |
| `src/views/SettingsView.vue` | 修改 | 「检查更新」卡片 |
| `docs/notes/auto-update-publish.md` | 创建 | 发布手册：密钥生成、Secrets、端到端验证 |

---

## 任务 1：Tauri 启用 updater 能力

**文件：**
- 修改：`src-tauri/Cargo.toml`（`[dependencies]` 段）
- 修改：`src-tauri/src/lib.rs`（Builder 链，`.invoke_handler(...)` 与 `.run(...)` 之间）
- 修改：`src-tauri/tauri.conf.json`
- 修改：`src-tauri/capabilities/default.json`

配置类改动无单元测试，靠构建/类型检查验证。

- [ ] **步骤 1：加 Rust 依赖**

`src-tauri/Cargo.toml` 的 `[dependencies]` 段（`tauri = ...` 那一行下方）加：

```toml
tauri-plugin-updater = "2"
```

- [ ] **步骤 2：注册插件**

`src-tauri/src/lib.rs` 的 Builder 链，在 `.invoke_handler(tauri::generate_handler![...])` 调用之后、`.run(tauri::generate_context!())` 之前插入一行：

```rust
        .plugin(tauri_plugin_updater::Builder::new().build())
```

- [ ] **步骤 3：改 tauri.conf.json**

`src-tauri/tauri.conf.json`：
- `"bundle"` 对象内新增 `"createUpdaterArtifacts": true`（紧跟 `"active": true,` 之后）
- 文件末尾 `"bundle": {...}` 同级追加顶层 `"plugins"` 块。最终尾部结构：

```json
  "bundle": {
    "active": true,
    "createUpdaterArtifacts": true,
    "targets": ["msi", "nsis"],
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.icns",
      "icons/icon.ico"
    ],
    "windows": {
      "webviewInstallMode": {
        "type": "downloadBootstrapper"
      }
    }
  },
  "plugins": {
    "updater": {
      "pubkey": "<替换为 tauri signer generate 生成的 .key.pub 内容>",
      "endpoints": [
        "https://<替换为你的七牛域名>/dayu-disk-manager/win/x64/latest.json"
      ],
      "windows": {
        "installMode": "passive"
      }
    }
  }
}
```

> 占位符说明：`pubkey` 与 `endpoints` 里的域名在发布前由用户自行替换（见任务 7）。`pubkey` 公开可分享，域名非敏感。此步先用占位符保证配置结构合法。

- [ ] **步骤 4：加权限**

`src-tauri/capabilities/default.json` 的 `permissions` 数组改为：

```json
  "permissions": [
    "core:default",
    "core:window:allow-close",
    "core:window:allow-minimize",
    "core:window:allow-toggle-maximize",
    "core:window:allow-start-dragging",
    "updater:default",
    "dialog:default",
    "dialog:allow-ask",
    "process:allow-restart"
  ]
```

- [ ] **步骤 5：验证配置编译通过**

运行（在项目根目录）：
```
pnpm tauri build --debug --no-bundle
```
预期：Rust 编译通过（首次会拉取 `tauri-plugin-updater` crate）。`--no-bundle` 跳过打包，只验证配置与编译。若报权限/pubkey 相关错误， pubkey 占位符不影响编译（仅在真正构建签名产物时校验）。

- [ ] **步骤 6：Commit**

```
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/lib.rs src-tauri/tauri.conf.json src-tauri/capabilities/default.json
git commit -m "feat(updater): 启用 Tauri updater 能力与权限配置"
```

---

## 任务 2：七牛上传脚本（TDD）

**文件：**
- 创建：`scripts/upload-qiniu.js`
- 创建：`scripts/upload-qiniu.test.ts`
- 创建：`.qiniu.local.json.example`
- 修改：`.gitignore`、`package.json`

先写可测的纯函数（latest.json 组装、url 拼接、bundle 产物查找），再写上传主流程。

- [ ] **步骤 1：写纯函数的失败测试**

创建 `scripts/upload-qiniu.test.ts`：

```ts
import { describe, expect, it } from 'vitest'
import {
  assembleLatestJson,
  buildQiniuUrl,
  matchNsisArtifacts,
} from './upload-qiniu.js'

describe('assembleLatestJson', () => {
  it('按 Tauri 规范组装 windows-x86_64 条目', () => {
    const json = assembleLatestJson({
      version: '0.2.0',
      notes: '大禹磁盘管理器 0.2.0',
      pubDate: '2026-07-24T10:00:00Z',
      exeFileName: 'dayu-disk-manager_0.2.0_x64-setup.exe',
      signature: 'RW1wdHkgc2lnbmF0dXJl',
      bucketDomain: 'qiniu.example.com',
    })
    const parsed = JSON.parse(json)
    expect(parsed.version).toBe('0.2.0')
    expect(parsed.notes).toBe('大禹磁盘管理器 0.2.0')
    expect(parsed.pub_date).toBe('2026-07-24T10:00:00Z')
    expect(parsed.platforms['windows-x86_64'].signature).toBe('RW1wdHkgc2lnbmF0dXJl')
    expect(parsed.platforms['windows-x86_64'].url)
      .toBe('https://qiniu.example.com/dayu-disk-manager/win/x64/dayu-disk-manager_0.2.0_x64-setup.exe')
  })
})

describe('buildQiniuUrl', () => {
  it('拼接 https + 域名 + 前缀 + 文件名', () => {
    expect(buildQiniuUrl('qiniu.example.com', 'dayu-disk-manager/win/x64', 'a.exe'))
      .toBe('https://qiniu.example.com/dayu-disk-manager/win/x64/a.exe')
  })
  it('去掉域名前导 https://', () => {
    expect(buildQiniuUrl('https://qiniu.example.com', 'p', 'a.exe'))
      .toBe('https://qiniu.example.com/p/a.exe')
  })
})

describe('matchNsisArtifacts', () => {
  it('从文件名列表中挑出 setup.exe 与同名 .sig', () => {
    const files = matchNsisArtifacts([
      'dayu-disk-manager_0.2.0_x64-setup.exe',
      'dayu-disk-manager_0.2.0_x64-setup.exe.sig',
      'dayu-disk-manager_0.2.0_x64_en-US.msi',
      'readme.txt',
    ])
    expect(files.exe).toBe('dayu-disk-manager_0.2.0_x64-setup.exe')
    expect(files.sig).toBe('dayu-disk-manager_0.2.0_x64-setup.exe.sig')
  })
  it('缺 .sig 时返回 null', () => {
    const files = matchNsisArtifacts(['dayu-disk-manager_0.2.0_x64-setup.exe'])
    expect(files.exe).toBe('dayu-disk-manager_0.2.0_x64-setup.exe')
    expect(files.sig).toBeNull()
  })
  it('无 setup.exe 时返回 null', () => {
    expect(matchNsisArtifacts(['foo.msi'])).toBeNull()
  })
})
```

- [ ] **步骤 2：运行测试验证失败**

运行：`pnpm vitest run scripts/upload-qiniu.test.ts`
预期：FAIL，报错 "Failed to resolve import './upload-qiniu.js'"（脚本尚未创建）。

- [ ] **步骤 3：实现脚本（含纯函数 + 上传主流程）**

创建 `scripts/upload-qiniu.js`：

```js
#!/usr/bin/env node
/**
 * 七牛上传脚本（Tauri 2 产物版）—— 上传 NSIS 安装包 + 签名 + 自组装 latest.json
 *
 * 用法：node scripts/upload-qiniu.js <版本号> <bundle目录>
 *   node scripts/upload-qiniu.js 0.2.0 src-tauri/target/x86_64-pc-windows-msvc/release/bundle
 *
 * 配置：本地 .qiniu.local.json 优先，否则读 QINIU_* 环境变量（CI）。
 */

import path from 'node:path'
import fs from 'node:fs'
import { fileURLToPath } from 'node:url'
import { createRequire } from 'node:module'

const require = createRequire(import.meta.url)
const qiniu = require('qiniu')

const REMOTE_PREFIX = 'dayu-disk-manager/win/x64'
const LOCAL_CONFIG_PATH = path.resolve(fileURLToPath(import.meta.url), '..', '..', '.qiniu.local.json')
const ZONE_MAP = {
  z0: 'Zone_z0', 'cn-east-2': 'Zone_cn_east_2', cn_east_2: 'Zone_cn_east_2',
  z1: 'Zone_z1', z2: 'Zone_z2', na0: 'Zone_na0', as0: 'Zone_as0',
}

// ── 纯函数（可单测） ────────────────────────────────
export function buildQiniuUrl(bucketDomain, prefix, fileName) {
  const host = String(bucketDomain).replace(/^https?:\/\//, '')
  return `https://${host}/${prefix}/${fileName}`
}

export function assembleLatestJson({ version, notes, pubDate, exeFileName, signature, bucketDomain }) {
  const url = buildQiniuUrl(bucketDomain, REMOTE_PREFIX, exeFileName)
  return JSON.stringify({
    version,
    notes,
    pub_date: pubDate,
    platforms: {
      'windows-x86_64': { signature, url },
    },
  }, null, 2)
}

export function matchNsisArtifacts(fileNames) {
  const exe = fileNames.find((n) => /-setup\.exe$/i.test(n) && !/\.sig$/i.test(n))
  if (!exe) return null
  const sig = fileNames.find((n) => n === `${exe}.sig`) ?? null
  return { exe, sig }
}

// ── 配置加载 ────────────────────────────────────────
function loadQiniuConfig() {
  let cfg = null
  if (fs.existsSync(LOCAL_CONFIG_PATH)) {
    cfg = JSON.parse(fs.readFileSync(LOCAL_CONFIG_PATH, 'utf8'))
  } else {
    cfg = {
      accessKey: process.env.QINIU_ACCESS_KEY,
      secretKey: process.env.QINIU_SECRET_KEY,
      bucket: process.env.QINIU_BUCKET,
      bucketDomain: process.env.QINIU_BUCKET_DOMAIN,
      zone: process.env.QINIU_ZONE,
    }
  }
  const missing = Object.entries(cfg).filter(([, v]) => !v).map(([k]) => k)
  if (missing.length) {
    console.error(`缺少七牛配置项: ${missing.join(', ')}（本地 .qiniu.local.json 或 QINIU_* 环境变量）`)
    process.exit(1)
  }
  return cfg
}

function resolveZone(zone) {
  return qiniu.zone[ZONE_MAP[zone] || zone] || null
}

// ── 产物查找 ────────────────────────────────────────
function findNsisBundleDir(bundleDir) {
  const direct = path.join(bundleDir, 'nsis')
  if (fs.existsSync(direct)) return direct
  // 兜底：递归一层找 nsis 子目录
  for (const sub of fs.readdirSync(bundleDir, { withFileTypes: true })) {
    if (sub.isDirectory()) {
      const candidate = path.join(bundleDir, sub.name, 'nsis')
      if (fs.existsSync(candidate)) return candidate
    }
  }
  return null
}

// ── 上传 ────────────────────────────────────────────
function uploadFile(mac, uploader, bucket, bucketDomain, localPath, remoteName) {
  const key = `${REMOTE_PREFIX}/${remoteName}`
  const putExtra = qiniu.resume_up.PutExtra.create()
  const putPolicy = new qiniu.rs.PutPolicy({ scope: `${bucket}:${key}` })
  const token = putPolicy.uploadToken(mac)
  console.log(`上传: ${remoteName} → ${key}`)
  return new Promise((resolve, reject) => {
    uploader.putFileV2(token, key, localPath, putExtra, (err, body, info) => {
      if (err) return reject(err)
      if (!info || info.statusCode !== 200) {
        return reject(new Error(`上传失败(${info ? info.statusCode : '?'}): ${JSON.stringify(body)}`))
      }
      console.log(`  完成: https://${bucketDomain}/${key}`)
      resolve()
    })
  })
}

// ── 主流程 ──────────────────────────────────────────
async function main() {
  const version = process.argv[2]
  const bundleDir = path.resolve(process.argv[3] || 'src-tauri/target/release/bundle')
  if (!version) {
    console.error('用法: node scripts/upload-qiniu.js <版本号> <bundle目录>')
    process.exit(1)
  }

  const cfg = loadQiniuConfig()
  const nsisDir = findNsisBundleDir(bundleDir)
  if (!nsisDir) {
    console.error(`在 ${bundleDir} 下找不到 nsis 产物目录`)
    process.exit(1)
  }
  const files = matchNsisArtifacts(fs.readdirSync(nsisDir))
  if (!files || !files.sig) {
    console.error('找不到 NSIS setup.exe 或其 .sig 签名文件（确认 createUpdaterArtifacts=true 且构建时设置了 TAURI_SIGNING_PRIVATE_KEY）')
    process.exit(1)
  }

  const config = new qiniu.conf.Config({ useHttpsDomain: true, accelerateUploading: true })
  config.zone = resolveZone(cfg.zone)
  if (!config.zone) {
    console.error(`无效 zone: ${cfg.zone}（可选 z0/z1/z2/na0/as0）`)
    process.exit(1)
  }
  const mac = new qiniu.auth.digest.Mac(cfg.accessKey, cfg.secretKey)
  const uploader = new qiniu.resume_up.ResumeUploader(config)

  const exePath = path.join(nsisDir, files.exe)
  const sigPath = path.join(nsisDir, files.sig)
  const signature = fs.readFileSync(sigPath, 'utf8').trim()
  const pubDate = new Date().toISOString()

  // 1) 先并发传 exe + sig
  await Promise.all([
    uploadFile(mac, uploader, cfg.bucket, cfg.bucketDomain, exePath, files.exe),
    uploadFile(mac, uploader, cfg.bucket, cfg.bucketDomain, sigPath, files.sig),
  ])
  // 2) 组装并上传 latest.json（url 运行时拼，指向已就位的 exe）
  const latestJson = assembleLatestJson({
    version, notes: `大禹磁盘管理器 ${version}`, pubDate,
    exeFileName: files.exe, signature, bucketDomain: cfg.bucketDomain,
  })
  const latestTmp = path.join(nsisDir, 'latest.json')
  fs.writeFileSync(latestTmp, latestJson, 'utf8')
  await uploadFile(mac, uploader, cfg.bucket, cfg.bucketDomain, latestTmp, 'latest.json')

  console.log('全部上传完成')
}

// 主模块守卫：仅直接运行时执行 main，被测试 import 时跳过
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main().catch((e) => { console.error('脚本异常:', e.message); process.exit(1) })
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`pnpm vitest run scripts/upload-qiniu.test.ts`
预期：PASS，3 个 describe 全绿。

- [ ] **步骤 5：创建本地配置模板**

创建 `.qiniu.local.json.example`：

```json
{
  "accessKey": "你的七牛 AccessKey",
  "secretKey": "你的七牛 SecretKey",
  "bucket": "你的七牛空间名",
  "bucketDomain": "你的七牛域名（如 qiniu.example.com，不带 https://）",
  "zone": "z2"
}
```

- [ ] **步骤 6：gitignore + 安装依赖**

`.gitignore` 末尾追加：
```
.qiniu.local.json
```

运行：`pnpm add -D qiniu`

- [ ] **步骤 7：Commit**

```
git add scripts/upload-qiniu.js scripts/upload-qiniu.test.ts .qiniu.local.json.example .gitignore package.json pnpm-lock.yaml
git commit -m "feat(release): 新增七牛上传脚本与本地配置模板"
```

---

## 任务 3：release.yml 集成签名与七牛上传

**文件：**
- 修改：`.github/workflows/release.yml`（`tauri-apps/tauri-action` 步骤 + 其后新增步骤）

- [ ] **步骤 1：给 tauri-action 注入签名密钥**

在 `.github/workflows/release.yml` 的 `tauri-apps/tauri-action@v0` 步骤中，`env:` 下 `GITHUB_TOKEN` 同级增加两个签名密钥，`with:` 下增加 `updaterJsonPreferNsis`。该步骤改为：

```yaml
      - name: Build Tauri app and upload to Release
        uses: tauri-apps/tauri-action@v0
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
          TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
        with:
          tagName: ${{ env.VERSION }}
          releaseName: '大禹磁盘管理器 ${{ env.VERSION }}'
          releaseBody: '请下载对应的 .msi 或 .exe 安装包。'
          releaseDraft: false
          prerelease: false
          updaterJsonPreferNsis: true
          args: --target x86_64-pc-windows-msvc --config src-tauri/tauri.release.conf.json
```

- [ ] **步骤 2：新增七牛上传步骤**

紧接上一步之后（与 `tauri-action` 同一 `steps` 列表，位于 job 末尾）追加：

```yaml
      - name: Upload update files to Qiniu
        shell: bash
        run: node scripts/upload-qiniu.js "$APP_VERSION" src-tauri/target/x86_64-pc-windows-msvc/release/bundle
        env:
          QINIU_ACCESS_KEY: ${{ secrets.QINIU_ACCESS_KEY }}
          QINIU_SECRET_KEY: ${{ secrets.QINIU_SECRET_KEY }}
          QINIU_BUCKET: ${{ secrets.QINIU_BUCKET }}
          QINIU_BUCKET_DOMAIN: ${{ secrets.QINIU_BUCKET_DOMAIN }}
          QINIU_ZONE: ${{ secrets.QINIU_ZONE }}
```

> 依赖前置：tauri-action 已在 secrets 提供签名密钥时生成 `.sig`，本步骤读取并上传。

- [ ] **步骤 3：本地校验 YAML 语法**

运行：
```
pnpm exec js-yaml .github/workflows/release.yml > NUL
```
预期：无输出（语法合法）。若 `js-yaml` 未装，改用 `node -e "require('js-yaml').load(require('fs').readFileSync('.github/workflows/release.yml','utf8'))"`（pnpm 依赖树里通常已有 js-yaml）。

- [ ] **步骤 4：Commit**

```
git add .github/workflows/release.yml
git commit -m "ci(release): 注入更新签名密钥并上传产物到七牛"
```

---

## 任务 4：前端 useUpdater composable（TDD）

**文件：**
- 创建：`src/composables/useUpdater.ts`
- 创建：`src/composables/useUpdater.test.ts`
- 修改：`package.json`

- [ ] **步骤 1：安装前端依赖**

运行：`pnpm add @tauri-apps/plugin-updater @tauri-apps/plugin-dialog`

- [ ] **步骤 2：写失败测试**

创建 `src/composables/useUpdater.test.ts`：

```ts
import { afterEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
  check: vi.fn(),
  ask: vi.fn(),
  message: vi.fn(),
  relaunch: vi.fn(),
  downloadAndInstall: vi.fn(),
}))

vi.mock('@tauri-apps/plugin-updater', () => ({
  check: mocks.check,
  relaunch: mocks.relaunch,
}))
vi.mock('@tauri-apps/plugin-dialog', () => ({
  ask: mocks.ask,
  message: mocks.message,
}))
vi.mock('@tauri-apps/plugin-process', () => ({ relaunch: mocks.relaunch }))

import { checkForUpdates } from './useUpdater'

function fakeUpdate() {
  return { available: true, version: '0.2.0', downloadAndInstall: mocks.downloadAndInstall }
}

afterEach(() => vi.clearAllMocks())

describe('checkForUpdates', () => {
  it('无更新：静默模式不提示，手动模式提示已是最新', async () => {
    mocks.check.mockResolvedValue({ available: false })
    await checkForUpdates(true)
    expect(mocks.ask).not.toHaveBeenCalled()
    await checkForUpdates(false)
    expect(mocks.message).toHaveBeenCalledWith('当前已是最新版本。')
  })

  it('有更新：用户同意 → 下载安装 → 重启', async () => {
    mocks.check.mockResolvedValue(fakeUpdate())
    mocks.ask.mockResolvedValue(true)
    mocks.downloadAndInstall.mockResolvedValue(undefined)
    await checkForUpdates(false)
    expect(mocks.ask).toHaveBeenCalled()
    expect(mocks.downloadAndInstall).toHaveBeenCalled()
    expect(mocks.relaunch).toHaveBeenCalled()
  })

  it('有更新但用户拒绝：不下载', async () => {
    mocks.check.mockResolvedValue(fakeUpdate())
    mocks.ask.mockResolvedValue(false)
    await checkForUpdates(false)
    expect(mocks.downloadAndInstall).not.toHaveBeenCalled()
  })

  it('检查失败：静默模式吞掉，手动模式提示错误', async () => {
    mocks.check.mockRejectedValue(new Error('网络错误'))
    await checkForUpdates(true) // 不抛
    await expect(checkForUpdates(false)).resolves.not.toThrow()
    expect(mocks.message).toHaveBeenCalledWith('检查更新失败：网络错误')
  })
})
```

> 注：`relaunch` 实际由 `@tauri-apps/plugin-process` 提供；本测试将其一并 mock。若实现改用 updater 内置 relaunch，同步调整 mock 与实现。

- [ ] **步骤 3：运行测试验证失败**

运行：`pnpm vitest run src/composables/useUpdater.test.ts`
预期：FAIL，"Cannot find module './useUpdater'"。

- [ ] **步骤 4：实现 composable**

创建 `src/composables/useUpdater.ts`：

```ts
import { check } from '@tauri-apps/plugin-updater'
import { ask, message } from '@tauri-apps/plugin-dialog'
import { relaunch } from '@tauri-apps/plugin-process'

/**
 * 检查并引导安装更新。
 * @param silent true=启动静默检查（无更新/失败都不打扰）；false=设置页手动触发（任何状态都给反馈）
 */
export async function checkForUpdates(silent: boolean): Promise<void> {
  let update
  try {
    update = await check()
  } catch (e) {
    if (silent) return
    message(`检查更新失败：${String(e).replace(/^Error:\s*/, '')}`)
    return
  }

  if (!update?.available) {
    if (!silent) message('当前已是最新版本。')
    return
  }

  const agreed = await ask(
    `发现新版本 ${update.version}，是否立即下载并安装？`,
    { title: '发现新版本', okLabel: '立即更新', cancelLabel: '稍后' },
  )
  if (!agreed) return

  await update.downloadAndInstall()
  await relaunch()
}
```

> 若 `@tauri-apps/plugin-process` 未随 plugin-dialog 一起装，补 `pnpm add @tauri-apps/plugin-process`。capabilities 的 `process:allow-restart` 已在任务 1 加好。

- [ ] **步骤 5：运行测试验证通过**

运行：`pnpm vitest run src/composables/useUpdater.test.ts`
预期：PASS，4 个用例全绿。

- [ ] **步骤 6：Commit**

```
git add src/composables/useUpdater.ts src/composables/useUpdater.test.ts package.json pnpm-lock.yaml
git commit -m "feat(updater): 新增 checkForUpdates composable 与单测"
```

---

## 任务 5：App.vue 启动静默检查

**文件：**
- 修改：`src/App.vue`

- [ ] **步骤 1：在 App 挂载后触发静默检查**

读取 `src/App.vue` 现有 `<script setup>` 内容，在已有 `onMounted`（若无则新增导入与调用）内追加一次容错的静默检查。在 `<script setup>` 顶部 import 区加：

```ts
import { checkForUpdates } from './composables/useUpdater'
```

在 `onMounted(...)` 回调末尾（或新增一个 `onMounted`）追加：

```ts
  // 启动静默检查更新；失败由 composable 内部吞掉，绝不阻塞主界面
  checkForUpdates(true).catch(() => {})
```

> 不新增可见 UI；`silent=true` 保证无更新或失败都打扰用户。

- [ ] **步骤 2：验证类型与构建**

运行：`pnpm build`
预期：`vue-tsc --noEmit && vite build` 通过，无类型错误。

- [ ] **步骤 3：Commit**

```
git add src/App.vue
git commit -m "feat(updater): App 启动时静默检查更新"
```

---

## 任务 6：SettingsView 检查更新入口（TDD）

**文件：**
- 修改：`src/views/SettingsView.vue`
- 创建：`src/views/SettingsView.updater.test.ts`（仅测更新卡片交互，避免与现有设置逻辑耦合）

- [ ] **步骤 1：写失败测试**

创建 `src/views/SettingsView.updater.test.ts`：

```ts
// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import SettingsView from './SettingsView.vue'

const mocks = vi.hoisted(() => ({
  checkForUpdates: vi.fn(),
  getConfig: vi.fn(),
  saveConfig: vi.fn(),
  exportHistory: vi.fn(),
}))

vi.mock('../composables/useUpdater', () => ({
  checkForUpdates: mocks.checkForUpdates,
}))
vi.mock('../ipc/invoke', () => ({
  ipc: {
    getConfig: mocks.getConfig,
    saveConfig: mocks.saveConfig,
    exportHistory: mocks.exportHistory,
  },
}))

describe('SettingsView 检查更新', () => {
  it('点击按钮触发手动检查', async () => {
    mocks.getConfig.mockResolvedValue({ repository: '', scan: { minSizeMb: 0, excludePaths: [] } })
    mocks.checkForUpdates.mockResolvedValue(undefined)
    const wrapper = mount(SettingsView)
    await flushPromises()
    await wrapper.get('[data-test="check-update"]').trigger('click')
    await flushPromises()
    expect(mocks.checkForUpdates).toHaveBeenCalledWith(false)
  })
})
```

- [ ] **步骤 2：运行测试验证失败**

运行：`pnpm vitest run src/views/SettingsView.updater.test.ts`
预期：FAIL，找不到 `[data-test="check-update"]`。

- [ ] **步骤 3：实现更新卡片**

在 `src/views/SettingsView.vue`：
- `<script setup>` import 区加：
  ```ts
  import { checkForUpdates } from '../composables/useUpdater'
  ```
- 加状态与处理函数（与现有 `saved`/`error` 同风格）：
  ```ts
  const checking = ref(false)
  async function checkUpdate() {
    checking.value = true
    try { await checkForUpdates(false) }
    catch (e) { error.value = String(e) }
    finally { checking.value = false }
  }
  ```
- 在模板「操作日志」section（`exportLog` 那段）之后、`</template>` 之前插入新卡片，沿用现有 `settings-section` 三栏样式：
  ```html
      <section class="settings-section">
        <div class="settings-label"><div class="settings-icon"><RefreshCw :size="17" /></div><div><strong>检查更新</strong><span>获取最新版本并引导安装</span></div></div>
        <div class="settings-control export-control"><button class="button button-secondary" :disabled="checking" data-test="check-update" @click="checkUpdate"><RefreshCw :size="14" /> {{ checking ? '检查中…' : '检查更新' }}</button></div>
      </section>
  ```
- import 图标（与现有 lucide import 合并）：把 `RefreshCw` 加入 `@lucide/vue` 的导入列表。

- [ ] **步骤 4：运行测试验证通过**

运行：`pnpm vitest run src/views/SettingsView.updater.test.ts`
预期：PASS。

- [ ] **步骤 5：跑全量前端测试确认无回归**

运行：`pnpm test`
预期：全绿（含原有 ScanView/ProgressStage 测试 + 新增 updater 相关）。

- [ ] **步骤 6：Commit**

```
git add src/views/SettingsView.vue src/views/SettingsView.updater.test.ts
git commit -m "feat(updater): 设置页新增检查更新入口"
```

---

## 任务 7：发布手册与端到端验证

**文件：**
- 创建：`docs/notes/auto-update-publish.md`

文档类，记录发布所需的人工操作与端到端验证步骤。

- [ ] **步骤 1：编写发布手册**

创建 `docs/notes/auto-update-publish.md`：

````markdown
# 自动更新发布手册

## 一次性准备

### 1. 生成 Tauri 更新签名密钥
```bash
pnpm tauri signer generate -w ~/.tauri/dayu-disk-manager.key
```
- 私钥文件 `~/.tauri/dayu-disk-manager.key`（设密码）→ **绝不进仓库**
- 公钥文件 `~/.tauri/dayu-disk-manager.key.pub`

### 2. 填配置
- `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey` ← 粘贴 `.key.pub` 全文
- `src-tauri/tauri.conf.json` 的 `endpoints` ← 把 `<替换为你的七牛域名>` 换成真实域名
- 本地手测：`cp .qiniu.local.json.example .qiniu.local.json` 并填好

### 3. 配置 GitHub Secrets
| Secret | 值 |
|--------|----|
| `TAURI_SIGNING_PRIVATE_KEY` | `.key` 文件内容 |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 生成时的密码（无则留空） |
| `QINIU_ACCESS_KEY` / `QINIU_SECRET_KEY` | 七牛密钥 |
| `QINIU_BUCKET` | 七牛空间名 |
| `QINIU_BUCKET_DOMAIN` | 七牛域名（不带 https://） |
| `QINIU_ZONE` | 存储区域，如 `z2` |

## 发布流程
1. 更新三处版本号一致：`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`package.json`
2. commit 后 `git tag vX.Y.Z && git push origin vX.Y.Z`
3. Actions 自动：构建签名 nsis → 发 GitHub Release → 上传 exe/.sig/latest.json 到七牛

## 端到端验证
1. **本地构建签名产物**：设 `TAURI_SIGNING_PRIVATE_KEY` 环境变量后 `pnpm tauri build`，确认 `bundle/nsis/` 下有 `*-setup.exe` 与 `.sig`
2. **本地跑上传**：`node scripts/upload-qiniu.js <版本> src-tauri/target/x86_64-pc-windows-msvc/release/bundle`
3. **清单可达**：浏览器打开 endpoint URL，返回 200 且 JSON 含 `platforms.windows-x86_64`
4. **App 更新闭环**：装旧版 → 发新版 → 重启旧版 → 启动静默检查弹 dialog → 同意 → 安装 → 重启后版本更新
````

- [ ] **步骤 2：Commit**

```
git add docs/notes/auto-update-publish.md
git commit -m "docs(release): 自动更新发布手册与端到端验证"
```

---

## 自检

**1. 规格覆盖度**（对照设计文档章节）：
- §后端启用 updater（Cargo/lib.rs/tauri.conf.json/capabilities）→ 任务 1 ✓
- §前端检查 UI（composable/App.vue/SettingsView）→ 任务 4/5/6 ✓
- §七牛上传脚本（含 latest.json 组装、url 运行时拼、上传顺序）→ 任务 2 ✓
- §release.yml 改造（签名密钥 env + 七牛步骤）→ 任务 3 ✓
- §签名密钥生成 → 任务 7 ✓
- §GitHub Secrets 清单 → 任务 7 ✓
- §占位符与用户自助配置（endpoints/pubkey 占位、七牛走 Secrets）→ 任务 1/2/7 ✓
- §验证方式 → 任务 7 端到端 + 各任务的构建/测试验证 ✓
- 无遗漏。

**2. 占位符扫描**：任务 1 的 `pubkey`/`endpoints` 为有意占位符（用户自助填，已在步骤说明 + 任务 7 指引），非缺陷；其余步骤均含完整代码与命令，无 TODO/"适当错误处理"等空话。✓

**3. 类型一致性**：
- `checkForUpdates(silent: boolean)` 在任务 4 定义，任务 5（`checkForUpdates(true)`）、任务 6（`checkForUpdates(false)`）调用签名一致 ✓
- `assembleLatestJson`/`buildQiniuUrl`/`matchNsisArtifacts` 任务 2 定义与测试用例参数一致 ✓
- 前端依赖 `@tauri-apps/plugin-process` 在任务 4 测试与实现中一致 mock/导入，并在实现注释里给出补装提示 ✓
- `data-test="check-update"` 在任务 6 测试与模板一致 ✓

---

## 执行交接

计划已完成并保存到 `docs/superpowers/plans/2026-07-24-auto-update-qiniu.md`。两种执行方式：

**1. 子代理驱动（推荐）** - 每个任务调度一个新的子代理，任务间进行审查，快速迭代

**2. 内联执行** - 在当前会话中使用 executing-plans 执行任务，批量执行并设有检查点

选哪种方式？
