<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { ipc } from '../ipc/invoke'
import type { Config } from '../ipc/types'
import { Download, Folder, Plus, Save, Settings, Trash2, SlidersHorizontal } from '@lucide/vue'

const config = ref<Config | null>(null)
const saved = ref(false)
const exported = ref('')
const loading = ref(true)
const error = ref<string | null>(null)

onMounted(async () => {
  try { config.value = await ipc.getConfig() }
  catch (e) { error.value = String(e) }
  finally { loading.value = false }
})

async function save() {
  if (!config.value) return
  try {
    await ipc.saveConfig(config.value)
    saved.value = true
    setTimeout(() => (saved.value = false), 1500)
  } catch (e) { error.value = String(e) }
}

async function exportLog() {
  try { exported.value = await ipc.exportHistory() }
  catch (e) { error.value = String(e) }
}

function addExclude() { config.value?.scan.excludePaths.push('') }
function removeExclude(i: number) { config.value?.scan.excludePaths.splice(i, 1) }
</script>

<template>
  <div class="page settings-page">
    <header class="page-header"><div><p class="eyebrow">应用偏好</p><h2>设置</h2><p class="page-subtitle">配置迁移仓库、扫描范围和本地日志。</p></div></header>
    <div v-if="error" class="notice notice-error">{{ error }}</div>
    <section v-if="loading" class="section-empty">正在加载设置…</section>
    <template v-else-if="config">
      <section class="settings-section">
        <div class="settings-label"><div class="settings-icon"><Folder :size="17" /></div><div><strong>迁移仓库</strong><span>迁移后的数据统一存放位置</span></div></div>
        <div class="settings-control"><input v-model="config.repository" placeholder="D:/Migrated" /><p>仓库需位于本地 NTFS 非系统盘，且不能位于待迁移目录内部。</p></div>
      </section>
      <section class="settings-section">
        <div class="settings-label"><div class="settings-icon"><SlidersHorizontal :size="17" /></div><div><strong>大目录阈值</strong><span>小于该体积的目录不会显示</span></div></div>
        <div class="settings-control inline-control"><input type="number" min="0" v-model.number="config.scan.minSizeMb" /><span>MB</span></div>
      </section>
      <section class="settings-section align-start">
        <div class="settings-label"><div class="settings-icon"><Settings :size="17" /></div><div><strong>排除路径</strong><span>扫描时跳过以下目录</span></div></div>
        <div class="settings-control">
          <div v-for="(_, i) in config.scan.excludePaths" :key="i" class="path-row"><input v-model="config.scan.excludePaths[i]" /><button class="button button-secondary icon-button" title="删除路径" aria-label="删除路径" @click="removeExclude(i)"><Trash2 :size="14" /></button></div>
          <button class="button button-secondary" @click="addExclude"><Plus :size="14" /> 添加路径</button>
        </div>
      </section>
      <div class="settings-actions"><button class="button button-primary" @click="save"><Save :size="15" /> 保存设置</button><span v-if="saved" class="saved">已保存</span></div>
      <section class="settings-section">
        <div class="settings-label"><div class="settings-icon"><Download :size="17" /></div><div><strong>操作日志</strong><span>导出本机保存的完整记录</span></div></div>
        <div class="settings-control export-control"><button class="button button-secondary" @click="exportLog"><Download :size="14" /> 导出日志</button><details v-if="exported"><summary>查看日志内容</summary><pre>{{ exported }}</pre></details></div>
      </section>
    </template>
  </div>
</template>
<style scoped>
.settings-page { max-width: none; }
.settings-section { display: grid; grid-template-columns: minmax(220px, .8fr) minmax(300px, 1.2fr); align-items: center; gap: 34px; margin-bottom: 12px; }
.settings-section.align-start { align-items: flex-start; }
.settings-label { display: flex; align-items: center; gap: 11px; }
.settings-label strong, .settings-label span { display: block; }
.settings-label strong { color: var(--text-primary); font-size: 13px; }
.settings-label span { margin-top: 4px; color: var(--text-tertiary); font-size: 11px; }
.settings-icon { display: grid; width: 34px; height: 34px; flex: 0 0 34px; place-items: center; color: var(--accent-dark); border-radius: 9px; background: #e7f2ff; }
.settings-control input { width: 100%; }
.settings-control p { margin: 7px 0 0; color: var(--text-tertiary); font-size: 10px; line-height: 1.5; }
.inline-control { display: flex; align-items: center; gap: 9px; color: var(--text-tertiary); font-size: 12px; }
.inline-control input { width: 120px; }
.path-row { display: flex; gap: 7px; margin-bottom: 7px; }
.icon-button { width: 35px; padding: 0; }
.settings-actions { display: flex; align-items: center; gap: 12px; padding: 7px 0 22px; }
.saved { color: #18794e; font-size: 12px; }
.export-control { text-align: right; }
details { margin-top: 10px; text-align: left; color: var(--text-secondary); font-size: 11px; }
pre { max-height: 200px; overflow: auto; padding: 12px; border-radius: 7px; background: #f0f0f3; white-space: pre-wrap; }
.section-empty { padding: 40px; color: var(--text-tertiary); text-align: center; }
@media (max-width: 900px) { .settings-section { grid-template-columns: 1fr; gap: 15px; } }
</style>
