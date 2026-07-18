import { defineStore } from 'pinia'
import { ref } from 'vue'
import { ipc } from '../ipc/invoke'
import type { LinkItem } from '../ipc/types'

export const useLinksStore = defineStore('links', () => {
  const items = ref<LinkItem[]>([])
  async function refresh() { items.value = await ipc.listLinks() }
  async function restore(id: string) {
    try { await ipc.startRestore(id); await refresh() }
    catch (e) { alert('还原失败: ' + String(e)) }
  }
  async function breakLink(id: string) {
    try { await ipc.breakLink(id); await refresh() }
    catch (e) { alert('断开失败: ' + String(e)) }
  }
  return { items, refresh, restore, breakLink }
})
