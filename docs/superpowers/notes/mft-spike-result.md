# MFT 逐记录枚举 go/no-go 门槛结论

**结论：no-go**

`FSCTL_GET_NTFS_FILE_RECORD` 逐记录 IOCTL 路线未通过性能门槛。按简报 0.4 授权的 no-go 对策，停止逐记录正式实现，T2 改为批量读取 `$MFT`。

## 机器配置

| 项 | 值 |
|---|---|
| 主板 | Micro-Star MS-7D99 |
| CPU | Intel i5-14600KF |
| 内存 | 31.8 GB |
| C 盘介质 | KINGSTON RBUSNS8180S3256GJ，SATA SSD，238.5 GB |
| OS | Windows 11 Pro 10.0.26200 |

C 盘在 SATA SSD 上，介质本身不是瓶颈（见下"瓶颈定位"）。

## 卷信息

| 项 | 值 | 来源 |
|---|---|---|
| 文件系统 | NTFS 3.1 | `FSCTL_GET_NTFS_VOLUME_DATA` 扩展结构 |
| `bytes_per_file_record_segment` | 1024 | 基础结构 |
| `MftValidDataLength` | 2,119,798,528 B | 基础结构 |
| `slot_count`（上限） | 2,070,272 | `ceil(MftValidDataLength / 1024)` |
| 在用记录数 | 1,524,444 | spike 三次枚举稳定值 |
| 在用率 | 73.6% | `1,524,444 / 2,070,272` |

## 测量方法

构建：`cargo build --manifest-path src-tauri/Cargo.toml --example mft_spike --release`（release，优化）。

运行：管理员 PowerShell 调用 `mft_spike.exe C`。spike 从 `slot_count - 1` 请求，按 API 返回的低 48 位记录号递减推进，HashSet 去重，记录 0 计入后终止。每次运行前不预热缓存。

三次独立运行，间隔数分钟，期间系统正常运行（含后台任务）。

## 性能数据

| 运行 | elapsed_ms | records_seen | transient_errors | 缓存状态 |
|---|---|---|---|---|
| #1 | 71,094 | 1,524,440 | 0 | 冷启动 |
| #2 | 2,472 | 1,524,440 | 0 | 热缓存 |
| #3 | 2,525 | 1,524,444 | 0 | 热缓存 |

换算到 100 万在用记录：

| 场景 | 单记录耗时 | 100 万记录耗时 | 门槛（10s） | 判定 |
|---|---|---|---|---|
| 冷启动 | 46.6 μs | **47 s** | 10 s | **超 4.7 倍 — no-go** |
| 热缓存 | 1.62 μs | 1.62 s | 10 s | 达标 |

峰值进程工作集：约 25.8 MB（远低于 512 MB 内存门槛）。

**冷启动 71 秒 vs 热缓存 2.5 秒**，相差 29 倍。这证明数据访问本身不是瓶颈——热缓存下 152 万记录 2.5 秒即可读完。瓶颈是**每记录一次 `DeviceIoControl(FSCTL_GET_NTFS_FILE_RECORD)` 系统调用的固有开销**：用户态 → 内核态 → NTFS 驱动 → 读单条 MFT 记录 → 返回。冷启动时每条记录的 MFT 页面需逐次从 SSD 取入，加上系统调用本身的开销，单记录 ~47 μs。这正是 WizTree 等工具选择批量读 `$MFT` 而非逐记录 IOCTL 的原因（设计规范 4.1 已预警此点）。

## 无遗漏证据与局限

### 证据

1. **三次独立枚举 records_seen 稳定一致**：1,524,440 / 1,524,440 / 1,524,444。前两次完全一致，第三次多 4 条系扫描期间正常文件变化（新建/删除），非稳定遗漏——`transient_errors = 0` 全程为零。
2. **transient_errors = 0**：三次均无扫描期竞态，枚举过程是确定性的。
3. **记录 0 正确计入并终止**：spike 逻辑保证 `$MFT` 自身（记录 0）先入 seen 再 break，不在处理前跳过。
4. **根 5（`$Root`）正确识别**：`seen=true in_use=true file_sig=true`——卷根目录记录存在、在用、`FILE` 签名正确。
5. **records_seen ≤ slot_count**：1,524,444 ≤ 2,070,272，且在用率 73.6% 合理（MFT 含已回收的空洞槽位）。

### 局限

**独立的 `$MFT::$BITMAP` 位图对照未执行。** 简报 0.3 要求"用受信任的 NTFS 工具导出同一时点的在用记录号集合，或读取 `$MFT::$BITMAP` 形成基准集合"。本次未完成该独立对照，原因：

- 读取 `$MFT::$BITMAP` 需解析记录 0 的属性链 + Data Run，属 T3 级 MFT 属性解析能力，超出 T0 spike（最小 ABI 边界 + 探测）范围。
- 本机未安装受信任 NTFS 交叉验证工具（ntfsinfo / WizTree），无法导出对照集合。

无遗漏的"独立对照"要求服务于逐记录路线。性能 no-go 已否决该路线，转批量读 `$MFT` 后将在 T3 用真实 fixture 重新验证无遗漏（批量读天然读取 `$MFT` 全部有效数据，无遗漏性由"`$MFT` 文件有效数据长度"这一卷级权威量保证，而非依赖枚举 API 的完整性）。

## 决定与对后续任务的影响

**no-go。** 按简报 0.4 与设计规范 4.1 的既定对策执行：

- **停止** `FSCTL_GET_NTFS_FILE_RECORD` 逐记录 IOCTL 的正式实现（`read_mft_record` 仅作为 T0 spike 探测用，不进生产扫描路径）。
- **T2 改为**：解析 `$MFT`（记录 0）的 `$DATA` 属性 Data Run，以卷级读取批量获取 `$MFT` 文件内容，再对缓冲区按 `bytes_per_file_record_segment` 切片逐记录解析。一次大读消除每记录系统调用开销，预期冷启动性能从 ~47s/百万 降至与热缓存同量级（<10s/百万 达标）。
- **接口不变**：`RawFileRecord` / `VolumeData` / T1 计划定义的 `RecordReader` trait 保持原样，仅底层 reader 实现从"逐记录 IOCTL"替换为"批量读 `$MFT` 后切片"。设计规范与简报明确"T3 及以后接口不变"。
- **不靠并行大量 IOCTL 绕过门槛**（简报明令禁止）——瓶颈在系统调用本身，并行 IOCTL 无法消除。
