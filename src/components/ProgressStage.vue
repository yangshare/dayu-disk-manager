<script setup lang="ts">
import type { ProgressEvent } from '../ipc/types'
defineProps<{ progress: ProgressEvent | null }>()
const stageLabels: Record<string, string> = {
  copying: '复制中', verifying: '校验中', renaming_source: '改名源目录',
  syncing: '增量同步', creating_junction: '建立链接', recording: '记录映射',
  cleaning: '清理原目录', removing_junction: '删除链接', switching: '切换目录',
}
</script>
<template>
  <div v-if="progress" class="progress">
    <div class="bar"><div class="fill" :style="{ width: progress.percent + '%' }" /></div>
    <span>{{ stageLabels[progress.stage] ?? progress.stage }} — {{ progress.percent }}% — {{ progress.message }}</span>
  </div>
</template>
<style scoped>
.progress { margin: 12px 0; }
.bar { height: 8px; background: #e5e7eb; border-radius: 4px; overflow: hidden; }
.fill { height: 100%; background: #3b82f6; transition: width .2s; }
</style>
