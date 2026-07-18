import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type { ProgressEvent } from './types'

export async function onProgress(cb: (e: ProgressEvent) => void): Promise<UnlistenFn> {
  return listen<ProgressEvent>('dayu://progress', (ev) => cb(ev.payload))
}
