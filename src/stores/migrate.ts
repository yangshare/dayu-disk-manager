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
  const progress = ref<ProgressEvent | null>(null)
  const result = ref<{ ok: boolean; message: string } | null>(null)
  let unlisten: (() => void) | null = null

  async function precheck(src: string) {
    prechecking.value = true
    error.value = null
    report.value = null
    try { report.value = await ipc.precheckMigrate(src) }
    catch (e) { error.value = String(e) }
    finally { prechecking.value = false }
  }

  async function initListener() {
    if (!unlisten) unlisten = await onProgress((e) => { progress.value = e })
  }

  async function run(migrationId: string, src: string, presetId: string | null) {
    await initListener()
    running.value = true; result.value = null
    try {
      await ipc.startMigrate(migrationId, src, presetId)
      result.value = { ok: true, message: '迁移完成' }
    } catch (e) {
      result.value = { ok: false, message: String(e) }
    } finally {
      running.value = false
    }
  }

  function cancel() { ipc.cancelMigrate() }

  function cleanup() { unlisten?.(); unlisten = null }
  return { report, prechecking, error, running, progress, result, precheck, run, cancel, cleanup }
})
