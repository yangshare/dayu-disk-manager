// @vitest-environment jsdom
import { describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import ProgressStage from './ProgressStage.vue'

describe('ProgressStage', () => {
  it('shows stage, transfer totals, and current path', () => {
    const wrapper = mount(ProgressStage, {
      props: {
        progress: {
          taskId: 'task-1',
          stage: 'copying',
          percent: 25,
          message: '正在复制到迁移仓库',
          transfer: {
            phase: 'copying',
            completedBytes: 2 * 1024 ** 3,
            totalBytes: 8 * 1024 ** 3,
            completedFiles: 1200,
            totalFiles: 4800,
            currentPath: '_cacache\\content-v2\\sha512\\ab',
          },
        },
      },
    })

    expect(wrapper.text()).toContain('正在复制到迁移仓库')
    expect(wrapper.text()).toContain('25%')
    expect(wrapper.text()).toContain('2.0 GB / 8.0 GB')
    expect(wrapper.text()).toContain('1,200 / 4,800')
    expect(wrapper.text()).toContain('_cacache\\content-v2\\sha512\\ab')
    expect(wrapper.find('[role="progressbar"]').attributes('aria-valuenow')).toBe('25')
    expect(wrapper.find('.fill').attributes('style')).toContain('width: 25%')
  })

  it('shows preparation work without pretending the total is known', () => {
    const wrapper = mount(ProgressStage, {
      props: {
        progress: {
          taskId: 'task-1',
          stage: 'copying',
          percent: 0,
          message: '正在统计待复制内容',
          transfer: {
            phase: 'preparing',
            completedBytes: 1024,
            completedFiles: 42,
          },
        },
      },
    })

    expect(wrapper.text()).toContain('准备阶段')
    expect(wrapper.text()).toContain('已发现文件')
    expect(wrapper.text()).toContain('42')
  })

  it('uses restore-specific labels and status copy', () => {
    const wrapper = mount(ProgressStage, {
      props: {
        operation: 'restore',
        progress: {
          taskId: 'restore-1',
          stage: 'removing_junction',
          percent: 70,
          message: '删除 junction',
        },
      },
    })

    expect(wrapper.text()).toContain('还原任务正在后台执行')
    expect(wrapper.text()).toContain('移除链接')
    expect(wrapper.text()).toContain('恢复普通目录')
  })
})
