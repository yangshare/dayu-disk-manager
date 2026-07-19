import { invoke } from '@tauri-apps/api/core'
import type {
  ScanItem, Migration, LinkItem, HistoryEntry, Config, PrecheckReport,
} from './types'

export const ipc = {
  scanDrives: () => invoke<ScanItem[]>('scan_drives'),
  cancelScan: () => invoke<boolean>('cancel_scan'),
  precheckMigrate: (src: string) => invoke<PrecheckReport>('precheck_migrate', { src }),
  startMigrate: (migrationId: string, src: string, presetId: string | null) =>
    invoke<Migration>('start_migrate', { migrationId, src, presetId }),
  cancelMigrate: () => invoke<boolean>('cancel_migrate'),
  startRestore: (migrationId: string) => invoke<boolean>('start_restore', { migrationId }),
  listLinks: () => invoke<LinkItem[]>('list_links'),
  breakLink: (migrationId: string) => invoke<boolean>('break_link_cmd', { migrationId }),
  listHistory: (op?: string, from?: string, to?: string) =>
    invoke<HistoryEntry[]>('list_history', { op, from, to }),
  getConfig: () => invoke<Config>('get_config'),
  saveConfig: (config: Config) => invoke<void>('save_config', { config }),
  exportHistory: () => invoke<string>('export_history'),
  getRecoveryAdvice: () => invoke<[string, string, string][]>('get_recovery_advice'),
}
