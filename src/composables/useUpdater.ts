import { check } from '@tauri-apps/plugin-updater'
import { ask, message } from '@tauri-apps/plugin-dialog'
import { relaunch } from '@tauri-apps/plugin-process'

/**
 * 检查并引导安装更新。
 * @param silent true=启动静默检查（无更新/失败都不打扰）；false=设置页手动触发（任何状态都给反馈）
 */
export async function checkForUpdates(silent: boolean): Promise<void> {
  let update
  try {
    update = await check()
  } catch (e) {
    if (silent) return
    message(`检查更新失败：${String(e).replace(/^Error:\s*/, '')}`)
    return
  }

  if (!update?.available) {
    if (!silent) message('当前已是最新版本。')
    return
  }

  const agreed = await ask(
    `发现新版本 ${update.version}，是否立即下载并安装？`,
    { title: '发现新版本', okLabel: '立即更新', cancelLabel: '稍后' },
  )
  if (!agreed) return

  await update.downloadAndInstall()
  await relaunch()
}
