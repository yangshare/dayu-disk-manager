import { describe, expect, it } from 'vitest'
import fixture from './__fixtures__/scan-contract.json'
import type {
  CurrentPhase, FastScanFailure, ScanDriveResult, ScanInvalidatedEvent,
  ScanMode, ScanProgressEvent, ScanSource,
} from './types'
import { formatSize } from './types'

describe('Rust IPC JSON 合约', () => {
  it('覆盖 ScanMode、ScanSource 与 CurrentPhase 的全部变体', () => {
    const modes: ScanMode[] = fixture.scanModes as ScanMode[]
    const sources: ScanSource[] = fixture.scanSources as ScanSource[]
    const phases: CurrentPhase[] = fixture.currentPhases as CurrentPhase[]

    expect(modes).toEqual(['auto', 'mft', 'filesystem'])
    expect(sources).toEqual(['mft', 'filesystem'])
    expect(phases).toEqual(['reading_mft', 'aggregating', 'annotating', 'walking_fs'])
  })

  it('保留内部标签 kind、Complete.snapshot 与快速失败 reason.kind', () => {
    const results = fixture.scanDriveResults as ScanDriveResult[]
    expect(results[0]).toEqual({ kind: 'needs_elevation' })
    expect(results[1]).toMatchObject({
      kind: 'fast_scan_unavailable',
      reason: { kind: 'invalid_volume_data' },
    })
    expect(results[2]).toMatchObject({
      kind: 'complete',
      snapshot: { scanId: 'fixture-scan', source: 'mft' },
    })
    expect(Object.prototype.hasOwnProperty.call(results[2], 'snapshot')).toBe(true)
    expect(Object.prototype.hasOwnProperty.call(results[1], 'reason')).toBe(true)
  })

  it('覆盖 FastScanFailure 的每个 kind（包括 I/O 的可空 code）', () => {
    const failures = fixture.fastScanFailures as FastScanFailure[]
    const kinds = failures.map((failure) => failure.kind)
    expect(kinds).toEqual([
      'unsupported_filesystem', 'unsupported_ntfs_version', 'mft_too_large', 'invalid_volume_data',
      'root_record_missing', 'excessive_record_errors', 'io', 'io',
    ])
    expect(failures[0]).toMatchObject({ actual: 'exfat' })
    expect(failures[1]).toMatchObject({ major: 1, minor: 2 })
    expect(failures[2]).toMatchObject({ bytes: 536870913 })
    expect(failures[5]).toMatchObject({ skipped: 1, scanned: 2 })
    expect(failures[6]).toMatchObject({ code: 5 })
    expect(failures[7]).toMatchObject({ code: null })
  })

  it('解码扫描进度和失效事件的 camelCase 字段', () => {
    const progress = fixture.scanProgress as ScanProgressEvent
    const events = fixture.invalidatedEvents as ScanInvalidatedEvent[]
    expect(progress.currentPhase).toBe('aggregating')
    expect(progress.estimatedRecordSlots).toBe(4)
    expect(events).toEqual([
      { reason: 'migrated', autoRescan: true },
      { reason: 'restored', autoRescan: false },
    ])
  })
})

describe('formatSize', () => {
  it('formats bytes/KB/MB/GB', () => {
    expect(formatSize(500)).toBe('500 B')
    expect(formatSize(2048)).toBe('2.0 KB')
    expect(formatSize(5 * 1024 * 1024)).toBe('5.0 MB')
    expect(formatSize(3 * 1024 ** 3)).toBe('3.0 GB')
  })
})
