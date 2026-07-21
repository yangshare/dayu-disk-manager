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

export interface TransferProgress {
  phase: 'preparing' | 'copying'
  completedBytes: number
  totalBytes?: number
  completedFiles: number
  totalFiles?: number
  currentPath?: string
}

export interface ProgressEvent {
  taskId: string
  stage: string
  percent: number
  message: string
  transfer?: TransferProgress
}

export type ScanSource = 'mft' | 'filesystem'
export type ScanMode = 'auto' | 'mft' | 'filesystem'

export type FastScanFailure =
  | { kind: 'unsupported_filesystem'; actual: string }
  | { kind: 'unsupported_ntfs_version'; major: number; minor: number }
  | { kind: 'invalid_volume_data' }
  | { kind: 'root_record_missing' }
  | { kind: 'excessive_record_errors'; skipped: number; scanned: number }
  | { kind: 'io'; code: number | null }

export type ScanDriveResult =
  | { kind: 'needs_elevation' }
  | { kind: 'fast_scan_unavailable'; reason: FastScanFailure }
  | { kind: 'complete'; snapshot: ScanSnapshot }

export interface ScanSnapshot {
  scanId: string
  source: ScanSource
  roots: TreeNode[]
  filteredRootCount: number
  rootFileSummary: RootFileSummary
  diagnostics: ScanDiagnostics
}

export interface ScanDiagnostics {
  scannedRecords: number
  scannedDirs: number
  scannedFiles: number
  skippedRecords: number
  orphanEntries: number
  hardLinkEntries: number
}

export interface RootFileSummary {
  directFileSizeBytes: number
  directFileCount: number
  systemMetadataSizeBytes: number | null
  totalKnownSizeBytes: number
  incomplete: boolean
}

export type ScanItemStatusTree = ScanItemStatus

export type AccessState = 'unknown' | 'accessible' | 'inaccessible'

export interface TreeNode {
  path: string
  displayName: string
  sizeBytes: number
  linkedTargetSizeBytes: number | null
  fileCount: number
  dirCount: number
  depth: number
  isReparse: boolean
  reparseTag: number | null
  isJunction: boolean
  accessState: AccessState
  matchedPreset: string | null
  category: PresetCategory | null
  autoMigrate: boolean
  scanStatus: ScanItemStatus | null
  migrationId: string | null
  childCount: number
  filteredChildCount: number
}

export interface ChildPage {
  items: TreeNode[]
  total: number
  nextOffset: number | null
}

export interface RevealLevel {
  parentPath: string
  page: ChildPage
}

export type CurrentPhase = 'reading_mft' | 'aggregating' | 'annotating' | 'walking_fs'

export interface ScanProgressEvent {
  scannedRecords: number
  scannedDirs: number
  scannedFiles: number
  estimatedRecordSlots: number
  currentPhase: CurrentPhase
}

export interface OperationOutcome {
  sourceChanged: boolean
  reason: string
}

export interface ScanInvalidatedEvent {
  reason: string
  autoRescan: boolean
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let v = bytes / 1024
  let i = 0
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++ }
  return `${v.toFixed(1)} ${units[i]}`
}

export function isStaleScanError(error: unknown): boolean {
  if (typeof error === 'string') return error === 'stale_scan'
  if (error && typeof error === 'object') {
    const value = error as { code?: unknown; message?: unknown }
    return value.code === 'stale_scan' || value.message === 'stale_scan'
  }
  return false
}
