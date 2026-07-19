<script setup lang="ts">
import { onMounted, onUnmounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useMigrateStore } from '../stores/migrate'
import ProgressStage from '../components/ProgressStage.vue'
import SizeCell from '../components/SizeCell.vue'
import { ArrowLeft, ArrowRight, CheckCircle2, CircleAlert, FolderKanban, LoaderCircle, X } from '@lucide/vue'

const route = useRoute()
const router = useRouter()
const store = useMigrateStore()
const src = String(route.query.src ?? '')
const presetId = (route.query.presetId as string) || null
const migrationId = (crypto.randomUUID?.() ?? Date.now().toString())
onMounted(async () => { if (src) await store.precheck(src) })
onUnmounted(() => store.cleanup())

async function confirm() {
  await store.run(migrationId, src, presetId)
  if (store.result?.ok) router.push({ name: 'links' })
}
</script>

<template>
  <div class="page">
    <header class="page-header">
      <div>
        <p class="eyebrow">安全操作</p>
        <h2>迁移</h2>
        <p class="page-subtitle">将目录移动到统一仓库，并在原位置保留透明的软链接。</p>
      </div>
      <button class="button button-secondary" @click="router.push('/scan')"><ArrowLeft :size="15" /> 返回扫描</button>
    </header>

    <section v-if="!src" class="section-empty">
      <FolderKanban :size="25" />
      <strong>还没有选择目录</strong>
      <span>请先从扫描分析中选择一个目录开始迁移。</span>
      <button class="button button-primary" @click="router.push('/scan')">前往扫描 <ArrowRight :size="15" /></button>
    </section>

    <template v-else>
      <section class="source-card">
        <div class="section-heading"><span>迁移源目录</span><span class="step-badge">1</span></div>
        <code>{{ src }}</code>
      </section>

      <div v-if="store.error" class="notice notice-error">{{ store.error }}</div>
      <section v-if="store.prechecking" class="section-empty compact">
        <LoaderCircle class="spinning" :size="22" /> <span>正在检查目录和目标磁盘…</span>
      </section>

      <section v-if="store.report" class="precheck-card">
        <div class="section-heading"><span>迁移预检</span><span class="step-badge">2</span></div>
        <div class="metrics">
          <div><span>源目录大小</span><strong><SizeCell :bytes="store.report.sourceSizeBytes" /></strong></div>
          <div><span>目标可用空间</span><strong><SizeCell :bytes="store.report.targetFreeBytes" /></strong></div>
        </div>
        <ul v-if="store.report.blockers.length || store.report.warnings.length" class="check-list">
          <li v-for="b in store.report.blockers" :key="b" class="check-block"><CircleAlert :size="16" /> {{ b }}</li>
          <li v-for="w in store.report.warnings" :key="w" class="check-warning"><CircleAlert :size="16" /> {{ w }}</li>
        </ul>
        <div v-else class="check-ok"><CheckCircle2 :size="17" /> 目录和目标磁盘均已通过检查</div>
        <div class="action-row">
          <button class="button button-primary" :disabled="!store.report.ok || store.running" @click="confirm">
            <LoaderCircle v-if="store.running" class="spinning" :size="15" />
            <ArrowRight v-else :size="15" />
            {{ store.running ? '迁移中' : (store.report.ok ? '确认迁移' : '存在阻断项') }}
          </button>
          <button v-if="store.running" class="button button-secondary" @click="store.cancel"><X :size="15" /> 取消</button>
        </div>
      </section>

      <ProgressStage :progress="store.progress" />
      <div v-if="store.result" class="notice" :class="store.result.ok ? 'notice-success' : 'notice-error'">{{ store.result.message }}</div>
    </template>
  </div>
</template>

<style scoped>
.source-card, .precheck-card { margin-bottom: 14px; }
.section-heading { display: flex; align-items: center; justify-content: space-between; margin-bottom: 15px; color: var(--text-primary); font-size: 13px; font-weight: 650; }
.step-badge { display: grid; width: 21px; height: 21px; place-items: center; color: var(--accent-dark); border-radius: 50%; background: #e2efff; font-size: 11px; }
.source-card code { display: block; overflow: hidden; padding: 11px 12px; text-overflow: ellipsis; white-space: nowrap; border: 1px solid var(--line); border-radius: 7px; background: white; }
.metrics { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 10px; margin-bottom: 17px; }
.metrics div { display: flex; flex-direction: column; gap: 7px; padding: 12px; border-radius: 8px; background: white; }
.metrics span { color: var(--text-tertiary); font-size: 11px; }
.metrics strong { color: var(--text-primary); font-size: 14px; }
.check-list { display: flex; flex-direction: column; gap: 8px; margin: 0 0 17px; padding: 0; list-style: none; font-size: 12px; }
.check-list li, .check-ok { display: flex; align-items: center; gap: 7px; }
.check-block { color: #b42318; } .check-warning { color: #a96800; }
.check-ok { margin-bottom: 17px; color: #18794e; font-size: 12px; }
.action-row { display: flex; gap: 8px; }
.section-empty { display: flex; flex-direction: column; align-items: center; gap: 11px; padding: 64px 20px; color: var(--text-tertiary); text-align: center; }
.section-empty strong { color: var(--text-secondary); font-size: 14px; }
.section-empty span { margin-bottom: 8px; font-size: 12px; }
.section-empty.compact { flex-direction: row; justify-content: center; padding: 28px; }
.notice-success { color: #18794e; background: #edf9f2; border: 1px solid #c9efd9; }
.spinning { animation: spin 1s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
</style>
