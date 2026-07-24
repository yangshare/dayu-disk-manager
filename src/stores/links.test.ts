// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import { useLinksStore } from './links'
import type { ProgressEvent } from '../ipc/types'

const mocks = vi.hoisted(() => ({
  listLinks: vi.fn(),
  startRestore: vi.fn(),
  breakLink: vi.fn(),
  onProgress: vi.fn(),
}))

vi.mock('../ipc/invoke', () => ({
  ipc: {
    listLinks: mocks.listLinks,
    startRestore: mocks.startRestore,
    breakLink: mocks.breakLink,
  },
}))

vi.mock('../ipc/events', () => ({ onProgress: mocks.onProgress }))

let progressHandler: ((event: ProgressEvent) => void) | null = null

beforeEach(() => {
  setActivePinia(createPinia())
  vi.clearAllMocks()
  mocks.listLinks.mockResolvedValue([])
  mocks.startRestore.mockResolvedValue(true)
  mocks.breakLink.mockResolvedValue(true)
  mocks.onProgress.mockImplementation(async (handler: (event: ProgressEvent) => void) => {
    progressHandler = handler
    return vi.fn()
  })
  progressHandler = null
})

describe('links store', () => {
  it('shows restore progress, prevents duplicate restores, and reports completion', async () => {
    let finishRestore: (() => void) | undefined
    mocks.startRestore.mockImplementationOnce(() => new Promise<void>((resolve) => { finishRestore = resolve }))
    const store = useLinksStore()

    const restorePromise = store.restore('migration-1')
    await Promise.resolve()
    await Promise.resolve()

    expect(store.running).toBe(true)
    expect(store.activeRestoreId).toBe('migration-1')
    expect(mocks.startRestore).toHaveBeenCalledWith('migration-1', false)
    expect(progressHandler).not.toBeNull()

    progressHandler?.({
      taskId: 'restore-migration-1', stage: 'copying', percent: 24,
      message: '正在复制回原磁盘',
    })
    expect(store.progress?.percent).toBe(24)

    await store.restore('migration-1')
    expect(mocks.startRestore).toHaveBeenCalledTimes(1)

    finishRestore?.()
    await restorePromise

    expect(store.running).toBe(false)
    expect(store.result).toEqual({ ok: true, message: '还原完成，源目录已恢复为普通目录。' })
    expect(mocks.listLinks).toHaveBeenCalledTimes(1)
  })

  it('keeps the failure in the page state instead of opening a browser alert', async () => {
    mocks.startRestore.mockRejectedValueOnce('任务冲突')
    const store = useLinksStore()

    await store.restore('migration-2')

    expect(store.result).toEqual({ ok: false, message: '还原失败：任务冲突' })
    expect(store.running).toBe(false)
  })
})
