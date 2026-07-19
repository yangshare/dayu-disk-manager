export interface ScanItem {
  path: string
  displayName: string
  sizeBytes: number
  matchedPreset: string | null
  category: PresetCategory | null
  autoMigrate: boolean
  isJunction: boolean
  inaccessible: boolean
  scanStatus: ScanItemStatus | null
  migrationId: string | null
}

export type ScanItemStatus =
  | 'migrated' | 'migration_pending' | 'link_broken'
  | 'existing_link' | 'contains_migrated' | 'contains_link'

export type PresetCategory =
  | 'communication' | 'game_library' | 'dev_cache'
  | 'ide' | 'container' | 'app_install' | 'custom'

export interface Migration {
  id: string
  schemaVersion: number
  source: string
  target: string
  oldPath: string
  preset: string | null
  createdAt: string
  status: 'active' | 'old_pending_delete' | 'target_pending_delete' | 'pending_manual_confirm'
  sourceVolumeSerial: string
  targetVolumeSerial: string
  recycleBinRef: string
  pendingCleanup: string | null
}

export interface LinkItem {
  id: string
  source: string
  target: string
  preset: string | null
  createdAt: string
  status: string
  valid: boolean
  broken: boolean
}

export interface HistoryEntry {
  op: string
  id: string
  src: string
  dst: string
  result: string
  time: string
  durationSec: number
}

export interface Config {
  schemaVersion: number
  repository: string
  scan: { minSizeMb: number; excludePaths: string[] }
  presets: Preset[]
}

export interface Preset {
  id: string
  name: string
  category: PresetCategory
  matchPaths: string[]
  matchProcesses: string[]
  autoMigrate: boolean
  targetSubdir: string
}

export interface PrecheckReport {
  ok: boolean
  warnings: string[]
  blockers: string[]
  sourceSizeBytes: number
  targetFreeBytes: number
}

export interface ProgressEvent {
  taskId: string
  stage: string
  percent: number
  message: string
  transfer?: {
    phase: 'preparing' | 'copying'
    completedBytes: number
    totalBytes?: number
    completedFiles: number
    totalFiles?: number
    currentPath?: string
  }
}

export interface ScanProgressEvent {
  scannedDirs: number
  scannedFiles: number
  currentPath: string
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let v = bytes / 1024
  let i = 0
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++ }
  return `${v.toFixed(1)} ${units[i]}`
}
