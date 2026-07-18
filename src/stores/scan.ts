import { defineStore } from 'pinia'
import { ref } from 'vue'
import { ipc } from '../ipc/invoke'
import type { ScanItem } from '../ipc/types'

export const useScanStore = defineStore('scan', () => {
  const items = ref<ScanItem[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function scan() {
    loading.value = true; error.value = null
    try { items.value = await ipc.scanDrives() }
    catch (e) { error.value = String(e) }
    finally { loading.value = false }
  }
  return { items, loading, error, scan }
})
