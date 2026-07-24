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

export function isAccelerateUploadingEnabled(value) {
  return value === true || String(value).toLowerCase() === 'true'
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
      accelerateUploading: process.env.QINIU_ACCELERATE_UPLOADING,
    }
  }
  const required = ['accessKey', 'secretKey', 'bucket', 'bucketDomain', 'zone']
  const missing = required.filter((key) => !cfg[key])
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

export function refreshCdnUrl(cdnManager, url) {
  return new Promise((resolve, reject) => {
    cdnManager.refreshUrls([url], (err, body, info) => {
      if (err) return reject(err)
      if (!info || info.statusCode !== 200) {
        return reject(new Error(`刷新 CDN 缓存失败(${info ? info.statusCode : '?'}): ${JSON.stringify(body)}`))
      }
      console.log(`已刷新 CDN 缓存: ${url}`)
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

  const config = new qiniu.conf.Config({
    useHttpsDomain: true,
    accelerateUploading: isAccelerateUploadingEnabled(cfg.accelerateUploading),
  })
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
  await refreshCdnUrl(
    new qiniu.cdn.CdnManager(mac),
    buildQiniuUrl(cfg.bucketDomain, REMOTE_PREFIX, 'latest.json'),
  )

  console.log('全部上传完成')
}

// 主模块守卫：仅直接运行时执行 main，被测试 import 时跳过
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main().catch((e) => { console.error('脚本异常:', e.message); process.exit(1) })
}
