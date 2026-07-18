<script setup lang="ts">
import { RouterView, RouterLink } from 'vue-router'
</script>

<template>
  <div class="layout">
    <nav class="sidebar">
      <h1>大禹磁盘管理器</h1>
      <RouterLink to="/scan">扫描分析</RouterLink>
      <RouterLink to="/migrate">迁移</RouterLink>
      <RouterLink to="/links">软链接管理</RouterLink>
      <RouterLink to="/history">操作历史</RouterLink>
      <RouterLink to="/settings">设置</RouterLink>
    </nav>
    <main class="content"><RouterView /></main>
  </div>
</template>

<style>
.layout { display: flex; height: 100vh; }
.sidebar { width: 200px; padding: 16px; background: #f4f4f5; display: flex; flex-direction: column; gap: 8px; }
.sidebar a { text-decoration: none; color: #333; padding: 8px; border-radius: 4px; }
.sidebar a.router-link-active { background: #3b82f6; color: #fff; }
.content { flex: 1; padding: 24px; overflow: auto; }
</style>
