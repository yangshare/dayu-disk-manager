<script setup lang="ts">
import { onMounted } from 'vue'
import { useLinksStore } from '../stores/links'
const store = useLinksStore()
onMounted(() => store.refresh())

function onBreak(id: string) {
  if (window.confirm('断开后原路径将不可用，确认？')) store.breakLink(id)
}
</script>

<template>
  <div>
    <h2>软链接管理</h2>
    <button @click="store.refresh()">刷新</button>
    <table>
      <thead><tr><th>源(原路径)</th><th>目标(数据)</th><th>状态</th><th>操作</th></tr></thead>
      <tbody>
        <tr v-for="l in store.items" :key="l.id">
          <td>{{ l.source }}</td>
          <td>{{ l.target }}</td>
          <td>
            <span v-if="l.broken" class="tag err">失效(目标缺失)</span>
            <span v-else-if="!l.valid" class="tag warn">链接无效</span>
            <span v-else class="tag ok">正常</span>
          </td>
          <td>
            <button :disabled="l.broken" @click="store.restore(l.id)">还原</button>
            <button @click="onBreak(l.id)">断开</button>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>
<style scoped>
.tag { padding: 2px 6px; border-radius: 4px; font-size: 12px; }
.tag.ok { background: #dcfce7; color: #16a34a; }
.tag.warn { background: #fef3c7; color: #d97706; }
.tag.err { background: #fee2e2; color: #dc2626; }
</style>
