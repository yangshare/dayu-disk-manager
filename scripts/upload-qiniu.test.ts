import { describe, expect, it, vi } from 'vitest'
import {
  assembleLatestJson,
  buildQiniuUrl,
  isAccelerateUploadingEnabled,
  matchNsisArtifacts,
  refreshCdnUrl,
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

describe('isAccelerateUploadingEnabled', () => {
  it.each([true, 'true', 'TRUE'])('仅显式 true 时启用传输加速：%j', (value) => {
    expect(isAccelerateUploadingEnabled(value)).toBe(true)
  })

  it.each([undefined, false, 'false', '1', ''])('其他值保持关闭：%j', (value) => {
    expect(isAccelerateUploadingEnabled(value)).toBe(false)
  })
})

describe('refreshCdnUrl', () => {
  it('刷新 latest.json 的 CDN 缓存', async () => {
    const refreshUrls = vi.fn((urls, callback) => callback(null, { code: 200 }, { statusCode: 200 }))
    await refreshCdnUrl({ refreshUrls }, 'https://qiniu.example.com/latest.json')
    expect(refreshUrls).toHaveBeenCalledWith(['https://qiniu.example.com/latest.json'], expect.any(Function))
  })

  it('缓存刷新失败时拒绝，避免把陈旧清单当成成功发布', async () => {
    const refreshUrls = vi.fn((urls, callback) => callback(new Error('permission denied')))
    await expect(refreshCdnUrl({ refreshUrls }, 'https://qiniu.example.com/latest.json'))
      .rejects.toThrow('permission denied')
  })
})
