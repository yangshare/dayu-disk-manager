import { createRouter, createWebHashHistory, type RouteRecordRaw } from 'vue-router'

const routes: RouteRecordRaw[] = [
  { path: '/', redirect: '/scan' },
  { path: '/scan', name: 'scan', component: () => import('../views/ScanView.vue') },
  { path: '/migrate', name: 'migrate', component: () => import('../views/MigrateView.vue') },
  { path: '/links', name: 'links', component: () => import('../views/LinksView.vue') },
  { path: '/history', name: 'history', component: () => import('../views/HistoryView.vue') },
  { path: '/settings', name: 'settings', component: () => import('../views/SettingsView.vue') },
]

export const router = createRouter({ history: createWebHashHistory(), routes })
