import { defineStore } from 'pinia'
import { ref } from 'vue'
import { ipc } from '../ipc/invoke'
import type { LinkItem } from '../ipc/types'

export const useLinksStore = defineStore('links', () => {
  const items = ref<LinkItem[]>([])
  async function refresh() { items.value = await ipc.listLinks() }
  async function restore(id: string) { await ipc.startRestore(id); await refresh() }
  async function breakLink(id: string) { await ipc.breakLink(id); await refresh() }
  return { items, refresh, restore, breakLink }
})
