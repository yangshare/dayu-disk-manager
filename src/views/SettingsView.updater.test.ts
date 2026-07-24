// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import SettingsView from './SettingsView.vue'

const mocks = vi.hoisted(() => ({
  checkForUpdates: vi.fn(),
  getConfig: vi.fn(),
  saveConfig: vi.fn(),
  exportHistory: vi.fn(),
}))

vi.mock('../composables/useUpdater', () => ({
  checkForUpdates: mocks.checkForUpdates,
}))
vi.mock('../ipc/invoke', () => ({
  ipc: {
    getConfig: mocks.getConfig,
    saveConfig: mocks.saveConfig,
    exportHistory: mocks.exportHistory,
  },
}))

describe('SettingsView 检查更新', () => {
  it('点击按钮触发手动检查', async () => {
    mocks.getConfig.mockResolvedValue({ repository: '', scan: { minSizeMb: 0, excludePaths: [] } })
    mocks.checkForUpdates.mockResolvedValue(undefined)
    const wrapper = mount(SettingsView)
    await flushPromises()
    await wrapper.get('[data-test="check-update"]').trigger('click')
    await flushPromises()
    expect(mocks.checkForUpdates).toHaveBeenCalledWith(false)
  })
})
