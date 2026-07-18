<script setup lang="ts">
import { onMounted } from 'vue'
import { useRouter } from 'vue-router'
import { useScanStore } from '../stores/scan'
import SizeCell from '../components/SizeCell.vue'

const store = useScanStore()
const router = useRouter()
onMounted(() => store.scan())

function migrate(item: { path: string; matchedPreset: string | null }) {
  // 选中目标后跳迁移页（传 path 与 presetId）
  router.push({ name: 'migrate', query: { src: item.path, presetId: item.matchedPreset ?? '' } })
}
</script>

<template>
  <div>
    <header><h2>扫描分析</h2>
      <button :disabled="store.loading" @click="store.scan()">
        {{ store.loading ? '扫描中…' : '重新扫描 C 盘' }}
      </button>
      <p v-if="store.error" class="err">{{ store.error }}</p>
    </header>
    <table>
      <thead><tr><th>名称</th><th>大小</th><th>类别</th><th>状态</th><th></th></tr></thead>
      <tbody>
        <tr v-for="it in store.items" :key="it.path">
          <td>{{ it.displayName }}<div class="path">{{ it.path }}</div></td>
          <td><SizeCell :bytes="it.sizeBytes" /></td>
          <td>{{ it.category ?? '自定义' }}</td>
          <td>
            <span v-if="it.isJunction" class="tag">已迁移(junction)</span>
            <span v-else-if="!it.autoMigrate" class="tag warn">需确认风险</span>
            <span v-else-if="it.inaccessible" class="tag err">无法访问</span>
          </td>
          <td>
            <button v-if="!it.isJunction" @click="migrate(it)">
              {{ it.autoMigrate ? '一键迁移' : '自定义迁移' }}
            </button>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>
<style scoped>
.path { font-size: 12px; color: #888; }
.tag { padding: 2px 6px; border-radius: 4px; background: #e5e7eb; font-size: 12px; }
.tag.warn { background: #fef3c7; } .tag.err { background: #fee2e2; }
</style>
