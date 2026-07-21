// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import { useScanStore } from './scan'
import type { ChildPage, ScanDriveResult, ScanInvalidatedEvent, ScanSnapshot, TreeNode } from '../ipc/types'

const mocks = vi.hoisted(() => ({
  scanDrive: vi.fn(),
  expandNode: vi.fn(),
  revealNode: vi.fn(),
  listRecommended: vi.fn(),
  restartElevated: vi.fn(),
  takeStartupScanIntent: vi.fn(),
  cancelScan: vi.fn(),
  onProgress: vi.fn(),
  onScanProgress: vi.fn(),
  onScanInvalidated: vi.fn(),
}))

vi.mock('../ipc/invoke', () => ({ ipc: mocks }))
vi.mock('../ipc/events', () => ({
  onProgress: mocks.onProgress,
  onScanProgress: mocks.onScanProgress,
  onScanInvalidated: mocks.onScanInvalidated,
}))

function node(path: string, depth = 0): TreeNode {
  return {
    path,
    displayName: path.split('\\').filter(Boolean).pop() ?? path,
    sizeBytes: 100,
    linkedTargetSizeBytes: null,
    fileCount: 1,
    dirCount: 1,
    depth,
    isReparse: false,
    reparseTag: null,
    isJunction: false,
    accessState: 'accessible',
    matchedPreset: null,
    category: null,
    autoMigrate: false,
    scanStatus: null,
    migrationId: null,
    childCount: 1,
    filteredChildCount: 1,
  }
}

function snapshot(id: string, source: 'mft' | 'filesystem' = 'mft', roots = [node('C:\\root')]): ScanSnapshot {
  return {
    scanId: id,
    source,
    roots,
    filteredRootCount: roots.length,
    rootFileSummary: {
      directFileSizeBytes: 5,
      directFileCount: 1,
      systemMetadataSizeBytes: source === 'mft' ? 2 : null,
      totalKnownSizeBytes: 7,
      incomplete: source === 'filesystem',
    },
    diagnostics: {
      scannedRecords: 2,
      scannedDirs: 1,
      scannedFiles: 1,
      skippedRecords: 0,
      orphanEntries: 0,
      hardLinkEntries: 0,
    },
  }
}

function complete(id: string, source: 'mft' | 'filesystem' = 'mft'): ScanDriveResult {
  return { kind: 'complete', snapshot: snapshot(id, source) }
}

function page(items: TreeNode[], nextOffset: number | null): ChildPage {
  return { items, total: items.length + (nextOffset === null ? 0 : 1), nextOffset }
}

let invalidationHandler: ((event: ScanInvalidatedEvent) => void) | undefined

beforeEach(() => {
  setActivePinia(createPinia())
  vi.clearAllMocks()
  mocks.scanDrive.mockReset().mockResolvedValue(complete('scan-1'))
  mocks.expandNode.mockReset().mockResolvedValue(page([], null))
  mocks.revealNode.mockReset().mockResolvedValue([])
  mocks.listRecommended.mockReset().mockResolvedValue([])
  mocks.restartElevated.mockReset().mockResolvedValue(true)
  mocks.cancelScan.mockReset().mockResolvedValue(true)
  mocks.onScanProgress.mockResolvedValue(vi.fn())
  mocks.onScanInvalidated.mockImplementation(async (handler: (event: ScanInvalidatedEvent) => void) => {
    invalidationHandler = handler
    return vi.fn()
  })
  invalidationHandler = undefined
  vi.stubGlobal('confirm', vi.fn(() => true))
})

describe('scan 状态机', () => {
  it('完成扫描后发布树、根汇总与 scan id', async () => {
    const store = useScanStore()
    await store.scan('mft')

    expect(mocks.scanDrive).toHaveBeenCalledWith('mft')
    expect(store.scanId).toBe('scan-1')
    expect(store.roots).toHaveLength(1)
    expect(store.rootFileSummary?.directFileCount).toBe(1)
    expect(store.loading).toBe(false)
  })

  it('接受提权后调用 restartElevated，成功时不继续 filesystem', async () => {
    mocks.scanDrive.mockResolvedValueOnce({ kind: 'needs_elevation' }).mockResolvedValueOnce(complete('elevated'))
    const store = useScanStore()

    await store.scan('auto')

    expect(mocks.restartElevated).toHaveBeenCalledTimes(1)
    expect(mocks.scanDrive).toHaveBeenCalledTimes(1)
    expect(store.scanId).toBe(null)
  })

  it('拒绝提权直接进入 filesystem', async () => {
    vi.stubGlobal('confirm', vi.fn(() => false))
    mocks.scanDrive.mockResolvedValueOnce({ kind: 'needs_elevation' }).mockResolvedValueOnce(complete('fs', 'filesystem'))
    const store = useScanStore()

    await store.scan('auto')

    expect(mocks.restartElevated).not.toHaveBeenCalled()
    expect(mocks.scanDrive).toHaveBeenNthCalledWith(2, 'filesystem')
    expect(store.source).toBe('filesystem')
  })

  it('UAC 取消或启动失败时仍提供 filesystem 选择', async () => {
    mocks.restartElevated.mockRejectedValueOnce(new Error('用户取消 UAC 提权'))
    mocks.scanDrive.mockResolvedValueOnce({ kind: 'needs_elevation' }).mockResolvedValueOnce(complete('fs', 'filesystem'))
    const store = useScanStore()

    await store.scan('auto')

    expect(mocks.scanDrive).toHaveBeenNthCalledWith(2, 'filesystem')
    expect(store.source).toBe('filesystem')
  })

  it('接受快速扫描失败降级后实际调用 filesystem', async () => {
    mocks.scanDrive.mockResolvedValueOnce({
      kind: 'fast_scan_unavailable',
      reason: { kind: 'unsupported_filesystem', actual: 'fat32' },
    }).mockResolvedValueOnce(complete('fs', 'filesystem'))
    const store = useScanStore()

    await store.scan('auto')

    expect(mocks.scanDrive).toHaveBeenNthCalledWith(2, 'filesystem')
    expect(store.source).toBe('filesystem')
  })

  it('拒绝快速扫描失败降级时保留旧快照', async () => {
    const store = useScanStore()
    await store.scan('mft')
    const oldRoots = store.roots
    vi.stubGlobal('confirm', vi.fn(() => false))
    mocks.scanDrive.mockResolvedValueOnce({
      kind: 'fast_scan_unavailable',
      reason: { kind: 'root_record_missing' },
    })

    await store.scan('auto')

    expect(store.roots).toBe(oldRoots)
    expect(store.scanId).toBe('scan-1')
    expect(store.error).toContain('根目录记录缺失')
  })
})

describe('失效与异步分页', () => {
  it('全局 MFT 失效清缓存并只自动重扫一次', async () => {
    const store = useScanStore()
    await store.scan('mft')
    expect(invalidationHandler).toBeDefined()
    mocks.scanDrive.mockResolvedValueOnce(complete('rescanned'))

    invalidationHandler!({ reason: 'migrated', autoRescan: true })
    await new Promise((resolve) => setTimeout(resolve, 0))
    await new Promise((resolve) => setTimeout(resolve, 0))

    expect(mocks.scanDrive).toHaveBeenCalledTimes(2)
    expect(mocks.scanDrive).toHaveBeenLastCalledWith('mft')
    expect(store.scanId).toBe('rescanned')
    expect(store.invalidated).toBe(false)
  })

  it('filesystem 失效不自动重扫且进入 invalidated 空态', async () => {
    const store = useScanStore()
    await store.scan('filesystem')
    invalidationHandler!({ reason: 'restored', autoRescan: false })
    await Promise.resolve()

    expect(mocks.scanDrive).toHaveBeenCalledTimes(1)
    expect(store.invalidated).toBe(true)
    expect(store.roots).toEqual([])
    expect(store.scanId).toBe(null)
  })

  it('旧分页响应在 scan id 变化后不能写入新快照', async () => {
    const store = useScanStore()
    await store.scan('mft')
    let resolvePage: (value: ChildPage) => void = () => {}
    mocks.expandNode.mockReturnValueOnce(new Promise<ChildPage>((resolve) => { resolvePage = resolve }))
    const pending = store.toggleNode('C:\\root')
    mocks.scanDrive.mockResolvedValueOnce(complete('new-scan', 'filesystem'))
    await store.scan('filesystem')
    resolvePage(page([node('C:\\root\\old')], null))
    await pending

    expect(store.scanId).toBe('new-scan')
    expect(store.pages).toEqual({})
  })

  it('StaleScan 统一清空快照并标记 invalidated', async () => {
    const store = useScanStore()
    await store.scan('mft')
    mocks.expandNode.mockRejectedValueOnce('stale_scan')

    await store.toggleNode('C:\\root')

    expect(store.scanId).toBe(null)
    expect(store.roots).toEqual([])
    expect(store.invalidated).toBe(true)
  })

  it('追加页按路径去重并防止同一 offset 并发请求', async () => {
    const store = useScanStore()
    const child = node('C:\\root\\child', 1)
    const duplicate = node('C:\\root\\child', 1)
    mocks.expandNode.mockResolvedValueOnce(page([child], 1))
    await store.scan('mft')
    await store.toggleNode('C:\\root')

    let resolveMore: (value: ChildPage) => void = () => {}
    mocks.expandNode.mockReturnValueOnce(new Promise<ChildPage>((resolve) => { resolveMore = resolve }))
    const first = store.loadMore('C:\\root')
    const second = store.loadMore('C:\\root')
    resolveMore(page([duplicate, node('C:\\root\\second', 1)], null))
    await Promise.all([first, second])

    expect(mocks.expandNode).toHaveBeenCalledTimes(2)
    expect(store.pages['C:\\root']?.items.map((item) => item.path)).toEqual([
      'C:\\root\\child', 'C:\\root\\second',
    ])
  })

  it('reveal 追加祖先分页时不制造重复节点', async () => {
    const store = useScanStore()
    await store.scan('mft')
    const child = node('C:\\root\\child', 1)
    mocks.revealNode.mockResolvedValueOnce([
      { parentPath: 'C:\\', page: page([node('C:\\root')], null) },
      { parentPath: 'C:\\root', page: page([child, child], null) },
    ])

    await store.reveal('C:\\root\\child')

    expect(store.pages['C:\\root']?.items).toHaveLength(1)
    expect(store.expandedKeys.has('C:\\root')).toBe(true)
    expect(store.highlightedPath).toBe('C:\\root\\child')
  })
})
