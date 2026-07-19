// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import { createPinia, setActivePinia } from 'pinia'
import ScanView from './ScanView.vue'

const mocks = vi.hoisted(() => ({
  scanDrives: vi.fn().mockResolvedValue([]),
  cancelScan: vi.fn().mockResolvedValue(true),
  onScanProgress: vi.fn().mockResolvedValue(vi.fn()),
  push: vi.fn(),
}))

vi.mock('../ipc/invoke', () => ({
  ipc: { scanDrives: mocks.scanDrives, cancelScan: mocks.cancelScan },
}))

vi.mock('../ipc/events', () => ({
  onScanProgress: mocks.onScanProgress,
}))

vi.mock('vue-router', () => ({
  useRouter: () => ({ push: mocks.push }),
}))

describe('ScanView', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    mocks.scanDrives.mockReset().mockResolvedValue([])
    mocks.cancelScan.mockClear()
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

  it('renders large result sets in bounded batches', async () => {
    mocks.scanDrives.mockResolvedValue(Array.from({ length: 201 }, (_, index) => ({
      path: `C:\\data\\${index}`,
      displayName: `dir-${index}`,
      sizeBytes: 201 - index,
      matchedPreset: null,
      category: null,
      autoMigrate: false,
      isJunction: false,
      inaccessible: false,
    })))
    const wrapper = mount(ScanView)

    await wrapper.get('[data-testid="start-scan"]').trigger('click')
    await flushPromises()

    expect(wrapper.findAll('tbody tr')).toHaveLength(200)
    await wrapper.get('.results-more button').trigger('click')
    expect(wrapper.findAll('tbody tr')).toHaveLength(201)
  })

  it('can cancel a running scan', async () => {
    let finishScan: (items: never[]) => void = () => {}
    mocks.scanDrives.mockReturnValue(new Promise<never[]>((resolve) => { finishScan = resolve }))
    const wrapper = mount(ScanView)

    await wrapper.get('[data-testid="start-scan"]').trigger('click')
    await flushPromises()
    const stopButton = wrapper.findAll('button').find((button) => button.text().includes('停止扫描'))
    expect(stopButton).toBeDefined()
    await stopButton!.trigger('click')
    expect(mocks.cancelScan).toHaveBeenCalledTimes(1)

    finishScan([])
    await flushPromises()
  })
})
