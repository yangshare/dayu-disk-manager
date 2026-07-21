import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type { ProgressEvent, ScanInvalidatedEvent, ScanProgressEvent } from './types'

export async function onProgress(cb: (event: ProgressEvent) => void): Promise<UnlistenFn> {
  return listen<ProgressEvent>('dayu://progress', (event) => cb(event.payload))
}

export async function onScanProgress(cb: (event: ScanProgressEvent) => void): Promise<UnlistenFn> {
  return listen<ScanProgressEvent>('dayu://scan-progress', (event) => cb(event.payload))
}

export async function onScanInvalidated(
  cb: (event: ScanInvalidatedEvent) => void,
): Promise<UnlistenFn> {
  return listen<ScanInvalidatedEvent>('dayu://scan-invalidated', (event) => cb(event.payload))
}
