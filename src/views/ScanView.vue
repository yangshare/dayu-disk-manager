<script setup lang="ts">
import { computed } from 'vue'
import { useRouter } from 'vue-router'
import { useScanStore } from '../stores/scan'
import SizeCell from '../components/SizeCell.vue'
import { FolderSearch, RefreshCw, ArrowRight, HardDrive, ShieldCheck, Clock3 } from '@lucide/vue'

const store = useScanStore()
const router = useRouter()
const sortedItems = computed(() => [...store.items].sort((a, b) => b.sizeBytes - a.sizeBytes))

const categoryLabels: Record<string, string> = {
  communication: '通讯数据', game_library: '游戏库', dev_cache: '开发缓存',
  ide: '开发工具', container: '容器数据', app_install: '应用程序', custom: '自定义',
}

function migrate(item: { path: string; matchedPreset: string | null }) {
  // 选中目标后跳迁移页（传 path 与 presetId）
  router.push({ name: 'migrate', query: { src: item.path, presetId: item.matchedPreset ?? '' } })
}
</script>

<template>
  <div class="page scan-page">
    <header class="page-header">
      <div>
        <p class="eyebrow">存储概览</p>
        <h2>扫描分析</h2>
        <p class="page-subtitle">查找占用空间较大的目录，了解哪些内容适合迁移。</p>
      </div>
      <button class="button button-primary" :disabled="store.loading" @click="store.scan()">
        <RefreshCw :size="16" :class="{ spinning: store.loading }" />
        {{ store.loading ? '扫描中' : (store.hasScanned ? '重新扫描' : '开始扫描') }}
      </button>
    </header>

    <div v-if="store.error" class="notice notice-error">{{ store.error }}</div>

    <section v-if="!store.hasScanned" class="scan-empty">
      <div class="empty-icon"><FolderSearch :size="28" /></div>
      <h3>准备扫描你的磁盘</h3>
      <p>扫描会检查用户目录和程序文件夹，整个过程不会修改任何文件。</p>
      <button data-testid="start-scan" class="button button-primary button-large" @click="store.scan()">
        <FolderSearch :size="17" /> 开始扫描
      </button>
      <div class="empty-meta">
        <span><ShieldCheck :size="15" /> 只读分析</span>
        <span><Clock3 :size="15" /> 可随时返回</span>
      </div>
    </section>

    <section v-else class="results-panel">
      <div class="results-toolbar">
        <div class="result-count">
          <HardDrive :size="17" />
          <strong>{{ store.loading ? '正在分析目录…' : `${store.items.length} 个目录` }}</strong>
          <span v-if="!store.loading" class="muted">按占用空间排序</span>
        </div>
        <span v-if="store.loading" class="loading-label"><span class="loading-dot" /> 正在扫描</span>
      </div>
      <div v-if="!store.loading && store.items.length === 0" class="table-empty">
        <HardDrive :size="24" />
        <strong>没有发现需要关注的目录</strong>
        <span>可以在设置中调低大目录阈值后再次扫描。</span>
      </div>
      <div v-else class="table-wrap">
        <table>
      <thead><tr><th>名称</th><th>大小</th><th>类别</th><th>状态</th><th></th></tr></thead>
      <tbody>
        <tr v-for="it in sortedItems" :key="it.path">
          <td>{{ it.displayName }}<div class="path">{{ it.path }}</div></td>
          <td><SizeCell :bytes="it.sizeBytes" /></td>
          <td>{{ categoryLabels[it.category ?? 'custom'] ?? '自定义' }}</td>
          <td>
            <span v-if="it.isJunction" class="tag">已迁移(junction)</span>
            <span v-else-if="!it.autoMigrate" class="tag warn">需确认风险</span>
            <span v-else-if="it.inaccessible" class="tag err">无法访问</span>
          </td>
          <td>
            <button v-if="!it.isJunction" class="button button-quiet" @click="migrate(it)">
              {{ it.autoMigrate ? '一键迁移' : '自定义迁移' }}
              <ArrowRight :size="14" />
            </button>
          </td>
        </tr>
      </tbody>
        </table>
      </div>
    </section>
  </div>
</template>
<style scoped>
.path { font-size: 12px; color: var(--text-tertiary); margin-top: 4px; }
.tag { display: inline-flex; padding: 4px 8px; border-radius: 999px; background: var(--surface-muted); color: var(--text-secondary); font-size: 12px; white-space: nowrap; }
.tag.warn { background: #fff3d8; color: #a96800; } .tag.err { background: #ffe4e1; color: #b42318; }
.spinning { animation: spin 1s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
</style>
