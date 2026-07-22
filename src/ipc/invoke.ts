import { invoke } from '@tauri-apps/api/core'
import type {
  ScanMode, ScanDriveResult, ChildPage, RevealLevel, TreeNode,
  Migration, LinkItem, HistoryEntry, Config, PrecheckReport,
} from './types'

export const ipc = {
  scanDrive: (mode: ScanMode) => invoke<ScanDriveResult>('scan_drive', { mode }),
  expandNode: (scanId: string, path: string, offset: number, limit: number) =>
    invoke<ChildPage>('expand_node', { scanId, path, offset, limit }),
  revealNode: (scanId: string, path: string, limit: number) =>
    invoke<RevealLevel[]>('reveal_node', { scanId, path, limit }),
  listRecommended: (scanId: string) => invoke<TreeNode[]>('list_recommended', { scanId }),
  restartElevated: () => invoke<boolean>('restart_elevated'),
  takeStartupScanIntent: () => invoke<boolean>('take_startup_scan_intent'),
  cancelScan: () => invoke<boolean>('cancel_scan'),
  precheckMigrate: (src: string) => invoke<PrecheckReport>('precheck_migrate', { src }),
  startMigrate: (migrationId: string, src: string, presetId: string | null, enableVss: boolean) =>
    invoke<Migration>('start_migrate', { migrationId, src, presetId, enableVss }),
  cancelMigrate: () => invoke<boolean>('cancel_migrate'),
  startRestore: (migrationId: string, enableVss: boolean) =>
    invoke<boolean>('start_restore', { migrationId, enableVss }),
  listLinks: () => invoke<LinkItem[]>('list_links'),
  breakLink: (migrationId: string) => invoke<boolean>('break_link_cmd', { migrationId }),
  listHistory: (op?: string, from?: string, to?: string) =>
    invoke<HistoryEntry[]>('list_history', { op, from, to }),
  getConfig: () => invoke<Config>('get_config'),
  saveConfig: (config: Config) => invoke<void>('save_config', { config }),
  exportHistory: () => invoke<string>('export_history'),
  getRecoveryAdvice: () => invoke<[string, string, string][]>('get_recovery_advice'),
}
