# WizTree 式按需展开扫描实现计划

> 日期：2026-07-20
> 对应设计：`docs/superpowers/specs/2026-07-20-mft-tree-scan-design.md`
> 状态：待执行；T0 go/no-go 通过前不得进入正式 MFT 引擎实现

## 1. 目标与验收边界

把扫描从“全盘递归 + 扁平阈值过滤”改造成“MFT 元数据快速建树 + 树形按需展开”。最终行为必须满足：

- NTFS 快速扫描先经过真实性能和正确性门槛；不达标就切换到批量读取 `$MFT`，不能继续扩展逐记录 IOCTL 路线。
- 默认只返回 `C:\` 未排除的可见一级目录，子目录由前端按层分页拉取。
- `minSizeMB` 只控制可见性，不能剪掉扫描或祖先聚合。
- 预设、迁移/链接状态、无法访问节点及其导航祖先强制可见；排除路径优先级最高。
- 文件只参与目录统计，不进入目录树；`C:\` 直接文件与 NTFS 系统元文件单独只读汇总。
- 后端只发布完成构建的不可变快照；所有读树 IPC 都带 `scan_id`。
- 迁移、还原、断链等真正改变源路径的成功操作使整树失效；MFT 快照自动重扫，filesystem 快照提示手动重扫。
- 非权限类快速扫描失败也返回结构化原因，由用户决定是否执行普通扫描，不能静默降级。

非目标保持设计第 2 节不变：不做 USN 增量、多卷扫描、单文件链接、磁盘缓存以及压缩/稀疏文件物理占用精确统计。

## 2. 强制工程规则

- 每个任务先写能证明行为的失败测试，再实现，再运行该任务列出的完整验证命令。
- 文档中的二进制布局必须以仓库锁定的 `windows 0.62` 生成绑定和 Microsoft ABI 为准，不手抄不完整的 Win32 结构体。
- 真实 NTFS fixture、USA fixup、`$ATTRIBUTE_LIST`、解析健壮性测试是 T3 完成条件，不能 `#[ignore]` 后宣告完成。
- filesystem 有界并发是首版要求，不是可选性能增强。
- 不保留“后续补上”“先用占位值”“首版可接受不符合规格”一类跨任务技术债。
- 不在计划提交命令中使用 `git add -A`；只暂存当前任务明确列出的文件。
- 每个任务结束单独 commit。若工作区已有用户改动，执行者只暂存本任务自己的变更。

## 3. 文件与模块边界

| 文件 | 职责 |
|------|------|
| `src-tauri/src/win32.rs` | 唯一 Win32 边界：文件系统识别、卷句柄、NTFS IOCTL、UAC |
| `src-tauri/src/mft.rs` | MFT 枚举、USA 修复、属性解析、扩展记录合并、紧凑 `MftIndex` |
| `src-tauri/src/scanner.rs` | `DirectoryGraph`、聚合、排除、标注、可见性、filesystem 降级 |
| `src-tauri/src/models.rs` | Rust IPC 模型 |
| `src-tauri/src/app_state.rs` | 不可变 `TreeStore` 与当前快照槽位 |
| `src-tauri/src/commands.rs` | 扫描、分页、定位、提权、失效 IPC 编排 |
| `src-tauri/src/lib.rs` | 状态初始化、启动意图、命令注册、安全配置接入 |
| `src/ipc/*` | TypeScript IPC 类型、invoke、事件 |
| `src/stores/scan.ts` | 快照、展开分页、失效与重扫状态机 |
| `src/views/ScanView.vue` | 缩进树表、推荐定位、根文件汇总 |

MFT 解析保留在 `mft.rs`，不再创建只为跨任务占位的 `mft_parse.rs`。

## 4. 阶段与依赖

```text
T0 最小 Win32 读取边界 + MFT spike（go/no-go）
  |
  +-- go --> T1 完整平台边界 --> T2 枚举 --> T3 解析与 fixture
  |
  +-- no-go -> 记录结论并把 T2 改成批量读取 $MFT；T3 及以后接口不变

T3 --> T4 DirectoryGraph --> T5 标注/可见性 --> T6 TreeStore
                                              |
                                              v
T7 IPC --> T8 提权闭环 --> T9 filesystem 降级 --> T10 失效
                                                       |
                                                       v
T11 前端 store/IPC --> T12 视图 --> T13 e2e 与发布门槛
```

T0 不通过时必须先修订本计划中的 T2 读取部分和 spike 结论文档，再继续执行。

---

## 阶段 0：真实性门槛

### 任务 0：最小只读 ABI 边界与 MFT spike

**文件：**

- 修改：`src-tauri/Cargo.toml`
- 修改：`src-tauri/src/win32.rs`
- 创建：`src-tauri/examples/mft_spike.rs`
- 创建：`docs/superpowers/notes/mft-spike-result.md`

#### 0.1 先锁定 ABI

- [ ] `windows` feature 至少增加 `Win32_System_IO`、`Win32_System_Ioctl`；spike 若用 API 读取峰值工作集，再增加对应的 Process Status feature。
- [ ] 直接使用 `windows::Win32::System::Ioctl` 中的：
  - `NTFS_VOLUME_DATA_BUFFER`
  - `NTFS_EXTENDED_VOLUME_DATA`
  - `NTFS_FILE_RECORD_INPUT_BUFFER`
  - `NTFS_FILE_RECORD_OUTPUT_BUFFER`
  - `FSCTL_GET_NTFS_VOLUME_DATA`
  - `FSCTL_GET_NTFS_FILE_RECORD`
- [ ] 不定义删减字段的同名 `repr(C)` 结构。特别锁定以下事实：
  - `NTFS_EXTENDED_VOLUME_DATA` 以 `ByteCount` 开头，版本号不在 offset 0。
  - file-record 输出在 `FileReferenceNumber` 后还有 `FileRecordLength`，`FileRecordBuffer` 不从第 8 字节开始。
- [ ] 用 `offset_of!(NTFS_FILE_RECORD_OUTPUT_BUFFER, FileRecordBuffer)` 取得数据偏移；输出缓冲至少为该偏移加 record segment 大小。
- [ ] `DeviceIoControl` 返回后同时校验 `bytes_returned`、`FileRecordLength` 和缓冲边界；只把实际 record 字节返回给上层。
- [ ] `FSCTL_GET_NTFS_VOLUME_DATA` 按实际 `bytes_returned` 判断扩展结构是否完整，并验证 `ByteCount`；不能用预分配容量冒充返回长度。
- [ ] 输出缓冲按目标结构对齐，或只用字节读取/`read_unaligned` 解析字段；禁止把任意 `Vec<u8>` 指针直接解引用成对齐结构造成 UB。
- [ ] `VolumeData` 至少保留 `bytes_per_sector`、`bytes_per_cluster`、`bytes_per_file_record_segment`、`mft_valid_data_length`、版本号和向上取整的 `slot_count`。
- [ ] 在任何 NTFS 专属控制码前，通过现有 `volume_info` 的扩展接口取得实际文件系统名称；非 NTFS 返回结构化 `UnsupportedFilesystem(actual)`。

`read_mft_record` 的边界返回值使用明确结构，不再混合输出头和记录体：

```rust
pub struct RawFileRecord {
    pub file_reference: u64,
    pub bytes: Vec<u8>,
}
```

#### 0.2 ABI 单测

- [ ] 断言生成绑定中的 `FileRecordBuffer` offset 与分配/切片逻辑一致。
- [ ] 给定短于 output header、`FileRecordLength` 越界、`bytes_returned` 不足的缓冲，统一返回错误且不 panic。
- [ ] 给定缺失/截断的扩展卷数据，返回 `InvalidVolumeData`，不得把零填充区解析成版本号。
- [ ] `ERROR_ACCESS_DENIED` 精确映射为 `VolumeError::AccessDenied`，其余 Win32 错误保留数值 code。

#### 0.3 spike 正确性

- [ ] 从 `slot_count - 1` 请求，始终使用 API 实际返回的低 48 位记录号推进。
- [ ] 返回号必须 `<= request`，否则立即 no-go；重复记录立即 no-go，不能只打印警告。
- [ ] 返回记录 0 时先计入结果，再终止；禁止在处理前跳过 0。
- [ ] 记录根 5 是否存在，并解析最小 FILE 签名/in-use 字段，避免只测系统调用吞吐。
- [ ] “无遗漏”必须有独立对照：用受信任的 NTFS 工具导出同一时点的在用记录号集合，或读取 `$MFT::$BITMAP` 形成基准集合；仅有 `HashSet` 去重不能证明无遗漏。
- [ ] 对扫描期间轻微变化单独记录新增/消失差异，不把可解释竞态混成稳定卷遗漏。
- [ ] 逐记录请求失败只在确认属于可恢复、可定位的记录变化时继续；普通 I/O/句柄错误立即 no-go，不能逐槽盲目递减掩盖全局错误。

#### 0.4 spike 性能

- [ ] 在约 10 万、100 万、500 万在用记录的 NTFS 卷分别运行 release 构建。
- [ ] 记录在用记录数、槽位数、耗时、吞吐、实际进程峰值工作集、重复数、遗漏数、扫描中变化数。
- [ ] 100 万在用记录需小于 10 秒且额外峰值内存小于 512 MB。
- [ ] 结果文档明确写 `go` 或 `no-go`，附机器、卷介质、NTFS 版本和测量方法。
- [ ] no-go 时停止逐记录正式实现，先把 T2 改为“解析 `$MFT` 记录 0 的 `$DATA` Data Run 后批量读取”；不得靠并行大量 IOCTL 绕过门槛。

#### 0.5 验证与提交

```powershell
cargo test --manifest-path src-tauri/Cargo.toml win32::tests
cargo build --manifest-path src-tauri/Cargo.toml --example mft_spike --release
cargo run --manifest-path src-tauri/Cargo.toml --example mft_spike --release -- C
```

只暂存本任务四个文件后提交：

```text
feat(mft): 验证 MFT 枚举 ABI 与 go-no-go 门槛
```

---

## 阶段 1：平台边界

### 任务 1：完整 win32 卷读取与 UAC 封装

**文件：** `src-tauri/src/win32.rs`、`src-tauri/Cargo.toml`

- [ ] `windows` feature 在 T0 基础上增加 `Win32_UI_Shell`、`Win32_UI_WindowsAndMessaging`；继续使用已有 `Win32_Foundation`。
- [ ] 把 T0 的最小读取代码整理成生产接口；`mft.rs` 不直接导入 `DeviceIoControl`。
- [ ] `open_volume("C")` 使用 `\\.\C:`、只读访问、共享读写，并通过 `Drop` 关闭有效句柄。
- [ ] 输入盘符严格校验为单个 ASCII 字母；不从任意字符串静默取首字符。
- [ ] `VolumeError` 至少区分 `AccessDenied`、`UnsupportedFilesystem { actual }`、`InvalidData`、`Io { code, operation }`。
- [ ] `read_volume_data` 校验所有除数非零、signed 长度非负、slot 向上取整不溢出，并只接受明确的 NTFS 3.1。
- [ ] `request_elevation` 使用宽字符 `OsStrExt`，不能把 exe 路径 `to_string_lossy()` 后再传给 Windows。
- [ ] `ShellExecuteW("runas")` 成功和 `ERROR_CANCELLED`/启动失败分开返回；后端不主动退出旧实例。
- [ ] `tauri.conf.json` 与 capability 在 T8 前已有可执行的安全收敛任务，T1 不以“人工确认现状”替代实现。

测试必须覆盖：盘符校验、短缓冲、实际返回长度、错误码映射、record 0、输出 reference 与 record bytes 分离。依赖真实管理员权限的测试使用显式环境 gate，不在普通 CI 中作不稳定断言。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml win32::tests
cargo build --manifest-path src-tauri/Cargo.toml
```

提交：`feat(win32): 完成 NTFS 卷读取与 UAC 平台边界`

---

## 阶段 2：MFT 引擎

### 任务 2：紧凑数据结构与正确枚举

**文件：** `src-tauri/src/mft.rs`、`src-tauri/src/lib.rs`

#### 2.1 数据模型

- [ ] 定义 `FileRef { record_no, sequence }`，所有父引用比较同时检查两部分。
- [ ] 定义 `MftName`、`MftRecord`、`DirectFileStats`、`MftIndex` 和 `MftError`，字段与设计 4.2 一致。
- [ ] `MftError` 单独包含 `Cancelled`，用户取消不能伪装成 I/O 失败。
- [ ] 明确诊断口径：
  - `scanned_records` = API 返回并检查过的在用记录总数，包含最终被判坏的记录。
  - `skipped_records` = 上述记录中因内容损坏/竞态无法使用的子集。
  - `scanned_files` 后续按文件 record 计数，不按硬链接入口计数。
  - `hard_link_entries` 单独统计额外长名入口。

#### 2.2 可测试读取接口

```rust
pub trait RecordReader {
    fn read(&self, requested_record: u64) -> Result<RawFileRecord, MftError>;
}
```

- [ ] mock 必须模拟真实语义：请求空洞时返回 `<= request` 的最近在用记录，而不是对每个空槽报错。
- [ ] 枚举器与解析器通过内部回调/trait 分离，使 T2 可以用测试 parser 验证循环，但不创建会被 T3 替换的假生产 parser。

#### 2.3 枚举循环

- [ ] `slot_count == 0` 或版本不支持在第一次读取前失败。
- [ ] 每轮先检查 cancel；读取后验证 `returned <= requested` 和未重复。
- [ ] 对返回记录执行解析/分类后，如果 `returned == 0` 才结束；否则下一请求为 `returned - 1`。
- [ ] 根 5 在完整枚举后仍缺失时返回 `RootRecordMissing`。
- [ ] 坏记录阈值使用语义明确且无溢出的条件：`skipped > 100 && skipped.saturating_mul(100) > scanned_records`。
- [ ] 每 4096 条或固定时间间隔发送进度，避免坏记录导致进度不增长。
- [ ] 聚合/解析期间也检查 cancel，不能只在 IOCTL 循环检查。

#### 2.4 测试

- [ ] `[100, 80, 5, 0]` 的 mock 返回序列证明按返回号递减而非逐槽扫描。
- [ ] 记录 0 被交给 parser 一次且只一次。
- [ ] 返回号大于请求、重复返回、根缺失、预取消、处理中取消、错误阈值分别有测试。
- [ ] 读取空洞不增加 `skipped_records`；只有实际返回的坏记录才增加。
- [ ] 测试请求序列精确相等，不能只断言“都大于 0”。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml mft::tests::enumeration
```

提交：`feat(mft): 实现无遗漏且可取消的 MFT 枚举`

### 任务 3：FILE record 解析、扩展记录合并与真实 fixture

**文件：**

- 修改：`src-tauri/src/mft.rs`
- 修改：`src-tauri/Cargo.toml`（增加 `proptest` 等 dev dependency）
- 创建：`src-tauri/tests/mft_fixture.rs`
- 创建：`src-tauri/tests/fixtures/ntfs_sample/*`
- 创建：`src-tauri/tests/fixtures/ntfs_sample/README.md`

#### 3.1 真实 fixture 是前置条件

- [ ] 从专用小 NTFS 卷导出脱敏记录，README 记录卷参数、导出方法、每个 record 的预期含义与校验值。
- [ ] fixture 必须覆盖根 5、namespace 0/1/2/3、父 sequence、多长名/硬链接、resident/non-resident `$DATA`、命名数据流、`$ATTRIBUTE_LIST`、extension record、reparse tag 和损坏记录。
- [ ] fixture 必须入库并在普通 `cargo test` 执行；不得标记 `ignore` 后完成 T3。

#### 3.2 FILE header 与 USA

- [ ] 用常量集中定义并测试 FILE header 字段，不从手写测试反推偏移：USA offset/count 在 `0x04/0x06`，sequence 在 `0x10`，first attribute 在 `0x14`，flags 在 `0x16`，bytes in use 在 `0x18`，base reference 在 `0x20`。
- [ ] record number 以 IOCTL 返回 reference 的低 48 位为权威；header 自带字段只用于一致性诊断。
- [ ] USA 使用卷的 `bytes_per_sector`；验证 `usa_count == record_len / bytes_per_sector + 1`、替换数组完整、每个 sector 尾部原值等于 USN，全部通过后才写回副本。
- [ ] 不硬编码 USA offset 为 `0x30`，不硬编码 sector 为 512，不跳过 USN 校验。

#### 3.3 属性遍历

- [ ] 每次读取属性 type/length 前做边界检查；length 为 0、短于该属性最小 header、越过 `bytes_in_use` 均返回 `BadRecord`。
- [ ] resident value 同时验证 value length 和 value offset。
- [ ] non-resident `$DATA` 读取逻辑大小时验证 header 长度，并用 `lowest_vcn == 0` 保证一个 stream 只累计一次。
- [ ] stream identity 至少包含属性名称与 attribute id；同一 stream 的 extension extent 不重复累计逻辑大小。
- [ ] `$FILE_NAME` 解码前验证 UTF-16 单元长度；保留 namespace 1/3，只有完全没有 1/3 时才回退 0，纯 DOS 2 不建边。
- [ ] `$REPARSE_POINT` 只解析 tag；此处不判断是不是本工具 junction。

#### 3.4 `$ATTRIBUTE_LIST` 与 extension record

- [ ] extension record 的 `base_record` 非零时绝不单独分类成文件或目录。
- [ ] 枚举过程维护有界 `pending_extensions`；基础记录出现时按完整 `FileRef` 和 attribute-list entry 合并。
- [ ] attribute list 引用尚未枚举的记录时允许通过 reader 精确读取，但必须验证返回 record 正是所请求值，并在主循环中去重。
- [ ] 合并键包含 attribute type、名称、attribute id、lowest VCN；每个逻辑 stream 只累计一次。
- [ ] resident `$ATTRIBUTE_LIST` 完整解析；non-resident list 若当前路线不能安全读取其 Data Run，必须把对应基础记录标为不可安全合并并跳过大小，不能当成空列表继续。
- [ ] `pending_extensions` 设置明确的记录数/字节上限，超限返回结构化快速扫描失败，不能让损坏卷无限占用内存。
- [ ] 无法安全合并时增加 skipped/诊断并跳过该记录的大小，不能静默发布任意值。
- [ ] 任务结束时仍未解析的 extension/base 关系进入诊断，不进入正常目录树。

#### 3.5 分类

- [ ] 记录 0..15 中只有根 5进入目录索引；其余有效系统记录的所有逻辑数据流进入 `system_metadata`。
- [ ] 普通文件按每个有效长名入口累加到完整父 `FileRef`；额外入口单独增加 hard-link 诊断。
- [ ] 目录仅保存必要名称、父引用、逻辑大小、reparse tag，不持久保存所有普通文件路径。

#### 3.6 健壮性与验证

- [ ] proptest 对任意截断点、属性长度、USA offset/count、UTF-16 长度运行，保证不 panic、不越界、循环必终止。
- [ ] fixture 断言解析出的明确名称、父引用、sequence、stream 大小、reparse tag，而不只断言 `is_ok()`。
- [ ] 扩展 extent、命名流和多硬链接各有独立精确大小测试。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml mft::tests
cargo test --manifest-path src-tauri/Cargo.toml --test mft_fixture
```

提交：`feat(mft): 完成经真实 fixture 验证的 NTFS 记录解析`

---

## 阶段 3：聚合、标注与快照

### 任务 4：线性构建 DirectoryGraph

**文件：** `src-tauri/src/scanner.rs`

- [ ] 第一遍创建所有目录节点，第二遍才连接父子，不能依赖 MFT 枚举顺序。
- [ ] 内部索引使用完整 `FileRef` 或在 record 索引命中后显式验证 sequence；普通文件父引用也必须验证 sequence。
- [ ] 根 5 是唯一锚点；缺父、sequence 陈旧、重复目录入口、循环节点和不可达节点进入 diagnostics，不伪挂到根下。
- [ ] 路径与深度用显式栈从根构建，避免递归栈溢出；使用三色/访问状态检测循环。
- [ ] 路径建立后先标出排除子树，再执行后序聚合；被排除节点及其文件绝不进入祖先大小、文件数或目录数。
- [ ] 后序聚合使用 O(V+E) 拓扑/显式栈算法，不使用反复扫描 pending 集合和经验 guard。
- [ ] `size_bytes`、`file_count`、`dir_count` 全部使用 saturating arithmetic。
- [ ] `DirectoryGraph` 只暴露从根可达且未排除的正常节点；orphan 单独留在 diagnostics。

测试至少覆盖：

- [ ] 输入顺序为父先、子先、随机顺序时结果完全一致。
- [ ] 陈旧目录父 sequence 和陈旧文件父 sequence 都不污染新目录。
- [ ] 排除多层子树后祖先大小/计数正确。
- [ ] 深链、环、缺父不会 hang 或栈溢出。
- [ ] 根直接文件与一级目录子树统计分开可取得。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml scanner::tests::graph
```

提交：`feat(scanner): 线性构建并聚合完整目录图`

### 任务 5：预设、迁移/链接状态、可见性与根汇总

**文件：** `src-tauri/src/scanner.rs`、`src-tauri/src/models.rs`

- [ ] 在 `DirNode` 増加 `matched_preset`、`access_state`、`is_junction`、状态、迁移 id、linked target size、depth、visible。
- [ ] MFT 节点默认 `AccessState::Unknown`；filesystem 后续产生 Accessible/Inaccessible。
- [ ] 预设匹配复用一次性构建的规范化索引，不为每个节点重复展开环境变量。
- [ ] 精确迁移状态由纯函数映射全部 `MigrationStatus`，并锁定优先级 `LinkBroken > MigrationPending > Migrated > ExistingLink`。
- [ ] `linked_target_size_bytes` 单独记录目标大小，永远不覆盖代表 C 盘占用的 `size_bytes`。
- [ ] `junction::exists` 用于确认 reparse 类型确实是 junction，`resolve/verify` 用于判断目标有效性；任意 reparse tag 不能自动变成 junction。
- [ ] 没有精确迁移记录的已确认 junction 标为 `ExistingLink`；有迁移记录的节点按迁移状态和有效性产生 `Migrated`/`MigrationPending`/`LinkBroken`。
- [ ] `ContainsLink` 只从确认的 junction 向上传播；`ContainsMigrated` 优先于 `ContainsLink`，精确状态不被祖先状态覆盖。
- [ ] 祖先传播检测循环并有上界，不能对损坏图无限循环。
- [ ] 阈值使用 `min_size_mb.saturating_mul(1024 * 1024)`。
- [ ] 大目录、预设、状态、Inaccessible 及其祖先可见；普通小目录隐藏；排除节点已在此前移除。
- [ ] `minSizeMB=0` 显示全部正常目录。
- [ ] 根文件汇总用图中实际根 `FileRef` 查找直接文件，不硬编码 sequence=1。
- [ ] MFT 有 skipped record 时 `incomplete=true`；filesystem 的系统元数据为 `None` 且始终 incomplete。
- [ ] `total_known_size_bytes` 对直接文件和已知系统元数据使用 `saturating_add`。

测试逐项对应设计第 10 节的阈值、状态优先级、junction 分类、链接目标大小和根汇总矩阵。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml scanner::tests::annotation
cargo test --manifest-path src-tauri/Cargo.toml scanner::tests::visibility
cargo test --manifest-path src-tauri/Cargo.toml scanner::tests::root_summary
```

提交：`feat(scanner): 完成树状态标注与可见性规则`

### 任务 6：不可变 TreeStore、分页、推荐和 reveal

**文件：** `src-tauri/src/models.rs`、`src-tauri/src/app_state.rs`、`src-tauri/src/scanner.rs`、`src-tauri/src/lib.rs`

- [ ] `TreeNode`、`AccessState`、`ScanSource`、`RootFileSummary`、`ChildPage`、`RevealLevel` 与设计第 5 节一致。
- [ ] `TreeStore` 保存 nodes、显式 parent 索引、按 size 倒排的 children、roots、root summary、source、scan id、filtered root count、recommended；同大小节点以规范化 path 升序作稳定 tie-break。
- [ ] 内部路径 key 使用统一的 Windows 大小写无关规范化形式，同时保留原展示 path。
- [ ] 只物化 `visible=true` 节点；根可作为内部导航父节点存在，但不作为扫描结果行返回。
- [ ] `child_count` 只统计直接可见子目录；`filtered_child_count` 只统计因阈值不可见的直接正常子目录。
- [ ] `children_page` 在序列化前分页，limit clamp 到 `1..=500`，offset 越界返回空页且 total 保持正确。
- [ ] `reveal_pages` 使用 parent 索引 O(depth) 建链，并为每层返回包含下一节点的实际定位页；目标不存在返回明确错误。
- [ ] recommended 从全部可见节点单遍构建并去重，仅含：命中预设、无状态、非 reparse、非 Inaccessible。
- [ ] recommended 低于阈值仍可存在，`auto_migrate=false` 保留，由前端显示需确认。
- [ ] 构建完成后才以 `Arc<TreeStore>` 一次性替换 `RwLock<Option<...>>`；读取命令只在锁内 clone Arc。

测试覆盖 size 排序（含稳定 tie-break）、500 上限、offset 边界、跨页 reveal、推荐去重与所有排除资格、只含可见节点。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml app_state::tests
cargo test --manifest-path src-tauri/Cargo.toml scanner::tests::materialize
```

提交：`feat(scan): 新增不可变分页树快照`

---

## 阶段 4：IPC、提权、降级与失效

### 任务 7：结构化 scan_drive 与读树 IPC

**文件：** `src-tauri/src/models.rs`、`src-tauri/src/error.rs`、`src-tauri/src/commands.rs`、`src-tauri/src/lib.rs`

#### 7.1 固定 JSON 合约

Rust 使用内部标签和结构体变体，避免 `content` 把不同变体错误包进同一个字段：

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScanDriveResult {
    NeedsElevation,
    FastScanUnavailable { reason: FastScanFailure },
    Complete { snapshot: ScanSnapshot },
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FastScanFailure {
    UnsupportedFilesystem { actual: String },
    UnsupportedNtfsVersion { major: u16, minor: u16 },
    InvalidVolumeData,
    RootRecordMissing,
    ExcessiveRecordErrors { skipped: u64, scanned: u64 },
    Io { code: Option<i32> },
}
```

- [ ] Rust 测试对每个变体执行 `serde_json::to_value` 并断言完整 JSON；TypeScript 测试读取这些固定 fixture，而不是把手写 JSON `as` 成类型后只检查字段。
- [ ] `StaleScan` 暴露稳定错误 code/token，前端不依赖本地化错误文案。

#### 7.2 scan_drive

- [ ] `scan_drive(Auto)` 先读取实际文件系统，再尝试 MFT；`Mft` 不降级；`Filesystem` 不触碰卷句柄。
- [ ] 所有阻塞扫描用 `tauri::async_runtime::spawn_blocking(...).await`，不在 async command 中用 scoped thread 同步 join。
- [ ] 扫描 token/互斥锁在成功、结构化失败、取消、worker panic 的所有出口清理。
- [ ] `cancel_scan` 只设置当前 token；没有运行中任务返回 false，MFT 枚举、聚合和 filesystem worker 都会观察同一 token。
- [ ] `AccessDenied` 返回 `NeedsElevation`；非 NTFS、版本、卷数据、根缺失、坏记录和不可恢复 I/O 全部映射 `FastScanUnavailable`。
- [ ] `FastScanFailure::Io` 保留原始 code；不能再映射为普通 `Store("MFT 扫描失败")`。
- [ ] `Cancelled` 返回取消错误，不发布快照且保留旧快照。
- [ ] `ScanProgressEvent` 固定为设计字段 `{ scanned_records, scanned_dirs, scanned_files, estimated_record_slots, current_phase }`；删除旧 `current_path` 合约。
- [ ] reading、aggregating、annotating、walking_fs 各阶段发送准确进度；不适用计数为 0。
- [ ] snapshot diagnostics 使用 graph 的 orphan 计数和文件 record 计数，不从硬链接入口数反推。
- [ ] 只有完整成功后发布 store，再从同一个 store 构造 snapshot，避免返回值与已发布快照分叉。

#### 7.3 读树命令

- [ ] `expand_node`、`reveal_node`、`list_recommended` 先在短锁内验证 scan id 并 clone Arc，然后释放锁。
- [ ] scan id 不匹配或当前树为空都返回 `StaleScan`。
- [ ] 三个命令不占扫描锁。

#### 7.4 可注入测试边界

- [ ] 把扫描 engine/open-volume 行为抽成内部 trait，使 commands 测试不依赖真实 C 盘或管理员权限。
- [ ] 测试 NeedsElevation/FastScanFailure 返回前锁已释放、旧快照保留、新快照原子替换、旧 id stale、分页/reveal/list 全部守卫。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml commands::tests
cargo build --manifest-path src-tauri/Cargo.toml
```

提交：`feat(commands): 新增结构化扫描与快照守卫 IPC`

### 任务 8：提权后端与安全约束

**文件：** `src-tauri/src/commands.rs`、`src-tauri/src/lib.rs`、`src-tauri/tauri.conf.json`、`src-tauri/capabilities/default.json`

- [ ] `restart_elevated` 只启动 `current_exe --elevated-scan` 并返回 `true`；UAC 取消/失败返回可识别错误且不退出旧实例。
- [ ] 后端任何分支都不直接退出当前进程；“成功后关闭旧窗口”的前端动作在 T11 与扫描状态机一起完成。
- [ ] 新实例把启动意图存为一次性 bool；`take_startup_scan_intent` 第二次必为 false。
- [ ] 为高完整性进程设置非空、仅允许本地资源的 CSP；确认没有 shell/process 任意执行 permission。
- [ ] 增加配置测试/静态断言，确保没有远程 URL、remote capability、任意命令行或可执行路径 IPC 参数。

测试覆盖：启动意图一次性、ShellExecute 成功/取消映射、任何结果都不由后端退出旧进程、安全配置不开放远程/任意执行能力。

提交：`feat(commands): 完成受限的整体提权重启闭环`

### 任务 9：有界并发 filesystem 降级

**文件：** `src-tauri/src/scanner.rs`、`src-tauri/src/commands.rs`、必要的新依赖

- [ ] worker 数默认 `min(available_parallelism, 4)`，强制 clamp 到 `1..=8`。
- [ ] 使用有界目录任务队列；调度和 worker 分离，队列容量有明确上限，不能为每个目录无界 spawn。
- [ ] worker 只产生 `DirectoryObservation`（path、parent、直接文件统计、access/reparse 状态），不并发修改祖先累计值。
- [ ] coordinator 分配稳定 node id 并构图；根 id 必须来自实际插入结果，不能预留一个未使用 id。
- [ ] 遍历完成后在单线程执行与 MFT 相同的排除、标注、后序聚合、可见性和物化。
- [ ] `read_dir` AccessDenied 把当前目录标成 Inaccessible；普通 entry 错误进入诊断，不能静默标 Accessible。
- [ ] reparse point 记录 tag/unknown marker 后停止下钻；只有 `junction` 验证成功才设置 is_junction。
- [ ] 每个普通文件同时累加直接大小、node file_count 和全局 scanned_files。
- [ ] `C:\` 直接普通文件注入 `RootFileSummary`；`system_metadata_size_bytes=None`、`incomplete=true`。
- [ ] 任一 worker/调度循环检查共享 cancel；取消后停止派发、等待 worker 收敛并返回 `Cancelled`，绝不发布部分树。
- [ ] 固定磁盘默认并发；对可移动/网络介质降到 1。当前范围只扫 C 盘，但策略通过函数表达并测试。

为了稳定测试 AccessDenied、reparse 和 I/O 错误，引入可注入 `FsReader`，不要依赖 CI 用户 ACL。

测试覆盖：与 MFT 图契约一致、根 id、文件数、排除、不访问 reparse target、Inaccessible、取消不发布、worker 上限、根文件汇总。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml scanner::tests::fs_fallback
cargo test --manifest-path src-tauri/Cargo.toml commands::tests::filesystem
```

提交：`feat(scan): 实现有界并发 filesystem 树扫描`

### 任务 10：文件系统操作后的整树失效

**文件：** `src-tauri/src/commands.rs`、`src-tauri/src/models.rs`、`src-tauri/src/migrator.rs`

- [ ] 失效辅助在一个 write lock 内 `take()` 当前 `Arc<TreeStore>`，从被取出的 store 读取 source；不再维护可能与 tree 分叉的 `last_scan_source` 占位字段。
- [ ] 事件 `{ reason, auto_rescan }` 中，只有被取出快照 source 为 MFT 时 `auto_rescan=true`。
- [ ] 所有真正改变源路径/链接形态的成功命令调用同一辅助：迁移、还原、断链，以及未来/现有的失效链接清理命令。
- [ ] migrator 返回明确的 `source_changed`/结果状态；pending/manual-confirm 按实际路径变化决定是否失效，不能简单按 `Ok`/`Err` 猜测。
- [ ] 失败且已完全回滚的操作不失效；失败但源路径已经变化的操作必须失效。
- [ ] 清 tree 发生在 emit 前；即使事件投递失败，旧 scan id 也已 stale。

测试必须先发布真实合成 TreeStore，再断言清除；不得以初始 `None` 测“清除成功”。覆盖成功、完整回滚、pending 路径已变/未变、MFT/filesystem auto_rescan。

提交：`feat(commands): 文件系统变化后原子失效扫描树`

---

## 阶段 5：前端

### 任务 11：IPC 类型、全局监听与分页树 store

**文件：**

- `src/ipc/types.ts`
- `src/ipc/invoke.ts`
- `src/ipc/events.ts`
- `src/stores/scan.ts`
- `src/stores/scan.test.ts`
- `src/ipc/types.test.ts`
- `src/App.vue`

#### 11.1 类型与合约

- [ ] store 状态至少包含 roots、scanId、filteredRootCount、rootFileSummary、recommended、expanded pages、expandedKeys、loading/cancelling、progress、invalidated 和定位高亮。
- [ ] TypeScript 精确镜像 T7 的内部标签：`Complete` 有 `snapshot`，快速失败有顶层 `reason`，reason 自身有 `kind`。
- [ ] `ScanProgressEvent.currentPhase` 使用固定 union，不用 `| string` 使拼写错误失去检查。
- [ ] invoke 删除 `scanDrives`，新增 scan/expand/reveal/recommended/elevation/startup intent。
- [ ] 合约测试读取 Rust 生成并提交的 JSON fixture，覆盖每个 enum variant。

#### 11.2 store 初始化与失效监听

- [ ] store 暴露幂等 `initialize()`；它注册一次 `dayu://tree-invalidated` 并将 unlisten 保留到应用退出/`$dispose`。
- [ ] 失效监听不放在 `scan()` 的 try/finally 中，不能扫描结束就注销。
- [ ] App 启动顺序固定为：取得 scan store -> `await initialize()` -> `takeStartupScanIntent()` -> 必要时 `scan('mft')`。
- [ ] 收到失效事件同步清空 scan id、roots、summary、recommended、分页、展开、定位高亮和错误的旧体积。
- [ ] `autoRescan=true` 排队执行一次 MFT scan；filesystem 只进入“结果已失效”状态，不显示“正在重扫”。

#### 11.3 scan 状态机

- [ ] 一次 `scan()` 内用循环处理 Auto -> NeedsElevation/FastFailure -> Filesystem，不能在 `loading=true` 时递归调用受 guard 保护的 `scan()`。
- [ ] 用户拒绝提权直接把下一轮 mode 设为 filesystem。
- [ ] 用户接受提权后检查 `restartElevated()` 的 bool；true 才关闭旧窗口，false/异常继续提供 filesystem。
- [ ] UAC 取消/启动失败时旧实例保持可用，并立即提供 filesystem 选择，不能只显示技术错误。
- [ ] 用户接受快速失败降级后实际发起 filesystem；拒绝则保留旧快照或空态。
- [ ] 对每个 `FastScanFailure.kind` 提供穷尽的面向用户说明，不能统一退化成“快速扫描不可用”。
- [ ] `loading/cancelling` 在整个多轮流程只由最外层 finally 收敛。

#### 11.4 异步分页防串写

- [ ] toggle/loadMore/reveal/listRecommended 发请求前捕获当前 scan id；响应后再次比较，只有仍一致才写缓存。
- [ ] 后端即使已 clone 旧 Arc 并成功返回，旧响应也不能写入新快照。
- [ ] stable `StaleScan` code 统一调用 `clearSnapshot()` 并进入 invalidated 状态。
- [ ] loadMore 对同一路径去重并防并发重复 offset；reveal 写入定位页时不制造重复节点。

测试覆盖设计第 10 节所有前端分支：接受/拒绝提权、UAC 失败、接受/拒绝 fallback、全局失效、MFT 自动重扫、filesystem 手动态、旧异步响应、StaleScan、追加页。

```powershell
pnpm test -- src/stores/scan.test.ts src/ipc/types.test.ts
pnpm build
```

提交：`feat(frontend): 新增一致的分页树扫描状态机`

### 任务 12：缩进树表、根汇总与推荐定位

**文件：** `src/views/ScanView.vue`、`src/views/ScanView.test.ts`、必要样式

- [ ] 默认渲染 roots；仅 `childCount>0` 显示展开按钮。
- [ ] 展开节点渲染已加载页；最后一页有 `nextOffset` 时显示该层“显示更多”。
- [ ] 行 key 使用 node path，不能用数组 index。
- [ ] 每行展示名称、完整路径、大小、类别、access/status、filtered child 提示和符合预检入口语义的操作。
- [ ] reparse/ExistingLink/Inaccessible 节点不出现误导性的直接迁移入口；最终资格仍由 precheck 决定。
- [ ] recommended 为空时隐藏区域；`autoMigrate=false` 明确显示需确认风险。
- [ ] 点击 recommended 后等待 `reveal()` 完成和 DOM 更新，滚动到目标并设置短暂高亮；只有展开缓存不算完成定位。
- [ ] `filteredRootCount>0` 显示一级隐藏数；roots 为空仍显示根文件汇总和阈值空态。
- [ ] 根文件汇总不可点击、无展开和迁移入口，并在 incomplete 时明确提示可能不完整。
- [ ] filesystem 失效显示“结果已失效，请重新扫描”；只有 MFT 自动重扫时显示正在重扫。

组件测试覆盖：根与多级展开、显示更多、推荐跨页定位后的 scroll/highlight、根汇总、空结果、filtered 提示、状态按钮资格、长路径不溢出。

```powershell
pnpm test -- src/views/ScanView.test.ts
pnpm build
```

提交：`feat(frontend): 完成按需展开扫描树与推荐定位`

---

## 阶段 6：端到端与发布门槛

### 任务 13：清理旧接口并完成 e2e

**文件：** 按本任务实际修改显式列出，不使用全仓暂存。

#### 13.1 清理

- [ ] `rg -n "scan_drives|scanDrives|ScanItem\b|push_if_big_or_preset|mft_parse" src src-tauri/src`，确认旧扫描结果链和占位 parser 已移除；仍被 links/history 合理使用的通用类型逐项说明。
- [ ] 删除旧 `scan_drives` 命令和 invoke 注册。
- [ ] 用 `rg -n 'TODO|占位|首版可接受|后续补|#\[ignore\]'` 搜索，逐项确认没有绕过本设计门槛的残留。
- [ ] 确认 `is_junction` 没有任何 `reparse_tag.is_some()` 直接赋值路径。
- [ ] 确认 filesystem worker 实际启用有界并发，不把它降为性能评估项。

#### 13.2 自动测试

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo test --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
pnpm test
pnpm build
```

- [ ] Rust JSON fixture 与 TypeScript 合约双向一致。
- [ ] MFT fixture 在普通测试中实际运行。
- [ ] 新旧 scan id 竞态、取消、失败保留旧树、迁移失效全部有自动测试。
- [ ] filesystem 与 MFT 对同一合成目录图的结构契约一致。

#### 13.3 手工 Windows e2e

- [ ] 管理员 NTFS：MFT 扫描、roots size 排序、展开、分页、推荐定位、根汇总。
- [ ] 非管理员：NeedsElevation 后拒绝，确认真正启动 filesystem；取消扫描不发布部分结果。
- [ ] 提权同意：新实例先注册监听再自动 MFT 扫描，旧实例只在启动成功后关闭。
- [ ] UAC 取消：旧实例保留且能继续普通扫描。
- [ ] 非 NTFS/注入快速失败：展示结构化用户说明并能显式降级。
- [ ] 调整 `minSizeMB` 后新 scan id 生效；强制可见节点和祖先仍存在。
- [ ] 迁移/还原/断链：旧树立即清空；MFT 自动重扫，filesystem 等待用户重扫。
- [ ] 推荐点击能跨页展开、滚动、高亮目标。
- [ ] 10 万/100 万/500 万性能结果仍满足 T0 采用路线；若真实性能回归则阻止发布。

#### 13.4 最终对照

逐节对照设计 0-12，特别确认：

- [ ] MFT 记录 0 被处理，根 5 被验证。
- [ ] USA、ATTRIBUTE_LIST、namespace、sequence、硬链接语义与 fixture 一致。
- [ ] 排除子树不污染祖先统计。
- [ ] 根文件不进入树且不能迁移。
- [ ] 所有 TreeStore 读取都守卫 scan id。
- [ ] 快速失败不静默降级。
- [ ] tree-invalidated 是应用级监听。
- [ ] 有界并发和安全约束已实现，不是备注。

只暂存本任务确实修改的文件后提交：

```text
test(scan): 串通 MFT 树扫描与失效流程
```

## 执行交接

执行者从 T0 开始。T0 结果文档没有明确 `go` 前，不得把 T2-T12 的代码提交为正式实现；可以提前讨论接口，但不能用未经验证的逐记录路线锁定后续代码。

每个任务完成时在本计划对应复选框更新状态，并附实际测试输出摘要。任何无法满足的强制项都应回到设计评审，不得自行改写成“可选增强”。
