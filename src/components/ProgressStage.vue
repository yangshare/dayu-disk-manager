<script setup lang="ts">
import { computed } from 'vue'
import { Check, Circle, LoaderCircle } from '@lucide/vue'
import type { ProgressEvent } from '../ipc/types'
import { formatSize } from '../ipc/types'

const props = withDefaults(defineProps<{
  progress: ProgressEvent | null
  operation?: 'migrate' | 'restore'
}>(), {
  operation: 'migrate',
})

const migrationStages = [
  { key: 'copying', label: '复制文件' },
  { key: 'verifying', label: '校验数据' },
  { key: 'renaming_source', label: '切换源目录' },
  { key: 'syncing', label: '同步变化' },
  { key: 'creating_junction', label: '建立链接' },
  { key: 'recording', label: '保存记录' },
  { key: 'cleaning', label: '完成清理' },
]

const restoreStages = [
  { key: 'copying', label: '复制回源盘' },
  { key: 'verifying', label: '校验数据' },
  { key: 'removing_junction', label: '移除链接' },
  { key: 'switching', label: '恢复普通目录' },
  { key: 'cleaning', label: '完成清理' },
]

const stages = computed(() => props.operation === 'restore' ? restoreStages : migrationStages)
const stageTrackStyle = computed(() => ({
  gridTemplateColumns: `repeat(${stages.value.length}, minmax(78px, 1fr))`,
  minWidth: `${stages.value.length * 92}px`,
}))

const currentStageIndex = computed(() => {
  const index = stages.value.findIndex((stage) => stage.key === props.progress?.stage)
  return index < 0 ? 0 : index
})

const safePercent = computed(() => Math.min(100, Math.max(0, props.progress?.percent ?? 0)))
type Transfer = NonNullable<ProgressEvent['transfer']>

function formatByteProgress(transfer: Transfer) {
  const completed = formatSize(transfer.completedBytes)
  return transfer.totalBytes === undefined ? completed : `${completed} / ${formatSize(transfer.totalBytes)}`
}

function formatFileProgress(transfer: Transfer) {
  const completed = transfer.completedFiles.toLocaleString()
  return transfer.totalFiles === undefined ? completed : `${completed} / ${transfer.totalFiles.toLocaleString()}`
}

function stageState(index: number) {
  if (safePercent.value === 100 || index < currentStageIndex.value) return 'completed'
  if (index === currentStageIndex.value) return 'current'
  return 'pending'
}
</script>

<template>
  <section v-if="progress" class="progress-panel" aria-live="polite">
    <div class="progress-header">
      <div class="status-mark"><LoaderCircle :size="18" /></div>
      <div class="status-copy">
        <strong>{{ progress.message }}</strong>
        <span>{{ progress.transfer?.phase === 'preparing' ? '准备阶段' : (operation === 'restore' ? '还原任务正在后台执行' : '迁移任务正在后台执行') }}</span>
      </div>
      <div class="percent"><strong>{{ safePercent }}%</strong><span>总进度</span></div>
    </div>

    <div class="bar" role="progressbar" :aria-valuenow="safePercent" aria-valuemin="0" aria-valuemax="100">
      <div class="fill" :style="{ width: `${safePercent}%` }" />
    </div>

    <div v-if="progress.transfer" class="transfer-details">
      <div>
        <span>{{ progress.transfer.phase === 'preparing' ? '已统计大小' : '传输数据' }}</span>
        <strong>{{ formatByteProgress(progress.transfer) }}</strong>
      </div>
      <div>
        <span>{{ progress.transfer.phase === 'preparing' ? '已发现文件' : '文件进度' }}</span>
        <strong>{{ formatFileProgress(progress.transfer) }}</strong>
      </div>
    </div>

    <div v-if="progress.transfer?.currentPath" class="current-path">
      <span>当前处理</span>
      <code :title="progress.transfer.currentPath">{{ progress.transfer.currentPath }}</code>
    </div>

    <div class="stage-scroll">
      <ol class="stage-track" :style="stageTrackStyle">
        <li v-for="(stage, index) in stages" :key="stage.key" :class="stageState(index)">
          <span class="stage-icon">
            <Check v-if="stageState(index) === 'completed'" :size="12" />
            <LoaderCircle v-else-if="stageState(index) === 'current'" :size="13" />
            <Circle v-else :size="11" />
          </span>
          <span>{{ stage.label }}</span>
        </li>
      </ol>
    </div>
  </section>
</template>

<style scoped>
.progress-panel { overflow: hidden; background: #fff; box-shadow: 0 7px 22px rgba(32, 34, 45, .05); }
.progress-header { display: grid; grid-template-columns: 34px minmax(0, 1fr) auto; align-items: center; gap: 11px; }
.status-mark { display: grid; width: 32px; height: 32px; place-items: center; color: var(--accent); border-radius: 50%; background: #e7f2ff; }
.status-mark svg { animation: spin 1s linear infinite; }
.status-copy { min-width: 0; }
.status-copy strong, .status-copy span { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.status-copy strong { color: var(--text-primary); font-size: 13px; }
.status-copy span { margin-top: 4px; color: var(--text-tertiary); font-size: 11px; }
.percent { min-width: 58px; text-align: right; }
.percent strong, .percent span { display: block; }
.percent strong { color: var(--accent-dark); font-size: 18px; }
.percent span { margin-top: 2px; color: var(--text-tertiary); font-size: 10px; }
.bar { height: 7px; margin-top: 17px; overflow: hidden; border-radius: 4px; background: #e9e9ed; }
.fill { height: 100%; border-radius: inherit; background: var(--accent); transition: width .2s ease; }
.transfer-details { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 1px; margin-top: 15px; overflow: hidden; border: 1px solid var(--line); border-radius: 7px; background: var(--line); }
.transfer-details div { min-width: 0; padding: 10px 12px; background: var(--surface-soft); }
.transfer-details span, .transfer-details strong { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.transfer-details span { margin-bottom: 5px; color: var(--text-tertiary); font-size: 10px; }
.transfer-details strong { color: var(--text-secondary); font-size: 12px; font-weight: 650; }
.current-path { display: grid; grid-template-columns: auto minmax(0, 1fr); align-items: center; gap: 10px; min-height: 34px; margin-top: 10px; padding: 0 10px; border-radius: 6px; background: var(--surface-muted); }
.current-path span { color: var(--text-tertiary); font-size: 10px; white-space: nowrap; }
.current-path code { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.stage-scroll { margin-top: 18px; overflow-x: auto; }
.stage-track { display: grid; margin: 0; padding: 0; list-style: none; }
.stage-track li { position: relative; display: flex; flex-direction: column; align-items: center; gap: 7px; color: var(--text-tertiary); font-size: 10px; text-align: center; }
.stage-track li::before { position: absolute; z-index: 0; top: 10px; right: 50%; left: -50%; height: 1px; content: ''; background: #dedee4; }
.stage-track li:first-child::before { display: none; }
.stage-icon { z-index: 1; display: grid; width: 21px; height: 21px; place-items: center; border-radius: 50%; color: #a1a1a8; background: #f1f1f4; }
.stage-track li.completed { color: #18794e; }
.stage-track li.completed::before { background: #8bd2ae; }
.stage-track li.completed .stage-icon { color: #fff; background: #279668; }
.stage-track li.current { color: var(--accent-dark); font-weight: 650; }
.stage-track li.current::before { background: #83baff; }
.stage-track li.current .stage-icon { color: #fff; background: var(--accent); }
.stage-track li.current .stage-icon svg { animation: spin 1s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

@media (max-width: 680px) {
  .transfer-details { grid-template-columns: 1fr; }
  .current-path { grid-template-columns: 1fr; gap: 4px; padding-top: 8px; padding-bottom: 8px; }
}
</style>
