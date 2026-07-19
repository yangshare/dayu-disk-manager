import { defineStore } from 'pinia'
import { ref } from 'vue'
import type { UnlistenFn } from '@tauri-apps/api/event'
import { ipc } from '../ipc/invoke'
import { onScanProgress } from '../ipc/events'
import type { ScanItem, ScanProgressEvent } from '../ipc/types'

export const useScanStore = defineStore('scan', () => {
  const items = ref<ScanItem[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)
  const hasScanned = ref(false)
  const cancelling = ref(false)
  const progress = ref<ScanProgressEvent | null>(null)

  async function scan() {
    if (loading.value) return
    hasScanned.value = true
    loading.value = true
    cancelling.value = false
    progress.value = null
    error.value = null
    let unlisten: UnlistenFn | undefined
    try {
      try { unlisten = await onScanProgress((event) => { progress.value = event }) }
      catch { /* Scanning still works if the progress channel is unavailable. */ }
      items.value = await ipc.scanDrives()
    } catch (e) {
      const message = String(e)
      if (!message.includes('用户取消')) error.value = message
    } finally {
      unlisten?.()
      loading.value = false
      cancelling.value = false
    }
  }

  async function cancelScan() {
    if (!loading.value || cancelling.value) return
    cancelling.value = true
    try { await ipc.cancelScan() }
    catch (e) {
      error.value = String(e)
      cancelling.value = false
    }
  }
  return { items, loading, error, hasScanned, cancelling, progress, scan, cancelScan }
})
