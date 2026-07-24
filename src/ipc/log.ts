import { trace, debug, info, warn, error } from '@tauri-apps/plugin-log'

// 前端统一日志出口。后端已接入 tauri-plugin-log，两者写入同一份文件
// （%LOCALAPPDATA%\dayu\logs\），排查问题时可按时间线对齐前后端。
//
// 用 logger.* 而非 console.*：console 输出在打包后用户机器上无法收集，
// 而走插件的日志会落盘，用户把日志文件发回来即可定位。

// 插件底层走 IPC，调用是异步的；前端日志通常 fire-and-forget，
// 吞掉 reject 避免产生未捕获的 Promise 拒绝。
function drop(p: Promise<void>): void {
  p.catch(() => {})
}

// 非 Tauri 运行时（vitest、纯浏览器预览）下插件不可用，回退到 console。
const hasPlugin =
  typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

function emit(
  level: (m: string) => Promise<void>,
  fallback: (m: string) => void,
  msg: string,
): void {
  if (hasPlugin) {
    drop(level(msg))
  } else {
    fallback(msg)
  }
}

export const logger = {
  trace: (msg: string) => emit(trace, (m) => console.debug(m), msg),
  debug: (msg: string) => emit(debug, (m) => console.debug(m), msg),
  info: (msg: string) => emit(info, (m) => console.info(m), msg),
  warn: (msg: string) => emit(warn, (m) => console.warn(m), msg),
  error: (msg: string) => emit(error, (m) => console.error(m), msg),
}
