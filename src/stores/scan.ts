import { defineStore } from 'pinia'
import { onScopeDispose, ref } from 'vue'
import { isTauri } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import type { UnlistenFn } from '@tauri-apps/api/event'
import { ipc } from '../ipc/invoke'
import { onScanInvalidated, onScanProgress } from '../ipc/events'
import type {
  FastScanFailure, RootFileSummary, ScanInvalidatedEvent, ScanMode,
  ScanProgressEvent, ScanSnapshot, ScanSource, TreeNode, ChildPage,
} from '../ipc/types'
import { isStaleScanError } from '../ipc/types'

const PAGE_SIZE = 200
const appWindow = isTauri() ? getCurrentWindow() : null

function failureDescription(reason: FastScanFailure): string {
  switch (reason.kind) {
    case 'unsupported_filesystem':
      return `当前卷的文件系统（${reason.actual}）不支持 MFT 快速扫描。`
    case 'unsupported_ntfs_version':
      return `当前 NTFS 版本（${reason.major}.${reason.minor}）不受快速扫描支持。`
    case 'invalid_volume_data':
      return '卷元数据无效，无法安全执行 MFT 快速扫描。'
    case 'root_record_missing':
      return '卷根目录记录缺失，无法安全执行 MFT 快速扫描。'
    case 'excessive_record_errors':
      return `MFT 记录错误过多（跳过 ${reason.skipped} / 扫描 ${reason.scanned}），已停止快速扫描。`
    case 'io':
      return reason.code === null
        ? '读取卷元数据时发生 I/O 错误。'
        : `读取卷元数据时发生 I/O 错误（代码 ${reason.code}）。`
  }
}

function userConfirm(message: string): boolean {
  try {
    return typeof globalThis.confirm === 'function' && globalThis.confirm(message)
  } catch {
    return false
  }
}

function errorMessage(error: unknown): string {
  if (typeof error === 'string') return error
  if (error && typeof error === 'object' && 'message' in error) {
    return String((error as { message: unknown }).message)
  }
  return String(error)
}

export const useScanStore = defineStore('scan', () => {
  const roots = ref<TreeNode[]>([])
  const scanId = ref<string | null>(null)
  const source = ref<ScanSource | null>(null)
  const filteredRootCount = ref(0)
  const rootFileSummary = ref<RootFileSummary | null>(null)
  const recommended = ref<TreeNode[]>([])
  const pages = ref<Record<string, ChildPage>>({})
  const expanded = ref<Map<string, ChildPage[]>>(new Map())
  const expandedKeys = ref<Set<string>>(new Set())
  const loadingPages = ref<Set<string>>(new Set())
  const highlightedPath = ref<string | null>(null)
  const invalidated = ref(false)
  const loading = ref(false)
  const cancelling = ref(false)
  const progress = ref<ScanProgressEvent | null>(null)
  const error = ref<string | null>(null)
  const hasScanned = ref(false)
  const initialized = ref(false)
  const pendingAutoRescan = ref(false)
  const autoRescanScheduled = ref(false)
  const pageRequests = new Map<string, Promise<ChildPage | null>>()
  let progressUnlisten: UnlistenFn | undefined
  let invalidatedUnlisten: UnlistenFn | undefined

  function clearSnapshot() {
    roots.value = []
    scanId.value = null
    source.value = null
    filteredRootCount.value = 0
    rootFileSummary.value = null
    recommended.value = []
    pages.value = {}
    expanded.value = new Map()
    expandedKeys.value = new Set()
    loadingPages.value = new Set()
    highlightedPath.value = null
    progress.value = null
    error.value = null
    invalidated.value = true
  }

  function applySnapshot(snapshot: ScanSnapshot) {
    scanId.value = snapshot.scanId
    source.value = snapshot.source
    roots.value = snapshot.roots
    filteredRootCount.value = snapshot.filteredRootCount
    rootFileSummary.value = snapshot.rootFileSummary
    recommended.value = []
    pages.value = {}
    expanded.value = new Map()
    expandedKeys.value = new Set()
    loadingPages.value = new Set()
    highlightedPath.value = null
    invalidated.value = false
    error.value = null
  }

  function handleOperationError(operationError: unknown): boolean {
    if (isStaleScanError(operationError)) {
      clearSnapshot()
      return true
    }
    error.value = errorMessage(operationError)
    return false
  }

  function handleInvalidated(event: ScanInvalidatedEvent) {
    const wasLoading = loading.value
    clearSnapshot()
    if (!event.autoRescan) return
    pendingAutoRescan.value = true
    if (!wasLoading) scheduleAutoRescan()
  }

  function scheduleAutoRescan() {
    if (autoRescanScheduled.value) return
    autoRescanScheduled.value = true
    queueMicrotask(() => {
      autoRescanScheduled.value = false
      if (!pendingAutoRescan.value || loading.value) return
      pendingAutoRescan.value = false
      void scan('mft')
    })
  }

  async function initialize() {
    if (initialized.value) return
    initialized.value = true
    try {
      const listener = (onScanProgress as unknown as ((cb: (event: ScanProgressEvent) => void) => Promise<UnlistenFn>) | undefined)
      if (typeof listener === 'function') {
        progressUnlisten = await listener((event) => { progress.value = event })
      }
    } catch {
      // The browser preview does not expose Tauri events; scans still work.
    }
    try {
      const listener = (onScanInvalidated as unknown as ((cb: (event: ScanInvalidatedEvent) => void) => Promise<UnlistenFn>) | undefined)
      if (typeof listener === 'function') {
        invalidatedUnlisten = await listener(handleInvalidated)
      }
    } catch {
      // The browser preview does not expose Tauri events; snapshots remain usable.
    }
  }

  async function scan(requestedMode: ScanMode = 'auto') {
    if (loading.value) return
    hasScanned.value = true
    loading.value = true
    cancelling.value = false
    progress.value = null
    error.value = null
    invalidated.value = false
    try {
      await initialize()
      let mode = requestedMode
      while (true) {
        const result = await ipc.scanDrive(mode)
        if (result.kind === 'complete') {
          // 失效事件优先于迟到的 scan 响应，不能复活旧树。
          if (!invalidated.value) {
            applySnapshot(result.snapshot)
            await listRecommended()
          }
          return
        }
        if (result.kind === 'needs_elevation') {
          const accepted = userConfirm('需要管理员权限读取卷元数据。是否重启并使用 MFT 快速扫描？')
          if (!accepted) {
            mode = 'filesystem'
            continue
          }
          try {
            const restarted = await ipc.restartElevated()
            if (restarted) {
              try { await appWindow?.close() } catch { /* 提权实例会继续工作 */ }
              return
            }
          } catch {
            error.value = '无法启动管理员扫描，已切换到文件系统扫描。'
          }
          mode = 'filesystem'
          continue
        }
        const reason = failureDescription(result.reason)
        const accepted = userConfirm(`${reason}\n是否改用文件系统扫描？`)
        if (!accepted) {
          error.value = reason
          return
        }
        mode = 'filesystem'
      }
    } catch (scanError) {
      if (isStaleScanError(scanError)) {
        clearSnapshot()
      } else if (!errorMessage(scanError).includes('用户取消')) {
        error.value = errorMessage(scanError)
      }
    } finally {
      loading.value = false
      cancelling.value = false
      if (pendingAutoRescan.value) scheduleAutoRescan()
    }
  }

  async function cancelScan() {
    if (!loading.value || cancelling.value) return
    cancelling.value = true
    try {
      await ipc.cancelScan()
    } catch (cancelError) {
      error.value = errorMessage(cancelError)
      cancelling.value = false
    }
  }

  function mergePage(parentPath: string, page: ChildPage) {
    const existing = pages.value[parentPath]
    if (!existing) {
      const seen = new Set<string>()
      const items = page.items.filter((item) => {
        if (seen.has(item.path)) return false
        seen.add(item.path)
        return true
      })
      pages.value[parentPath] = {
        items,
        total: page.total,
        nextOffset: page.nextOffset,
      }
      const next = new Map(expanded.value)
      next.set(parentPath, [pages.value[parentPath]!])
      expanded.value = next
      return
    }
    const items = existing.items.filter((item, index, all) => (
      all.findIndex((candidate) => candidate.path === item.path) === index
    ))
    const seen = new Set(items.map((item) => item.path))
    const appended = page.items.filter((item) => !seen.has(item.path) && seen.add(item.path))
    pages.value[parentPath] = {
      items: [...items, ...appended],
      total: page.total,
      nextOffset: page.nextOffset,
    }
    const next = new Map(expanded.value)
    next.set(parentPath, [pages.value[parentPath]!])
    expanded.value = next
  }

  async function requestPage(parentPath: string, offset: number, id: string): Promise<ChildPage | null> {
    const requestKey = `${id}:${parentPath}:${offset}`
    const previous = pageRequests.get(requestKey)
    if (previous) return previous
    const request = (async () => {
      try {
        const page = await ipc.expandNode(id, parentPath, offset, PAGE_SIZE)
        if (scanId.value !== id) return null
        return page
      } catch (pageError) {
        if (scanId.value === id) handleOperationError(pageError)
        return null
      } finally {
        pageRequests.delete(requestKey)
      }
    })()
    pageRequests.set(requestKey, request)
    return request
  }

  async function toggleNode(parentPath: string) {
    const id = scanId.value
    if (!id) return
    if (expandedKeys.value.has(parentPath)) {
      const next = new Set(expandedKeys.value)
      next.delete(parentPath)
      expandedKeys.value = next
      return
    }
    if (!pages.value[parentPath]) {
      const page = await requestPage(parentPath, 0, id)
      if (!page || scanId.value !== id) return
      mergePage(parentPath, page)
    }
    if (scanId.value !== id) return
    const next = new Set(expandedKeys.value)
    next.add(parentPath)
    expandedKeys.value = next
  }

  async function loadMore(parentPath: string) {
    const id = scanId.value
    const page = pages.value[parentPath]
    if (!id || !page || page.nextOffset === null) return
    const offset = page.nextOffset
    const key = `${id}:${parentPath}:${offset}`
    if (loadingPages.value.has(key)) return
    loadingPages.value = new Set(loadingPages.value).add(key)
    try {
      const nextPage = await requestPage(parentPath, offset, id)
      if (!nextPage || scanId.value !== id) return
      mergePage(parentPath, nextPage)
    } finally {
      const next = new Set(loadingPages.value)
      next.delete(key)
      loadingPages.value = next
    }
  }

  async function reveal(path: string) {
    const id = scanId.value
    if (!id) return
    try {
      const levels = await ipc.revealNode(id, path, PAGE_SIZE)
      if (scanId.value !== id) return
      for (const level of levels) {
        mergePage(level.parentPath, level.page)
        const next = new Set(expandedKeys.value)
        next.add(level.parentPath)
        expandedKeys.value = next
      }
      if (scanId.value === id) highlightedPath.value = path
    } catch (revealError) {
      if (scanId.value === id) handleOperationError(revealError)
    }
  }

  async function listRecommended() {
    const id = scanId.value
    if (!id) return
    try {
      const result = await ipc.listRecommended(id)
      if (scanId.value === id) recommended.value = result
    } catch (recommendedError) {
      if (scanId.value === id) handleOperationError(recommendedError)
    }
  }

  onScopeDispose(() => {
    progressUnlisten?.()
    invalidatedUnlisten?.()
    progressUnlisten = undefined
    invalidatedUnlisten = undefined
    pageRequests.clear()
  })

  return {
    roots, scanId, source, filteredRootCount, rootFileSummary, recommended,
    pages, expanded, expandedKeys, loadingPages, highlightedPath,
    invalidated, loading, cancelling, progress, error, hasScanned,
    initialize, clearSnapshot, scan, cancelScan, toggleNode,
    loadMore, reveal, listRecommended,
  }
})

export { PAGE_SIZE, failureDescription }
