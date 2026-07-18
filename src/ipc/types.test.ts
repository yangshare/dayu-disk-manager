import { describe, it, expect } from 'vitest'
import { formatSize } from './types'

describe('formatSize', () => {
  it('formats bytes/KB/MB/GB', () => {
    expect(formatSize(500)).toBe('500 B')
    expect(formatSize(2048)).toBe('2.0 KB')
    expect(formatSize(5 * 1024 * 1024)).toBe('5.0 MB')
    expect(formatSize(3 * 1024 ** 3)).toBe('3.0 GB')
  })
})
