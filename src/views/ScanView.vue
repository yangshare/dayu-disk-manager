<script setup lang="ts">
import { computed, nextTick } from 'vue'
import { useRouter } from 'vue-router'
import { useScanStore } from '../stores/scan'
import SizeCell from '../components/SizeCell.vue'
import {
  FolderSearch, RefreshCw, ArrowRight, HardDrive, ShieldCheck, Clock3, Square, ChevronDown,
  Link2, Sparkles, AlertTriangle, Info, LoaderCircle,
} from '@lucide/vue'
import type { ScanItemStatus, TreeNode } from '../ipc/types'

// 树视图与 brief :5/:7 共同点：行 key 用 node path（绝不用数组 index）。
interface NodeRow {
  kind: 'node'
  node: TreeNode
  depth: number
  parentPath: string | null
}
interface MoreRow {
  kind: 'more'
  parentPath: string
  depth: number
}
type FlatRow = NodeRow | MoreRow

const store = useScanStore()
const router = useRouter()

// 节点 DOM 引用，用于 reveal() 定位后 scrollIntoView。
const rowElements = new Map<string, HTMLElement>()
function setRowRef(el: Element | unknown, path: string) {
  if (el instanceof HTMLElement) rowElements.set(path, el)
  else rowElements.delete(path)
}

const categoryLabels: Record<string, string> = {
  communication: '通讯数据', game_library: '游戏库', dev_cache: '开发缓存',
  ide: '开发工具', container: '容器数据', app_install: '应用程序', custom: '自定义',
}

const statusLabels: Record<ScanItemStatus, string> = {
  migrated: '已迁移',
  migration_pending: '迁移待处理',
  link_broken: '链接异常',
  existing_link: '已有软链接',
  contains_migrated: '包含已迁移目录',
  contains_link: '包含软链接目录',
}

const managedStatuses = new Set<ScanItemStatus>([
  'migrated', 'migration_pending', 'link_broken', 'contains_migrated',
])

// DFS 把当前可见节点拍平 + 在每层 children 末尾插入 more-row（如果 nextOffset 非 null）。
const flatRows = computed<FlatRow[]>(() => {
  const rows: FlatRow[] = []
  const expanded = store.expandedKeys
  const pages = store.pages
  function walk(node: TreeNode, depth: number, parentPath: string | null) {
    rows.push({ kind: 'node', node, depth, parentPath })
    if (!expanded.has(node.path)) return
    const page = pages[node.path]
    if (!page) return
    for (const child of page.items) walk(child, depth + 1, node.path)
    if (page.nextOffset !== null) {
      rows.push({ kind: 'more', parentPath: node.path, depth: depth + 1 })
    }
  }
  for (const root of store.roots) walk(root, 0, null)
  return rows
})

// brief :14 区分两种状态：
// - invalidated + 未在重扫：结果已失效（filesystem 失效不重扫 → source=null 但 invalidated=true）
// - loading + mft（且已扫过）：MFT 自动重扫中
const showInvalidatedNotice = computed(() => store.invalidated && !store.loading)
const showRescanningNotice = computed(() => store.loading && store.source === 'mft' && store.hasScanned && !store.invalidated)

// brief :9 红线：reparse / existing_link / inaccessible 不显示"一键/自定义迁移"误导入口。
function showMigrateAction(node: TreeNode): boolean {
  if (node.isReparse) return false
  if (node.scanStatus === 'existing_link') return false
  if (node.accessState === 'inaccessible') return false
  return true
}

function showStatusTag(node: TreeNode): boolean {
  return node.scanStatus !== null
}

function showInaccessibleTag(node: TreeNode): boolean {
  return node.scanStatus === null && node.accessState === 'inaccessible'
}

function showAutoMigrateWarning(node: TreeNode): boolean {
  if (node.scanStatus !== null) return false
  if (node.accessState === 'inaccessible') return false
  return node.autoMigrate === false
}

function toggleNode(path: string) {
  void store.toggleNode(path)
}

function loadMore(parentPath: string) {
  void store.loadMore(parentPath)
}

// brief :11 reveal 定位：await + nextTick + scrollIntoView + 高亮 class（CSS 动画短暂）。
async function revealNode(path: string) {
  await store.reveal(path)
  await nextTick()
  const el = rowElements.get(path)
  if (el && typeof el.scrollIntoView === 'function') {
    el.scrollIntoView({ block: 'center', behavior: 'smooth' })
  }
}

function migrate(node: TreeNode) {
  router.push({ name: 'migrate', query: { src: node.path, presetId: node.matchedPreset ?? '' } })
}

function viewLinks() {
  router.push({ name: 'links' })
}

function resumeScan() {
  void store.scan()
}

function cancelScan() {
  void store.cancelScan()
}

function isHighlighted(path: string): boolean {
  return store.highlightedPath === path
}

function encodeId(path: string): string {
  return encodeURIComponent(path)
}
</script>

<template>
  <div class="page scan-page">
    <header class="page-header">
      <div>
        <p class="eyebrow">存储概览</p>
        <h2>扫描分析</h2>
        <p class="page-subtitle">查找占用空间较大的目录，按树形结构浏览、了解哪些内容适合迁移。</p>
      </div>
      <div class="header-actions">
        <button v-if="store.loading" class="button button-secondary" :disabled="store.cancelling" @click="cancelScan">
          <Square :size="14" /> {{ store.cancelling ? '正在停止' : '停止扫描' }}
        </button>
        <button v-else class="button button-primary" @click="resumeScan">
          <RefreshCw :size="16" /> {{ store.hasScanned ? '重新扫描' : '开始扫描' }}
        </button>
      </div>
    </header>

    <div v-if="store.error" class="notice notice-error">{{ store.error }}</div>

    <section v-if="!store.hasScanned" class="scan-empty">
      <div class="empty-icon"><FolderSearch :size="28" /></div>
      <h3>准备扫描你的磁盘</h3>
      <p>扫描会检查用户目录和程序文件夹，整个过程不会修改任何文件。</p>
      <button data-testid="start-scan" class="button button-primary button-large" @click="resumeScan">
        <FolderSearch :size="17" /> 开始扫描
      </button>
      <div class="empty-meta">
        <span><ShieldCheck :size="15" /> 只读分析</span>
        <span><Clock3 :size="15" /> 可随时返回</span>
      </div>
    </section>

    <template v-else>
      <div v-if="showInvalidatedNotice" class="notice notice-error" data-testid="invalidated-notice">
        <AlertTriangle :size="15" /> 结果已失效，请重新扫描。
      </div>
      <div v-else-if="showRescanningNotice" class="notice notice-info" data-testid="rescanning-notice">
        <LoaderCircle :size="15" class="spinning" /> 正在重扫…
      </div>

      <section v-if="store.rootFileSummary" class="root-summary" data-testid="root-summary">
        <div class="root-summary-head">
          <span class="root-summary-title"><Info :size="15" /> 根文件汇总</span>
          <span v-if="store.rootFileSummary.incomplete" class="tag warn" data-testid="root-summary-incomplete">
            可能不完整
          </span>
        </div>
        <dl class="root-summary-grid">
          <div><dt>根级文件</dt><dd>{{ store.rootFileSummary.directFileCount }} 个</dd></div>
          <div><dt>根级大小</dt><dd><SizeCell :bytes="store.rootFileSummary.directFileSizeBytes" /></dd></div>
          <div><dt>元数据</dt><dd>
            <SizeCell v-if="store.rootFileSummary.systemMetadataSizeBytes !== null" :bytes="store.rootFileSummary.systemMetadataSizeBytes" />
            <span v-else class="muted">未记录</span>
          </dd></div>
          <div><dt>已知总占用</dt><dd><SizeCell :bytes="store.rootFileSummary.totalKnownSizeBytes" /></dd></div>
        </dl>
      </section>

      <section v-if="store.recommended.length > 0" class="recommended-panel" data-testid="recommended-panel">
        <div class="recommended-head">
          <Sparkles :size="16" />
          <strong>推荐迁移</strong>
          <span class="muted">{{ store.recommended.length }} 个目录</span>
        </div>
        <ul class="recommended-list">
          <li v-for="rec in store.recommended" :key="rec.path" :data-path="rec.path">
            <div class="recommended-name">
              <strong>{{ rec.displayName }}</strong>
              <span v-if="!rec.autoMigrate" class="tag warn">需确认风险</span>
              <span v-else class="tag auto">一键迁移</span>
              <span class="muted path-inline">{{ rec.path }}</span>
            </div>
            <div class="recommended-meta">
              <SizeCell :bytes="rec.sizeBytes" />
              <span v-if="rec.matchedPreset" class="muted">匹配预设：{{ rec.matchedPreset }}</span>
            </div>
            <button class="button button-quiet" :data-testid="`reveal-${encodeId(rec.path)}`" @click="revealNode(rec.path)">
              定位 <ArrowRight :size="14" />
            </button>
          </li>
        </ul>
      </section>

      <section class="results-panel">
        <div class="results-toolbar">
          <div class="result-count">
            <HardDrive :size="17" />
            <strong>
              <template v-if="store.loading">已扫描 {{ store.progress?.scannedDirs ?? 0 }} 个目录</template>
              <template v-else>{{ store.roots.length }} 个根目录</template>
            </strong>
            <span v-if="!store.loading && store.filteredRootCount > 0" class="muted" data-testid="filtered-roots">
              一级隐藏 {{ store.filteredRootCount }} 个目录
            </span>
          </div>
          <span v-if="store.loading" class="loading-label">
            <span class="loading-dot" /> {{ store.progress?.scannedFiles ?? 0 }} 个文件
          </span>
        </div>

        <div v-if="!store.loading && store.roots.length === 0" class="table-empty" data-testid="threshold-empty">
          <HardDrive :size="24" />
          <strong>没有发现需要关注的目录</strong>
          <span>可以在设置中调低大目录阈值后再次扫描。</span>
        </div>

        <div v-else class="table-wrap" data-testid="tree-wrap">
          <table>
            <thead><tr><th>名称</th><th>大小</th><th>类别</th><th>状态</th><th></th></tr></thead>
            <tbody>
              <template v-for="row in flatRows" :key="row.kind === 'node' ? row.node.path : `more:${row.parentPath}`">
                <tr
                  v-if="row.kind === 'node'"
                  :data-path="row.node.path"
                  :data-row-id="encodeId(row.node.path)"
                  :data-depth="row.depth"
                  :class="['tree-row', { highlighted: isHighlighted(row.node.path) }]"
                  :ref="(el: Element | unknown) => setRowRef(el, row.node.path)"
                >
                  <td class="cell-name">
                    <span class="indent-spacer" :style="{ width: `${row.depth * 18}px` }" />
                    <button
                      v-if="row.node.childCount > 0"
                      class="caret"
                      :aria-label="store.expandedKeys.has(row.node.path) ? '折叠' : '展开'"
                      :data-testid="`caret-${encodeId(row.node.path)}`"
                      @click="toggleNode(row.node.path)"
                    >
                      <ChevronDown :size="14" :class="{ expanded: store.expandedKeys.has(row.node.path) }" />
                    </button>
                    <span v-else class="caret-spacer" />
                    <span class="name-text">
                      <strong>{{ row.node.displayName }}</strong>
                      <span class="path" :title="row.node.path">{{ row.node.path }}</span>
                    </span>
                  </td>
                  <td><SizeCell :bytes="row.node.sizeBytes" /></td>
                  <td>{{ categoryLabels[row.node.category ?? 'custom'] ?? '自定义' }}</td>
                  <td>
                    <span v-if="showStatusTag(row.node)" class="tag" :class="row.node.scanStatus!">
                      {{ statusLabels[row.node.scanStatus!] }}
                    </span>
                    <span v-else-if="showInaccessibleTag(row.node)" class="tag err">无法访问</span>
                    <span v-else-if="showAutoMigrateWarning(row.node)" class="tag warn">需确认风险</span>
                    <span v-else class="muted">—</span>
                    <span v-if="row.node.filteredChildCount > 0" class="filtered-hint" :data-testid="`filtered-children-${encodeId(row.node.path)}`">
                      隐藏 {{ row.node.filteredChildCount }} 个子目录
                    </span>
                  </td>
                  <td>
                    <button
                      v-if="showMigrateAction(row.node)"
                      class="button button-quiet"
                      :data-testid="`migrate-${encodeId(row.node.path)}`"
                      @click="migrate(row.node)"
                    >
                      {{ row.node.autoMigrate ? '一键迁移' : '自定义迁移' }}
                      <ArrowRight :size="14" />
                    </button>
                    <button
                      v-else-if="row.node.scanStatus && managedStatuses.has(row.node.scanStatus)"
                      class="button button-secondary"
                      @click="viewLinks"
                    >
                      <Link2 :size="14" /> 查看链接
                    </button>
                    <span v-else class="muted">—</span>
                  </td>
                </tr>
                <tr v-else class="more-row" :data-testid="`more-${encodeId(row.parentPath)}`">
                  <td :colspan="5" :style="{ paddingLeft: `${row.depth * 18 + 16 + 28}px` }">
                    <button class="button button-quiet" @click="loadMore(row.parentPath)">
                      <ChevronDown :size="14" /> 显示更多
                    </button>
                  </td>
                </tr>
              </template>
            </tbody>
          </table>
        </div>
      </section>
    </template>
  </div>
</template>

<style scoped>
.spinning { animation: spin 1s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

.header-actions { display: flex; gap: 8px; }

.notice-info { display: inline-flex; align-items: center; gap: 7px; margin-bottom: 14px; color: #0c4a8b; background: #e7f2ff; border: 1px solid #c8dffc; }

.root-summary { margin-bottom: 20px; padding: 16px 18px; border: 1px solid var(--line); border-radius: 10px; background: var(--surface-soft); }
.root-summary-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 12px; }
.root-summary-title { display: inline-flex; align-items: center; gap: 7px; color: var(--text-primary); font-size: 13px; font-weight: 650; }
.root-summary-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 12px; margin: 0; }
.root-summary-grid > div { display: flex; flex-direction: column; gap: 4px; }
.root-summary-grid dt { color: var(--text-tertiary); font-size: 11px; }
.root-summary-grid dd { margin: 0; color: var(--text-primary); font-size: 13px; font-weight: 600; }

.recommended-panel { margin-bottom: 20px; padding: 16px 18px; border: 1px solid #c8e0ff; border-radius: 10px; background: #f5faff; }
.recommended-head { display: flex; align-items: center; gap: 8px; margin-bottom: 12px; color: var(--accent-dark); font-size: 13px; font-weight: 650; }
.recommended-list { display: flex; flex-direction: column; gap: 10px; margin: 0; padding: 0; list-style: none; }
.recommended-list li { display: grid; grid-template-columns: 1fr auto auto; align-items: center; gap: 14px; padding: 10px 12px; border: 1px solid var(--line); border-radius: 8px; background: white; }
.recommended-name { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; min-width: 0; }
.recommended-name strong { color: var(--text-primary); font-size: 13px; }
.path-inline { font-size: 11px; word-break: break-all; }
.recommended-meta { display: flex; align-items: center; gap: 10px; color: var(--text-tertiary); font-size: 12px; }

.tree-row .cell-name { display: flex; align-items: flex-start; gap: 6px; min-width: 0; }
.tree-row .indent-spacer { display: inline-block; flex-shrink: 0; }
.tree-row .caret {
  display: inline-grid; flex-shrink: 0;
  width: 22px; height: 22px; margin-top: 1px;
  place-items: center;
  background: transparent; border: 0; border-radius: 6px;
  color: var(--text-tertiary);
  transition: background .15s ease, color .15s ease;
}
.tree-row .caret:hover { background: var(--surface-muted); color: var(--text-primary); }
.tree-row .caret svg { transition: transform .15s ease; }
.tree-row .caret svg.expanded { transform: rotate(0deg); }
.tree-row .caret svg:not(.expanded) { transform: rotate(-90deg); }
.tree-row .caret-spacer { display: inline-block; flex-shrink: 0; width: 22px; height: 22px; }
.tree-row .name-text { display: flex; flex-direction: column; min-width: 0; gap: 2px; }
.tree-row .name-text strong { color: var(--text-primary); font-size: 13px; word-break: break-all; }
.tree-row .path {
  color: var(--text-tertiary);
  font-size: 11px;
  font-weight: 400;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 100%;
}

.filtered-hint {
  display: inline-block;
  margin-left: 8px;
  color: var(--text-tertiary);
  font-size: 11px;
}

.more-row td { padding: 6px 16px; background: #fafbfc; border-top: 1px dashed var(--line); }
.more-row button { font-size: 12px; }

.highlighted { animation: highlight-flash 1.6s ease-out; }
@keyframes highlight-flash {
  0% { background-color: #fff3a8; }
  100% { background-color: transparent; }
}

.tag.warn { color: #a96800; background: #fff3d8; }
.tag.err { color: #b42318; background: #ffe4e1; }
.tag.auto { color: #0c68d7; background: #e2efff; }
.tag.migrated { color: #18794e; background: #e8f8ef; }
.tag.migration_pending, .tag.contains_migrated, .tag.contains_link { color: #a96800; background: #fff3d8; }
.tag.link_broken { color: #b42318; background: #ffe4e1; }
.tag.existing_link { color: #0c68d7; background: #e2efff; }
</style>