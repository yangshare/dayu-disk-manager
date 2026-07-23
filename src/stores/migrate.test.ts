// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import { useMigrateStore } from './migrate'

const mocks = vi.hoisted(() => ({
  precheckMigrate: vi.fn(),
  onProgress: vi.fn(),
}))

vi.mock('../ipc/invoke', () => ({ ipc: { precheckMigrate: mocks.precheckMigrate } }))
vi.mock('../ipc/events', () => ({ onProgress: mocks.onProgress }))

beforeEach(() => {
  setActivePinia(createPinia())
  vi.clearAllMocks()
  mocks.precheckMigrate.mockResolvedValue({
    ok: true,
    warnings: [],
    blockers: [],
    sourceSizeBytes: 1024,
    targetFreeBytes: 2048,
    vssAvailable: false,
  })
})

describe('migrate store', () => {
  it('clears the previous migration result and progress when prechecking a new source', async () => {
    const store = useMigrateStore()
    store.progress = {
      taskId: 'task-previous',
      stage: 'cleaning',
      percent: 100,
      message: '迁移完成',
    }
    store.result = { ok: true, message: '迁移完成' }

    await store.precheck('D:\\new-source')

    expect(mocks.precheckMigrate).toHaveBeenCalledWith('D:\\new-source')
    expect(store.progress).toBeNull()
    expect(store.result).toBeNull()
    expect(store.report?.ok).toBe(true)
  })
})
