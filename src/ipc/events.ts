import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type { ProgressEvent, ScanProgressEvent } from './types'

export async function onProgress(cb: (e: ProgressEvent) => void): Promise<UnlistenFn> {
  return listen<ProgressEvent>('dayu://progress', (ev) => cb(ev.payload))
}

export async function onScanProgress(cb: (e: ScanProgressEvent) => void): Promise<UnlistenFn> {
  return listen<ScanProgressEvent>('dayu://scan-progress', (ev) => cb(ev.payload))
}
