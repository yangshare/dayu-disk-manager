// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import { createPinia, setActivePinia } from 'pinia'
import ScanView from './ScanView.vue'

const mocks = vi.hoisted(() => ({
  scanDrives: vi.fn().mockResolvedValue([]),
  push: vi.fn(),
}))

vi.mock('../ipc/invoke', () => ({
  ipc: { scanDrives: mocks.scanDrives },
}))

vi.mock('vue-router', () => ({
  useRouter: () => ({ push: mocks.push }),
}))

describe('ScanView', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    mocks.scanDrives.mockClear()
  })

  it('waits for the user before starting a scan', async () => {
    const wrapper = mount(ScanView)
    await flushPromises()

    expect(mocks.scanDrives).not.toHaveBeenCalled()
    expect(wrapper.text()).toContain('准备扫描你的磁盘')

    await wrapper.get('[data-testid="start-scan"]').trigger('click')
    await flushPromises()
    expect(mocks.scanDrives).toHaveBeenCalledTimes(1)
  })
})
