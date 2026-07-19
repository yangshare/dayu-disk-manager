<script setup lang="ts">
import { onMounted } from 'vue'
import { useLinksStore } from '../stores/links'
import { Link2, RefreshCw, RotateCcw, Unlink, FolderOpen } from '@lucide/vue'
const store = useLinksStore()
onMounted(() => store.refresh())

function onBreak(id: string) {
  if (window.confirm('断开后原路径将不可用，确认？')) store.breakLink(id)
}
</script>

<template>
  <div class="page">
    <header class="page-header">
      <div><p class="eyebrow">迁移映射</p><h2>软链接管理</h2><p class="page-subtitle">查看已迁移目录的连接状态，需要时还原或断开链接。</p></div>
      <button class="button button-secondary" :disabled="store.loading" @click="store.refresh"><RefreshCw :size="15" :class="{ spinning: store.loading }" /> 刷新</button>
    </header>
    <div v-if="store.error" class="notice notice-error">{{ store.error }}</div>
    <section class="results-panel links-panel">
      <div class="results-toolbar"><div class="result-count"><Link2 :size="17" /><strong>{{ store.items.length }} 条链接</strong></div></div>
      <div v-if="!store.loading && !store.items.length" class="table-empty"><FolderOpen :size="24" /><strong>还没有迁移记录</strong><span>从扫描分析中选择目录后，迁移记录会显示在这里。</span></div>
      <div v-else class="table-wrap">
        <table><thead><tr><th>源目录</th><th>目标目录</th><th>状态</th><th>操作</th></tr></thead><tbody>
          <tr v-for="l in store.items" :key="l.id">
            <td><strong>{{ l.source }}</strong><div class="path">{{ l.createdAt }}</div></td><td>{{ l.target }}</td>
            <td><span v-if="l.broken" class="tag err">目标缺失</span><span v-else-if="!l.valid" class="tag warn">链接无效</span><span v-else class="tag ok">正常</span></td>
            <td class="actions"><button class="button button-quiet" :disabled="l.broken" @click="store.restore(l.id)"><RotateCcw :size="14" /> 还原</button><button class="button button-secondary" @click="onBreak(l.id)"><Unlink :size="14" /> 断开</button></td>
          </tr>
        </tbody></table>
      </div>
    </section>
  </div>
</template>
<style scoped>
.links-panel { padding: 0 !important; background: var(--surface) !important; }
.actions { display: flex; gap: 7px; white-space: nowrap; }
.tag.ok { color: #18794e; background: #e8f8ef; } .tag.warn { color: #a96800; background: #fff3d8; } .tag.err { color: #b42318; background: #ffe4e1; }
.spinning { animation: spin 1s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
</style>
