<script setup lang="ts">
import { ref, onMounted, watch } from 'vue'
import { ipc } from '../ipc/invoke'
import type { HistoryEntry } from '../ipc/types'

const opFilter = ref<string>('')
const items = ref<HistoryEntry[]>([])

async function load() {
  items.value = await ipc.listHistory(opFilter.value || undefined)
}
onMounted(load)
watch(opFilter, load)

const opLabel: Record<string, string> = {
  migrate: '迁移', restore: '还原', break_link: '断开链接', delete_link: '删除链接',
}
const resultClass = (r: string) => r === 'ok' ? 'ok' : 'err'
</script>

<template>
  <div>
    <h2>操作历史</h2>
    <select v-model="opFilter">
      <option value="">全部</option>
      <option v-for="k in Object.keys(opLabel)" :key="k" :value="k">{{ opLabel[k] }}</option>
    </select>
    <table>
      <thead><tr><th>时间</th><th>操作</th><th>源</th><th>目标</th><th>结果</th></tr></thead>
      <tbody>
        <tr v-for="(h, i) in items" :key="i">
          <td>{{ h.time }}</td>
          <td>{{ opLabel[h.op] ?? h.op }}</td>
          <td>{{ h.src }}</td>
          <td>{{ h.dst }}</td>
          <td :class="resultClass(h.result)">{{ h.result }}</td>
        </tr>
      </tbody>
    </table>
  </div>
</template>
<style scoped>
.ok { color: #16a34a; } .err { color: #dc2626; }
</style>
