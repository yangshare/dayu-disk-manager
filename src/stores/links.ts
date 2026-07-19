import { defineStore } from 'pinia'
import { ref } from 'vue'
import { ipc } from '../ipc/invoke'
import type { LinkItem } from '../ipc/types'

export const useLinksStore = defineStore('links', () => {
  const items = ref<LinkItem[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)
  async function refresh() {
    loading.value = true; error.value = null
    try { items.value = await ipc.listLinks() }
    catch (e) { error.value = String(e) }
    finally { loading.value = false }
  }
  async function restore(id: string) {
    try { await ipc.startRestore(id); await refresh() }
    catch (e) { alert('还原失败: ' + String(e)) }
  }
  async function breakLink(id: string) {
    try { await ipc.breakLink(id); await refresh() }
    catch (e) { alert('断开失败: ' + String(e)) }
  }
  return { items, loading, error, refresh, restore, breakLink }
})
