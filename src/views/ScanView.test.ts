// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import ScanView from './ScanView.vue'
import type {
  ChildPage, RevealLevel, ScanDriveResult, ScanInvalidatedEvent,
  ScanSnapshot, TreeNode,
} from '../ipc/types'

const mocks = vi.hoisted(() => ({
  scanDrive: vi.fn(),
  expandNode: vi.fn(),
  revealNode: vi.fn(),
  listRecommended: vi.fn(),
  restartElevated: vi.fn(),
  takeStartupScanIntent: vi.fn(),
  cancelScan: vi.fn(),
  onScanProgress: vi.fn(),
  onScanInvalidated: vi.fn(),
  push: vi.fn(),
}))

vi.mock('../ipc/invoke', () => ({
  ipc: {
    scanDrive: mocks.scanDrive,
    expandNode: mocks.expandNode,
    revealNode: mocks.revealNode,
    listRecommended: mocks.listRecommended,
    restartElevated: mocks.restartElevated,
    takeStartupScanIntent: mocks.takeStartupScanIntent,
    cancelScan: mocks.cancelScan,
  },
}))

vi.mock('../ipc/events', () => ({
  onScanProgress: mocks.onScanProgress,
  onScanInvalidated: mocks.onScanInvalidated,
}))

vi.mock('vue-router', () => ({
  useRouter: () => ({ push: mocks.push }),
}))

function eid(path: string): string {
  return encodeURIComponent(path)
}

function node(path: string, overrides: Partial<TreeNode> = {}): TreeNode {
  const name = path.split('\\').filter(Boolean).pop() ?? path
  return {
    path,
    displayName: name,
    sizeBytes: 1024,
    linkedTargetSizeBytes: null,
    fileCount: 1,
    dirCount: 1,
    depth: 0,
    isReparse: false,
    reparseTag: null,
    isJunction: false,
    accessState: 'accessible',
    matchedPreset: null,
    category: null,
    autoMigrate: true,
    scanStatus: null,
    migrationId: null,
    childCount: 0,
    filteredChildCount: 0,
    ...overrides,
  }
}

function page(items: TreeNode[], nextOffset: number | null = null): ChildPage {
  return { items, total: items.length + (nextOffset === null ? 0 : 1), nextOffset }
}

function snapshot(
  id: string,
  source: 'mft' | 'filesystem' = 'mft',
  roots: TreeNode[] = [],
  filteredRootCount = 0,
): ScanSnapshot {
  return {
    scanId: id,
    source,
    roots,
    filteredRootCount,
    rootFileSummary: {
      directFileSizeBytes: 100,
      directFileCount: 1,
      systemMetadataSizeBytes: 50,
      totalKnownSizeBytes: 150,
      incomplete: false,
    },
    diagnostics: {
      scannedRecords: 1,
      scannedDirs: 1,
      scannedFiles: 1,
      skippedRecords: 0,
      orphanEntries: 0,
      hardLinkEntries: 0,
      unresolvedExtensions: 0,
    },
  }
}

function complete(
  id: string,
  source: 'mft' | 'filesystem' = 'mft',
  roots: TreeNode[] = [],
  filteredRootCount = 0,
): ScanDriveResult {
  return { kind: 'complete', snapshot: snapshot(id, source, roots, filteredRootCount) }
}

let invalidationHandler: ((event: ScanInvalidatedEvent) => void) | null = null

beforeEach(async () => {
  const { createPinia, setActivePinia } = await import('pinia')
  setActivePinia(createPinia())
  vi.clearAllMocks()
  mocks.scanDrive.mockReset()
  mocks.expandNode.mockReset().mockResolvedValue(page([]))
  mocks.revealNode.mockReset().mockResolvedValue([])
  mocks.listRecommended.mockReset().mockResolvedValue([])
  mocks.restartElevated.mockReset().mockResolvedValue(true)
  mocks.takeStartupScanIntent.mockReset().mockResolvedValue(false)
  mocks.cancelScan.mockReset().mockResolvedValue(true)
  mocks.onScanProgress.mockReset().mockResolvedValue(vi.fn())
  mocks.onScanInvalidated.mockReset().mockImplementation(async (handler: (event: ScanInvalidatedEvent) => void) => {
    invalidationHandler = handler
    return vi.fn()
  })
  mocks.push.mockReset()
  invalidationHandler = null
  vi.stubGlobal('confirm', vi.fn(() => true))
})

async function startScan(wrapper: ReturnType<typeof mount>) {
  await wrapper.get('[data-testid="start-scan"]').trigger('click')
  await flushPromises()
  await flushPromises()
}

describe('ScanView 缩进树表', () => {
  it('渲染 roots；仅 childCount>0 节点显示展开按钮', async () => {
    const rootWithKids = node('C:\\Users', { childCount: 2, sizeBytes: 4096 })
    const rootLeaf = node('C:\\Games', { childCount: 0, sizeBytes: 2048 })
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', [rootWithKids, rootLeaf]))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    const rows = wrapper.findAll('tr.tree-row')
    expect(rows).toHaveLength(2)
    expect(rows[0]!.attributes('data-path')).toBe('C:\\Users')
    expect(rows[1]!.attributes('data-path')).toBe('C:\\Games')
    expect(wrapper.find(`[data-testid="caret-${eid('C:\\Users')}"]`).exists()).toBe(true)
    expect(wrapper.find(`[data-testid="caret-${eid('C:\\Games')}"]`).exists()).toBe(false)
  })

  it('行 key 使用 node path，点击展开按钮递归加载多层子树 + data-depth 缩进', async () => {
    const root = node('C:\\Users', { childCount: 1 })
    const child = node('C:\\Users\\me', { depth: 1, childCount: 1 })
    const grand = node('C:\\Users\\me\\AppData', { depth: 2 })
    mocks.expandNode
      .mockResolvedValueOnce(page([child]))
      .mockResolvedValueOnce(page([grand]))
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', [root]))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    await wrapper.get(`[data-testid="caret-${eid('C:\\Users')}"]`).trigger('click')
    await flushPromises()
    expect(mocks.expandNode).toHaveBeenCalledWith('s1', 'C:\\Users', 0, expect.any(Number))

    await wrapper.get(`[data-testid="caret-${eid('C:\\Users\\me')}"]`).trigger('click')
    await flushPromises()
    expect(mocks.expandNode).toHaveBeenCalledWith('s1', 'C:\\Users\\me', 0, expect.any(Number))

    const paths = wrapper.findAll('tr.tree-row').map((tr) => tr.attributes('data-path'))
    expect(paths).toEqual(['C:\\Users', 'C:\\Users\\me', 'C:\\Users\\me\\AppData'])
    expect(wrapper.find('tr[data-depth="2"]').exists()).toBe(true)
  })

  it('children 末尾 nextOffset 非 null 时显示"显示更多"行', async () => {
    const root = node('C:\\Users', { childCount: 5 })
    const child = node('C:\\Users\\me', { depth: 1, childCount: 0 })
    mocks.expandNode.mockResolvedValueOnce(page([child], 200))
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', [root]))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)
    await wrapper.get(`[data-testid="caret-${eid('C:\\Users')}"]`).trigger('click')
    await flushPromises()

    const more = wrapper.find(`[data-testid="more-${eid('C:\\Users')}"]`)
    expect(more.exists()).toBe(true)
    expect(more.text()).toContain('显示更多')
  })

  it('点击"显示更多"调用 store.loadMore 并以 nextOffset 续拉', async () => {
    const root = node('C:\\Users', { childCount: 5 })
    const child1 = node('C:\\Users\\me', { depth: 1 })
    const child2 = node('C:\\Users\\you', { depth: 1 })
    mocks.expandNode
      .mockResolvedValueOnce(page([child1], 200))
      .mockResolvedValueOnce(page([child2], null))
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', [root]))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)
    await wrapper.get(`[data-testid="caret-${eid('C:\\Users')}"]`).trigger('click')
    await flushPromises()

    await wrapper.get(`[data-testid="more-${eid('C:\\Users')}"] button`).trigger('click')
    await flushPromises()
    expect(mocks.expandNode).toHaveBeenCalledWith('s1', 'C:\\Users', 200, expect.any(Number))

    const rows = wrapper.findAll('tr.tree-row').map((tr) => tr.attributes('data-path'))
    expect(rows).toContain('C:\\Users\\you')
  })

  it('recommended 点击：await reveal + scrollIntoView + highlighted class', async () => {
    const root = node('C:\\Users', { childCount: 1 })
    const rec = node('C:\\Users\\me\\AppData', { depth: 2, matchedPreset: 'p1', autoMigrate: true })
    const me = node('C:\\Users\\me', { depth: 1, childCount: 1 })
    const levels: RevealLevel[] = [
      { parentPath: 'C:\\Users', page: page([me]) },
      { parentPath: 'C:\\Users\\me', page: page([rec]) },
    ]
    mocks.revealNode.mockResolvedValueOnce(levels)
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', [root]))
    mocks.listRecommended.mockResolvedValueOnce([rec])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    // jsdom 默认不实现 scrollIntoView，按需打桩。
    const originalScroll = (Element.prototype as unknown as { scrollIntoView?: () => void }).scrollIntoView
    const spy = vi.fn()
    ;(Element.prototype as unknown as { scrollIntoView: () => void }).scrollIntoView = spy
    try {
      await wrapper.get(`[data-testid="reveal-${eid(rec.path)}"]`).trigger('click')
      await flushPromises()
      await flushPromises()

      expect(mocks.revealNode).toHaveBeenCalledWith('s1', rec.path, expect.any(Number))
      const row = wrapper.findAll('tr.tree-row').find((tr) => tr.attributes('data-path') === rec.path)
      expect(row).toBeDefined()
      expect(row!.classes()).toContain('highlighted')
      expect(spy).toHaveBeenCalled()
    } finally {
      if (originalScroll) {
        ;(Element.prototype as unknown as { scrollIntoView: () => void }).scrollIntoView = originalScroll
      } else {
        delete (Element.prototype as unknown as { scrollIntoView?: () => void }).scrollIntoView
      }
    }
  })

  it('根文件汇总不可点击、无展开/迁移入口', async () => {
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', []))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    const summary = wrapper.get('[data-testid="root-summary"]')
    expect(summary.text()).toContain('根文件汇总')
    expect(summary.text()).toContain('根级文件')
    expect(summary.text()).toContain('已知总占用')
    expect(summary.findAll('[data-testid^="caret-"]')).toHaveLength(0)
    expect(summary.findAll('[data-testid^="migrate-"]')).toHaveLength(0)
    expect(summary.findAll('button')).toHaveLength(0)
  })

  it('根文件汇总 incomplete=true 时显示"可能不完整"提示', async () => {
    const snap = snapshot('s1', 'filesystem', [], 0)
    snap.rootFileSummary.incomplete = true
    mocks.scanDrive.mockResolvedValueOnce({ kind: 'complete', snapshot: snap })
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    expect(wrapper.find('[data-testid="root-summary-incomplete"]').exists()).toBe(true)
    expect(wrapper.text()).toContain('可能不完整')
  })

  it('roots 为空仍显示根文件汇总 + 阈值空态', async () => {
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', []))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    expect(wrapper.find('[data-testid="root-summary"]').exists()).toBe(true)
    expect(wrapper.find('[data-testid="threshold-empty"]').exists()).toBe(true)
    expect(wrapper.text()).toContain('没有发现需要关注的目录')
  })

  it('filteredRootCount>0 显示一级隐藏数；filteredChildCount>0 显示子级隐藏提示', async () => {
    const root = node('C:\\Users', { childCount: 1, filteredChildCount: 4 })
    mocks.scanDrive.mockResolvedValueOnce({ kind: 'complete', snapshot: snapshot('s1', 'mft', [root], 7) })
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    expect(wrapper.find('[data-testid="filtered-roots"]').exists()).toBe(true)
    expect(wrapper.text()).toContain('一级隐藏 7 个目录')
    expect(wrapper.find(`[data-testid="filtered-children-${eid(root.path)}"]`).exists()).toBe(true)
    expect(wrapper.text()).toContain('隐藏 4 个子目录')
  })

  it('reparse / existing_link / inaccessible 节点不显示直接迁移入口', async () => {
    const reparseNode = node('C:\\junction', { isReparse: true, isJunction: true, autoMigrate: false })
    const linkNode = node('C:\\already-linked', { scanStatus: 'existing_link' })
    const inaccNode = node('C:\\locked', { accessState: 'inaccessible' })
    const normal = node('C:\\normal', {})
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', [reparseNode, linkNode, inaccNode, normal]))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    expect(wrapper.find(`[data-testid="migrate-${eid('C:\\junction')}"]`).exists()).toBe(false)
    expect(wrapper.find(`[data-testid="migrate-${eid('C:\\already-linked')}"]`).exists()).toBe(false)
    expect(wrapper.find(`[data-testid="migrate-${eid('C:\\locked')}"]`).exists()).toBe(false)
    expect(wrapper.find(`[data-testid="migrate-${eid('C:\\normal')}"]`).exists()).toBe(true)
  })

  it('existing_link 显示"已有软链接"标签', async () => {
    const linkNode = node('C:\\already-linked', { scanStatus: 'existing_link', migrationId: 'm1' })
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', [linkNode]))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    expect(wrapper.text()).toContain('已有软链接')
  })

  it('autoMigrate=false 显示"需确认风险"标签', async () => {
    const warnNode = node('C:\\risky', { autoMigrate: false })
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', [warnNode]))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    expect(wrapper.text()).toContain('需确认风险')
    expect(wrapper.find(`[data-testid="migrate-${eid('C:\\risky')}"]`).text()).toContain('自定义迁移')
  })

  it('filesystem 源失效显示"结果已失效，请重新扫描"', async () => {
    const root = node('C:\\Users', {})
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'filesystem', [root]))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    expect(invalidationHandler).not.toBeNull()
    invalidationHandler!({ reason: 'restored', autoRescan: false })
    await flushPromises()
    await flushPromises()

    expect(wrapper.find('[data-testid="invalidated-notice"]').exists()).toBe(true)
    expect(wrapper.text()).toContain('结果已失效')
  })

  it('长路径行结构完整，路径 ellipsis class 与 title 齐全', async () => {
    const longPath = 'C:\\' + 'a'.repeat(220)
    const longNode = node(longPath, {})
    mocks.scanDrive.mockResolvedValueOnce(complete('s1', 'mft', [longNode]))
    mocks.listRecommended.mockResolvedValueOnce([])
    const wrapper = mount(ScanView)

    await startScan(wrapper)

    const row = wrapper.findAll('tr.tree-row').find((tr) => tr.attributes('data-path') === longPath)
    expect(row).toBeDefined()
    const pathSpan = row!.get('.path')
    expect(pathSpan.attributes('title')).toBe(longPath)
    expect(pathSpan.text()).toBe(longPath)
  })
})