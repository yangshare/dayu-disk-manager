<script setup lang="ts">
import { ref, onMounted, watch } from 'vue'
import { ipc } from '../ipc/invoke'
import type { HistoryEntry } from '../ipc/types'
import { History, ListFilter, RefreshCw, Clock3 } from '@lucide/vue'

const opFilter = ref<string>('')
const items = ref<HistoryEntry[]>([])
const loading = ref(false)
const error = ref<string | null>(null)

async function load() {
  loading.value = true; error.value = null
  try { items.value = await ipc.listHistory(opFilter.value || undefined) }
  catch (e) { error.value = String(e) }
  finally { loading.value = false }
}
onMounted(load)
watch(opFilter, load)

const opLabel: Record<string, string> = { migrate: '迁移', restore: '还原', break_link: '断开链接', delete_link: '删除链接' }
const resultLabel = (r: string) => r === 'ok' ? '成功' : '失败'
</script>

<template>
  <div class="page">
    <header class="page-header">
      <div><p class="eyebrow">活动记录</p><h2>操作历史</h2><p class="page-subtitle">所有迁移、还原和链接操作都会记录在本机。</p></div>
      <div class="header-actions">
        <label class="filter-control"><ListFilter :size="14" /><select v-model="opFilter" aria-label="操作类型"><option value="">全部操作</option><option v-for="k in Object.keys(opLabel)" :key="k" :value="k">{{ opLabel[k] }}</option></select></label>
        <button class="button button-secondary icon-button" title="刷新" aria-label="刷新" :disabled="loading" @click="load"><RefreshCw :size="15" :class="{ spinning: loading }" /></button>
      </div>
    </header>
    <div v-if="error" class="notice notice-error">{{ error }}</div>
    <section class="results-panel history-panel">
      <div class="results-toolbar"><div class="result-count"><History :size="17" /><strong>{{ items.length }} 条记录</strong></div></div>
      <div v-if="!loading && !items.length" class="table-empty"><Clock3 :size="24" /><strong>暂无操作记录</strong><span>完成一次迁移或还原后，记录会显示在这里。</span></div>
      <div v-else class="table-wrap"><table><thead><tr><th>时间</th><th>操作</th><th>源目录</th><th>目标目录</th><th>结果</th></tr></thead><tbody>
        <tr v-for="h in items" :key="`${h.id}-${h.time}-${h.op}`"><td class="time">{{ h.time }}</td><td>{{ opLabel[h.op] ?? h.op }}</td><td>{{ h.src }}</td><td>{{ h.dst }}</td><td><span class="tag" :class="h.result === 'ok' ? 'ok' : 'err'">{{ resultLabel(h.result) }}</span></td></tr>
      </tbody></table></div>
    </section>
  </div>
</template>
<style scoped>
.header-actions { display: flex; gap: 8px; }
.filter-control { display: flex; align-items: center; gap: 6px; min-height: 34px; padding-left: 10px; color: var(--text-tertiary); border: 1px solid var(--line); border-radius: 7px; background: white; }
.filter-control select { min-height: 32px; padding-left: 0; border: 0; box-shadow: none; background: transparent; }
.icon-button { width: 34px; padding: 0; }
.history-panel { padding: 0 !important; background: white !important; }
.time { white-space: nowrap; }
.tag.ok { color: #18794e; background: #e8f8ef; } .tag.err { color: #b42318; background: #ffe4e1; }
.spinning { animation: spin 1s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
</style>
