import { defineStore } from 'pinia'
import { ref } from 'vue'
import { ipc } from '../ipc/invoke'
import { onProgress } from '../ipc/events'
import type { PrecheckReport, ProgressEvent } from '../ipc/types'

export const useMigrateStore = defineStore('migrate', () => {
  const report = ref<PrecheckReport | null>(null)
  const prechecking = ref(false)
  const error = ref<string | null>(null)
  const running = ref(false)
  const cancelling = ref(false)
  const progress = ref<ProgressEvent | null>(null)
  const result = ref<{ ok: boolean; message: string } | null>(null)
  // VSS 开关：仅在管理员权限下可用（precheck.vssAvailable）。可用时默认勾选，
  // 让迁移免疫被占用文件；不可用时锁定为 false。
  const enableVss = ref(false)
  let unlisten: (() => void) | null = null
  let activeTaskId: string | null = null

  async function precheck(src: string) {
    prechecking.value = true
    error.value = null
    report.value = null
    // A new source directory starts a distinct migration flow.
    progress.value = null
    result.value = null
    try {
      report.value = await ipc.precheckMigrate(src)
      enableVss.value = report.value.vssAvailable
    }
    catch (e) { error.value = String(e) }
    finally { prechecking.value = false }
  }

  async function initListener(taskId: string) {
    unlisten?.()
    activeTaskId = taskId
    unlisten = await onProgress((event) => {
      if (event.taskId === activeTaskId) progress.value = event
    })
  }

  async function run(migrationId: string, src: string, presetId: string | null) {
    if (running.value) return
    running.value = true
    cancelling.value = false
    progress.value = null
    result.value = null
    try {
      await initListener(`task-${migrationId}`)
      const migration = await ipc.startMigrate(migrationId, src, presetId, enableVss.value)
      result.value = migration.status === 'old_pending_delete'
        ? { ok: true, message: '迁移完成；旧目录未移入回收站，已保留等待清理。' }
        : { ok: true, message: '迁移完成' }
    } catch (e) {
      result.value = { ok: false, message: String(e) }
    } finally {
      running.value = false
      cancelling.value = false
    }
  }

  async function cancel() {
    if (!running.value || cancelling.value) return
    cancelling.value = true
    try { await ipc.cancelMigrate() }
    catch (e) {
      result.value = { ok: false, message: String(e) }
      cancelling.value = false
    }
  }

  function cleanup() {
    activeTaskId = null
    unlisten?.()
    unlisten = null
  }
  return { report, prechecking, error, running, cancelling, progress, result, enableVss, precheck, run, cancel, cleanup }
})
