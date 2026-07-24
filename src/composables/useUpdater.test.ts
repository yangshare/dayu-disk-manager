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
