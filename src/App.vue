<script setup lang="ts">
import { RouterView, RouterLink } from 'vue-router'
import { isTauri } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import {
  BarChart3, FolderKanban, Link2, History, Settings,
  Minus, Square, X, ShieldCheck, Sparkles,
} from '@lucide/vue'

const appWindow = isTauri() ? getCurrentWindow() : null

function isInteractiveTarget(target: EventTarget | null) {
  return target instanceof Element
    && !!target.closest('button,a,input,select,textarea,[data-no-drag]')
}

async function startDrag(event: MouseEvent) {
  if (event.button !== 0 || event.detail > 1 || isInteractiveTarget(event.target)) return
  try { await appWindow?.startDragging() } catch { /* browser preview */ }
}

async function minimize() {
  try { await appWindow?.minimize() } catch { /* browser preview */ }
}

async function toggleMaximize() {
  try { await appWindow?.toggleMaximize() } catch { /* browser preview */ }
}

async function closeWindow() {
  try { await appWindow?.close() } catch { /* browser preview */ }
}
</script>

<template>
  <div class="app-window">
    <header class="titlebar" @mousedown="startDrag" @dblclick="toggleMaximize">
      <div class="titlebar-title">大禹磁盘管理器</div>
      <div class="titlebar-spacer" />
      <div class="traffic-lights" data-no-drag @dblclick.stop>
        <button class="traffic-button traffic-close" aria-label="关闭" title="关闭" @mousedown.stop @click.stop="closeWindow"><X :size="11" /></button>
        <button class="traffic-button traffic-minimize" aria-label="最小化" title="最小化" @mousedown.stop @click.stop="minimize"><Minus :size="11" /></button>
        <button class="traffic-button traffic-maximize" aria-label="最大化" title="最大化" @mousedown.stop @click.stop="toggleMaximize"><Square :size="9" /></button>
      </div>
    </header>

    <div class="app-body">
      <aside class="sidebar">
        <div class="brand">
          <div class="brand-mark"><Sparkles :size="18" /></div>
          <div>
            <strong>大禹磁盘管理器</strong>
            <span>空间管理工具</span>
          </div>
        </div>

        <nav class="nav-list" aria-label="主导航">
          <p class="nav-label">工作区</p>
          <RouterLink to="/scan"><BarChart3 :size="17" /> <span>扫描分析</span></RouterLink>
          <RouterLink to="/migrate"><FolderKanban :size="17" /> <span>迁移</span></RouterLink>
          <RouterLink to="/links"><Link2 :size="17" /> <span>软链接管理</span></RouterLink>
          <p class="nav-label nav-label-spaced">记录</p>
          <RouterLink to="/history"><History :size="17" /> <span>操作历史</span></RouterLink>
          <RouterLink to="/settings"><Settings :size="17" /> <span>设置</span></RouterLink>
        </nav>

        <div class="sidebar-footer">
          <ShieldCheck :size="15" />
          <span>本地运行 · 数据不上传</span>
        </div>
      </aside>
      <main class="content"><RouterView /></main>
    </div>
  </div>
</template>
