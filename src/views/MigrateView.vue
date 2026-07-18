<script setup lang="ts">
import { onMounted, onUnmounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useMigrateStore } from '../stores/migrate'
import ProgressStage from '../components/ProgressStage.vue'
import SizeCell from '../components/SizeCell.vue'

const route = useRoute()
const router = useRouter()
const store = useMigrateStore()
const src = String(route.query.src ?? '')
const presetId = (route.query.presetId as string) || null
const migrationId = (crypto.randomUUID?.() ?? Date.now().toString())
onMounted(async () => { if (src) { await store.precheck(src) } })
onUnmounted(() => store.cleanup())

async function confirm() {
  await store.run(migrationId, src, presetId)
  if (store.result?.ok) router.push({ name: 'links' })
}
</script>

<template>
  <div>
    <h2>迁移</h2>
    <p>源：<code>{{ src }}</code></p>
    <div v-if="store.report">
      <h3>预检结果</h3>
      <p>源大小：<SizeCell :bytes="store.report.sourceSizeBytes" />　目标剩余：<SizeCell :bytes="store.report.targetFreeBytes" /></p>
      <ul>
        <li v-for="b in store.report.blockers" :key="b" class="block">⛔ {{ b }}</li>
        <li v-for="w in store.report.warnings" :key="w" class="warn">⚠️ {{ w }}</li>
      </ul>
      <button :disabled="!store.report.ok || store.running" @click="confirm()">
        {{ store.running ? '迁移中…' : (store.report.ok ? '确认迁移' : '存在阻断项，无法迁移') }}
      </button>
      <button v-if="store.running" @click="store.cancel()">取消</button>
    </div>
    <ProgressStage :progress="store.progress" />
    <p v-if="store.result" :class="store.result.ok ? 'ok' : 'err'">{{ store.result.message }}</p>
  </div>
</template>
<style scoped>
.block { color: #dc2626; } .warn { color: #d97706; }
.ok { color: #16a34a; } .err { color: #dc2626; }
</style>
