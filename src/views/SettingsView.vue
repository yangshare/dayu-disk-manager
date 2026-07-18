<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { ipc } from '../ipc/invoke'
import type { Config } from '../ipc/types'

const config = ref<Config | null>(null)
const saved = ref(false)
const exported = ref('')

onMounted(async () => { config.value = await ipc.getConfig() })

async function save() {
  if (!config.value) return
  await ipc.saveConfig(config.value)
  saved.value = true
  setTimeout(() => (saved.value = false), 1500)
}

async function exportLog() {
  exported.value = await ipc.exportHistory()
}

function addExclude() { config.value?.scan.excludePaths.push('') }
function removeExclude(i: number) { config.value?.scan.excludePaths.splice(i, 1) }
</script>

<template>
  <div v-if="config">
    <h2>设置</h2>
    <section>
      <label>统一迁移仓库路径</label>
      <input v-model="config.repository" placeholder="D:/Migrated" />
      <p class="hint">仓库不能位于 C 盘、不能是网络路径、不能位于待迁源目录内部，所在盘需为本地 NTFS。</p>
    </section>
    <section>
      <label>大目录阈值 (MB)</label>
      <input type="number" v-model.number="config.scan.minSizeMb" />
    </section>
    <section>
      <label>扫描排除路径</label>
      <div v-for="(_, i) in config.scan.excludePaths" :key="i" class="row">
        <input v-model="config.scan.excludePaths[i]" />
        <button @click="removeExclude(i)">删除</button>
      </div>
      <button @click="addExclude">添加排除路径</button>
    </section>
    <section>
      <button @click="save">保存设置</button>
      <span v-if="saved" class="ok">已保存</span>
    </section>
    <section>
      <button @click="exportLog">导出操作日志</button>
      <details v-if="exported"><summary>日志内容</summary><pre>{{ exported }}</pre></details>
    </section>
  </div>
</template>
<style scoped>
section { margin-bottom: 16px; }
input { width: 100%; max-width: 480px; padding: 6px; }
.row { display: flex; gap: 8px; margin: 4px 0; }
.hint { font-size: 12px; color: #888; }
.ok { color: #16a34a; }
</style>
