import { defineStore } from 'pinia'
import { ref } from 'vue'
import { ipc } from '../ipc/invoke'
import { onProgress } from '../ipc/events'
import type { LinkItem, ProgressEvent } from '../ipc/types'
import { logger } from '../ipc/log'

export const useLinksStore = defineStore('links', () => {
  const items = ref<LinkItem[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)
  const running = ref(false)
  const activeRestoreId = ref<string | null>(null)
  const progress = ref<ProgressEvent | null>(null)
  const result = ref<{ ok: boolean; message: string } | null>(null)
  let unlisten: (() => void) | null = null
  let activeTaskId: string | null = null

  async function refresh() {
    loading.value = true; error.value = null
    try { items.value = await ipc.listLinks() }
    catch (e) {
      logger.error(`加载链接列表失败: ${String(e)}`)
      error.value = String(e)
    }
    finally { loading.value = false }
  }

  async function initListener(taskId: string) {
    unlisten?.()
    activeTaskId = taskId
    unlisten = await onProgress((event) => {
      if (event.taskId === activeTaskId) progress.value = event
    })
  }

  async function restore(id: string) {
    if (running.value) return
    running.value = true
    activeRestoreId.value = id
    error.value = null
    progress.value = null
    result.value = null
    try {
      await initListener(`restore-${id}`)
      await ipc.startRestore(id, false)
      result.value = { ok: true, message: '还原完成，源目录已恢复为普通目录。' }
      await refresh()
    } catch (e) {
      logger.error(`还原失败: ${String(e)}`)
      result.value = { ok: false, message: `还原失败：${String(e)}` }
    } finally {
      running.value = false
      activeRestoreId.value = null
    }
  }

  async function breakLink(id: string) {
    if (running.value) return
    error.value = null
    try { await ipc.breakLink(id); await refresh() }
    catch (e) {
      logger.warn(`断开链接失败: ${String(e)}`)
      result.value = { ok: false, message: `断开失败：${String(e)}` }
    }
  }

  function cleanup() {
    activeTaskId = null
    unlisten?.()
    unlisten = null
  }

  return {
    items, loading, error, running, activeRestoreId, progress, result,
    refresh, restore, breakLink, cleanup,
  }
})
