# WizTree 式按需展开扫描 — 设计规格

> 日期：2026-07-20
> 状态：评审修订完成，待 MFT 可行性验证
> 主题：把 dayu-disk-manager 的扫描从"全盘递归 + 扁平阈值过滤"改造为"MFT 元数据快速建树 + 树形按需展开"

## 0. 决策汇总

本设计基于头脑风暴与评审阶段已确认的 10 项关键决策：

| # | 决策点 | 选定 |
|---|--------|------|
| 1 | 扫描引擎 | 先验证 `FSCTL_GET_NTFS_FILE_RECORD` 逐记录读取的正确性与性能；达标后解析 `$FILE_NAME` 建树、`$DATA` 算体积，否则改用批量读取 `$MFT` 的实现 |
| 2 | 权限策略 | `scan_drive(Auto)` 返回 `NeedsElevation`，释放扫描锁后由前端选择整体提权重启或多线程 `std::fs` 降级；其他快速引擎错误同样允许用户转普通扫描 |
| 3 | 扫描根 | `C:\` 未排除的一级目录，按子树总大小倒排；`minSizeMB` 参与可见性过滤，但预设/状态/无法访问节点及祖先链强制保留 |
| 4 | 预设场景呈现 | 徽章（节点上标注）+ 顶部"推荐迁移"快捷区；快捷区只返回当前可发起迁移的预设目录 |
| 5 | 迁移状态标注 | 全树递归标注（祖先链传播"包含已迁移/包含软链接"）|
| 6 | 数据传输模型 | 树常驻后端内存 + 前端按需拉子节点 |
| 7 | 失效策略 | 迁移/还原/断链成功后立即使整棵树和前端展开缓存失效；MFT 模式自动重扫，降级模式提示手动重扫，不展示旧数据 |
| 8 | "拿大小"路线 | 候选路线为 `FSCTL_GET_NTFS_FILE_RECORD` 按“返回记录号递减”枚举；USN 无大小字段，不单独承担建树 |
| 9 | 根目录文件 | `C:\` 直接文件和 NTFS 系统元文件作为只读汇总展示，不进入目录树，也不提供迁移入口 |
| 10 | 快速扫描失败 | 非权限类 MFT 失败返回结构化原因；用户确认后走 filesystem 降级，不静默切换，也不只显示技术错误 |

## 1. 背景与问题

当前扫描实现（`src-tauri/src/scanner.rs` 的 `scan_root`）是**后序深度遍历整棵目录树**，边遍历边累加父目录大小，再用 `push_if_big_or_preset` 把"size ≥ min_size **或** 命中预设"的目录全部摊平返回。

由此产生两个问题：

- **粒度太细**：父目录和它的子目录同时进结果列表（只要各自都超阈值），用户看到一个扁平的大列表，里面混着 `C:\Users`、`C:\Users\xxx\AppData`、`C:\Users\xxx\AppData\Local\npm-cache` 等不同层级的目录。
- **效率不高**：部分因粒度细导致 IPC 传输和前端渲染了海量深层节点；底层遍历仍需走遍所有文件。

参考 WizTree 的做法：先扫 `C:\` 一级目录、按大小倒排，用户对感兴趣的大目录逐层展开下级，直到定位到适合软链接的目录再迁移。

## 2. 目标与非目标

**目标：**

- 在可行性门槛通过后，使典型 NTFS 系统盘首次扫描达到秒级，消除"等几分钟"的体验。
- 结果以树形按需展开呈现，粒度由用户的展开行为决定，默认只看一级。
- `minSizeMB` 过滤不值得关注的小目录，同时保留预设命中、迁移/链接状态、无法访问节点及其导航祖先。
- 保留"智能识别预设场景 + 一键迁移"核心价值。
- 保留迁移状态标注（已迁移/包含已迁移/链接异常等）。

**非目标（YAGNI）：**

- USN Journal 增量追踪文件变化（手动重扫足够）。
- 多卷扫描（只针对 C 盘）。
- 单文件链接（README 已明确排除）。
- 扫描结果磁盘缓存（内存树已足够，重扫即重建）。
- 压缩/稀疏文件大小的完全精确（取逻辑大小用于排序足够）。

### 2.1 用户可见性规则

`minSizeMB` 是展示阈值，不是扫描剪枝条件。无论一个目录最终是否显示，扫描引擎都必须先读取其文件统计并完成全树聚合，否则祖先大小会失真。

目录满足以下任一条件时进入可见树：

- `size_bytes >= minSizeMB * 1024 * 1024`；
- 命中任一预设，即使低于阈值；
- 具有 `Migrated`、`MigrationPending`、`LinkBroken`、`ExistingLink`、`ContainsMigrated` 或 `ContainsLink` 状态；
- `access_state == Inaccessible`，用于明确告知扫描覆盖缺口；
- 是上述任一强制可见节点的祖先，作为展开导航链。

其余小目录不进入 `TreeStore`，也不占用 IPC 和前端分页。`file_count` / `dir_count` 仍表示未过滤子树的真实聚合计数，`child_count` 表示直接可见子目录数，`filtered_child_count` 表示因阈值隐藏的直接子目录数。排除路径优先级最高：明确排除的整棵子树即使命中预设或迁移状态也不进入扫描树。阈值换算使用 `saturating_mul`，修改 `minSizeMB`、排除路径或预设后，需要重新扫描生成新快照。

## 3. 架构与模块边界

遵循 README 既定分层，MFT 调用走 `win32.rs` 薄封装，记录解析与聚合在新增的 `mft.rs`，树存储与标注复用 `scanner.rs`。

```
┌─────────────────────────────────────────────────┐
│  Frontend (Vue 3)                                │
│  ├─ ScanView      树形结果展示（缩进树表）        │
│  │                + 顶部"推荐迁移"快捷区         │
│  └─ stores/scan   树 + 按需拉子 + 展开缓存         │
├──── Tauri IPC ──────────────────────────────────┤
│  Backend (Rust)                                  │
│  ├─ mft          MFT 记录解析（新增）            │
│  ├─ scanner      聚合建树 + 预设命中 + 状态标注    │
│  │                + build_tree_from_fs 降级聚合   │
│  ├─ TreeStore    内存树常驻（app_state，新增）     │
│  ├─ commands     scan_drive / expand_node /       │
│  │                reveal_node / elevation /       │
│  │                cancel_scan                     │
│  └─ win32        MFT/UAC/卷句柄 封装（扩展）       │
└─────────────────────────────────────────────────┘
```

**各单元职责与接口：**

| 单元 | 职责 | 对外接口 | 依赖 |
|------|------|---------|------|
| `mft` | 枚举、校验并解析 MFT 记录；保留目录入口，按父目录预聚合文件体积 | `scan_volume(letter, cancel, progress) -> Result<MftIndex, MftError>` | `win32` |
| `scanner` | 把 `MftIndex` 聚合成完整目录图；完成预设/状态标注和阈值可见性计算后物化 `TreeStore` | `build_graph(index, cfg) -> DirectoryGraph`、`annotate_graph(...)`、`materialize_tree(...)`、`build_tree_from_fs(cfg) -> TreeStore`（降级） | `store`（读配置/迁移记录）、`junction` |
| `TreeStore` | 不可变内存快照：可见目录节点表 + 父子索引 + roots | `roots()`、`children_page(...)`、`reveal_pages(...)`、`node(path)`、`recommended()` | 无 |
| `commands` | IPC 编排：触发扫描、返回提权/降级需求、按需分页拉子、树失效 | `scan_drive`、`restart_elevated`、`expand_node`、`list_recommended`、`cancel_scan` | `mft`、`scanner`、`TreeStore`、`win32` |
| `win32` | 卷句柄、MFT DeviceIoControl、UAC 提权 | `open_volume(letter) -> Result<Handle, VolumeError>`、`read_mft_record(handle, ref) -> Vec<u8>`、`request_elevation()` | 无（平台边界）|

## 4. 后端：MFT 扫描引擎（新增 `src-tauri/src/mft.rs`）

整个方案的技术重心与最高风险部分。

### 4.1 可行性门槛（实现前置）

`FSCTL_GET_NTFS_FILE_RECORD` 每返回一条在用记录都需要一次 `DeviceIoControl`，它不是 WizTree 使用的批量 `$MFT` 读取。正式实现树和 UI 前，先完成一个独立 spike：

- 在真实 NTFS 卷上验证枚举无遗漏、无重复、能正确处理 MFT 空洞和扫描期间的轻微变化。
- 在参考 SSD 上分别测量约 10 万、100 万、500 万条在用记录的读取时间与峰值内存。
- 100 万条在用记录完整读取的目标是 10 秒内、峰值额外内存不超过 512 MB；结果写入实现计划。该门槛不是发布性能承诺，而是决定技术路线的 go/no-go 条件。
- 未达标时停止扩展逐记录 IOCTL 方案，改为引入经过验证的 NTFS 解析库，或实现按 Data Run 批量读取 `$MFT`；不得仅通过增大线程数掩盖单记录系统调用成本。

### 4.2 数据结构

```rust
#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct FileRef {
    pub record_no: u64,          // File Reference 低 48 位
    pub sequence: u16,           // File Reference 高 16 位，用于排除已复用的陈旧引用
}

pub struct MftName {
    pub parent: FileRef,
    pub name: String,
    pub namespace: u8,           // 0=POSIX, 1=Win32, 2=DOS, 3=Win32AndDos
}

pub struct MftRecord {
    pub id: FileRef,
    pub base_record: Option<FileRef>, // 扩展记录指向基础记录
    pub names: Vec<MftName>,      // 一个文件记录可有多个硬链接入口
    pub logical_size: u64,        // 所有有效 $DATA 流的逻辑大小
    pub is_dir: bool,             // FILE record header 的目录标志
    pub reparse_tag: Option<u32>, // 从 $REPARSE_POINT 解析，不把所有 reparse point 都叫 junction
}

pub struct DirectFileStats {
    pub size_bytes: u64,
    pub file_count: u64,
}

pub struct MftIndex {
    pub directories: Vec<MftRecord>,
    pub direct_files: HashMap<FileRef, DirectFileStats>,
    pub system_metadata: DirectFileStats, // 0..15 中除根记录 5 外的逻辑大小汇总
    pub estimated_record_slots: u64,
    pub scanned_records: u64,
    pub skipped_records: u64,
    pub orphan_entries: u64,
    pub hard_link_entries: u64,
}

pub enum MftError {
    NeedElevation,               // CreateFileW ERROR_ACCESS_DENIED
    UnsupportedFilesystem(String),
    UnsupportedNtfsVersion { major: u16, minor: u16 },
    InvalidVolumeData,
    RootRecordMissing,
    ExcessiveRecordErrors { skipped: u64, scanned: u64 },
    Io(std::io::Error),
    BadRecord { ref_no: u64 },
}
```

文件不会转换为 `TreeNode`。扫描阶段只把每个文件的逻辑大小和数量累加到其 `MftName.parent`，然后丢弃文件名；因此后端常驻内存规模主要与目录数而不是文件数相关。

硬链接首版仍采用近似策略：同一文件的每个有效长名称入口都给对应父目录累加一次大小。该行为会在不同目录间重复归属大小，但不会随机丢失入口。若后续要展示物理唯一占用，需要另加“按 record 去重”的统计口径，不能复用当前 `size_bytes`。

### 4.3 扫描流程

1. 先用卷信息确认文件系统为 NTFS；不是 NTFS 时返回 `UnsupportedFilesystem`，不调用 NTFS 专属控制码。随后 `CreateFileW(r"\\.\C:", GENERIC_READ, FILE_SHARE_READ|FILE_SHARE_WRITE, OPEN_EXISTING)` 拿卷句柄。`ERROR_ACCESS_DENIED` → 返回 `NeedElevation`，上层触发 UAC。
2. `FSCTL_GET_NTFS_VOLUME_DATA` 的输出缓冲区按 `NTFS_VOLUME_DATA_BUFFER + NTFS_EXTENDED_VOLUME_DATA` 读取：基础结构取得 `BytesPerFileRecordSegment`、`MftValidDataLength`、`BytesPerCluster`，扩展结构取得 `MajorVersion`、`MinorVersion`。扩展部分不足时返回明确错误，不读取越界字段。
3. 校验版本为 NTFS 3.1；用 `slot_count = ceil(MftValidDataLength / BytesPerFileRecordSegment)` 得到记录槽位数。API 不提供 `NumberOfFileRecords`；`slot_count == 0` 作为无效卷数据报错。
4. 从 `slot_count - 1` 向下调用 `FSCTL_GET_NTFS_FILE_RECORD`。API 可能返回“小于等于请求值的最近在用记录”，因此每次必须读取输出 `FileReferenceNumber`，先提取其低 48 位 `returned_record_no`，下一次请求 `returned_record_no - 1`；用 `HashSet` 防御重复，返回 0 后终止，严禁无符号下溢。
5. 对输出缓冲区做长度、`FILE` 签名、Update Sequence Array fixup、属性偏移和属性长度校验。单条损坏或扫描中消失的记录计入 `skipped_records` 后继续；卷句柄失效、结构参数不可信等全局错误才终止扫描。
6. 解析属性流：
   - FILE record header → in-use、目录标志、sequence、base record reference。
   - `$FILE_NAME` → 名称与完整父 File Reference；统一拆成低 48 位 `record_no` 和高 16 位 `sequence`。
   - `$DATA` → resident 取 value length，non-resident 取属性头 logical/real data size，不跟随 Data Run；同一 stream 的扩展 extent 不得重复累加。
   - `$REPARSE_POINT` → reparse tag；`IO_REPARSE_TAG_MOUNT_POINT` 也只能标为 mount-point 类，是否为本工具 junction 仍由 `junction::verify` 判断。
   - `$ATTRIBUTE_LIST` / extension record → 合并回基础记录；无法安全合并时记录诊断并跳过该记录的大小，不能静默产生任意值。
7. 名称选择规则：保留 namespace `1`（Win32）和 `3`（Win32AndDos），跳过纯 DOS 短名 `2`；仅当该记录没有 `1/3` 名称时才回退到 POSIX `0`。不同长名称入口视为硬链接边。
8. 记录号 `0..15` 中仅保留 5 号根目录作为建树锚点；其余系统元文件不进入目录树，但其 `$DATA` 逻辑大小累计到 `system_metadata`。根记录的自引用单独终止，不能形成父子循环。
9. 每次循环都检查 `cancel`；每 4096 条或达到时间间隔时 emit 一次进度。输出紧凑的 `MftIndex`，而不是保存所有文件路径的 `Vec<MftRecord>`。
10. 根记录 5 缺失/无效时整个快速扫描失败。其他坏记录可以跳过，但最终跳过数同时超过 100 条和在用记录的 1% 时返回 `ExcessiveRecordErrors`，不发布可信度不足的部分树；阈值以内则在 diagnostics 中保留数量。实现比例判断时使用 checked/saturating arithmetic，避免乘法溢出。

### 4.4 已知风险与对策（规格明确写明）

- **`$DATA` 非驻留属性**：大文件 `$DATA` 用 Data Run 而非内联，但逻辑大小写在属性头里，不需跟随 Data Run。✅ 安全。
- **多 `$FILE_NAME` 属性**：按 4.3 的 namespace 优先级选长名，纯 DOS 名不建边；多个有效长名保留为多个入口。
- **硬链接重复计数**：同一文件的多个有效入口会分别计入各自父目录。首版接受该近似，并在扫描诊断中返回 `hard_link_entries` 供测试和后续评估。
- **File Reference 复用**：父引用的 record number 与 sequence 都必须匹配当前父记录；sequence 不匹配的边作为 orphan 跳过并计入诊断。
- **NTFS 版本**：首版只支持扩展卷数据明确报告的 NTFS 3.1。
- **解析正确性**：MFT 二进制偏移易错，必须用真实小 NTFS 镜像做 fixture 测试（见第 10 节）。

### 4.5 聚合（放 `scanner.rs`，复用现有工具）

- `HashMap<FileRef, Directory>` 建目录索引；记录号 5 是根（`C:\`），即使位于保留记录区也必须存在。
- 只为目录建立节点。文件已在 `MftIndex.direct_files` 中按父目录预聚合，不进入 `TreeStore`。
- 父 File Reference 的 record number 和 sequence 都匹配后才连接。缺父、陈旧 sequence、循环或无法解析路径的目录放入 diagnostics，不挂到 `C:\` 下伪装成正常节点。
- 两遍后序累加：先把直接文件统计放入目录，再从叶子向根 `saturating_add` 体积、文件数、目录数。
- 展开路径：根 path = `C:\`，子节点 path = `parent.path + "\" + name`。
- 路径构建完成后应用 `ScanContext::preset_index` / `excluded`。被排除目录的整棵子树不进入父级聚合，保持与现有扫描语义一致。
- 聚合先产出内部 `DirectoryGraph`。在完整图上完成预设匹配、迁移/链接状态标注和祖先传播后，再按 2.1 的规则计算 `visible`。
- `materialize_tree` 只把 `visible == true` 的目录写入 `TreeStore`；`children`、`roots` 和 `child_count` 只引用可见节点。这样低于阈值的普通小目录不会通过 IPC 暴露，但它们的大小和计数已经包含在可见祖先中。

## 5. 内存树数据结构（新增 `TreeNode` + `TreeStore`）

现有 `ScanItem` 是扁平无父子概念。新增仅表示目录的 `TreeNode`，采用**扁平存储 + 父子索引**。文件只参与目录聚合，不会出现在 `TreeStore` 或迁移按钮中。

`src-tauri/src/models.rs`：

```rust
pub struct TreeNode {
    pub path: String,              // 完整绝对路径，唯一 key
    pub display_name: String,      // 末级名（预设命中时显示预设名）
    pub size_bytes: u64,           // C: 上该子树的逻辑大小；不跟随 reparse target
    pub linked_target_size_bytes: Option<u64>, // 已迁移节点目标目录大小，仅用于链接详情
    pub file_count: u64,           // 子树文件数
    pub dir_count: u64,
    pub depth: u32,                // 相对 C:\ 深度（C:\=0，一级=1）
    pub is_reparse: bool,
    pub reparse_tag: Option<u32>,
    pub is_junction: bool,         // 由 junction::verify/resolve 确认，不由 reparse flag 猜测
    pub access_state: AccessState, // MFT=Unknown；FS 遍历可为 Accessible/Inaccessible
    pub matched_preset: Option<String>,
    pub category: Option<PresetCategory>,
    pub auto_migrate: bool,
    pub scan_status: Option<ScanItemStatus>,   // 全树递归标注后填入
    pub migration_id: Option<String>,
    pub child_count: u32,          // 直接子目录数；>0 时前端显示展开箭头
    pub filtered_child_count: u32, // 仅因 minSizeMB 隐藏的直接子目录数
}

pub enum AccessState { Unknown, Accessible, Inaccessible }

pub enum ScanSource { Mft, Filesystem }

pub struct RootFileSummary {
    pub direct_file_size_bytes: u64,      // C:\ 直接普通文件，如 pagefile.sys/hiberfil.sys
    pub direct_file_count: u64,
    pub system_metadata_size_bytes: Option<u64>, // MFT 模式可得；filesystem 模式通常未知
    pub total_known_size_bytes: u64,
    pub incomplete: bool,
}

pub struct ChildPage {
    pub items: Vec<TreeNode>,
    pub total: u32,
    pub next_offset: Option<u32>,
}

pub struct RevealLevel {
    pub parent_path: String,
    pub page: ChildPage,           // 包含祖先链下一节点的那一页
}
```

`src-tauri/src/app_state.rs` 新增不可变快照 `TreeStore`。构建和标注全部完成后，才用 `RwLock<Option<Arc<TreeStore>>>` 一次性替换当前快照；读取命令只短暂 clone `Arc`，不在序列化期间持锁。

```rust
pub struct TreeStore {
    scan_id: String,                                       // 每次成功扫描生成 UUID
    source: ScanSource,
    root_file_summary: RootFileSummary,
    nodes: HashMap<String, TreeNode>,                       // path -> node
    children: HashMap<String, Vec<String>>,                  // parent path -> child paths（按 size 倒排）
    roots: Vec<String>,                                      // C:\ 一级子目录 path（按 size 倒排）
    filtered_root_count: u32,                                // 因 minSizeMB 隐藏的一级目录数
    recommended: Vec<String>,                                // 当前可发起迁移的预设目录 path
}

impl TreeStore {
    pub fn roots(&self) -> Vec<TreeNode>;
    pub fn children_page(&self, path: &str, offset: u32, limit: u32) -> ChildPage;
    pub fn node(&self, path: &str) -> Option<&TreeNode>;
    pub fn recommended(&self) -> Vec<TreeNode>;
    pub fn reveal_pages(&self, path: &str, limit: u32) -> Vec<RevealLevel>;
}
```

所有读取 IPC 都必须同时携带 `scan_id`。若它与当前快照不一致，返回 `StaleScan`，前端清空展开缓存，不能把旧扫描的异步响应写进新树。

`children_page` 的 `limit` 后端限制为 `1..=500`，默认 200。分页必须发生在序列化之前，而不只是前端截断 DOM。`reveal_pages` 专供“推荐迁移”定位：返回从一级根到目标节点的祖先链，以及每一层包含下一个祖先的子节点页，确保目标即使不在该层第一页也能被展开和滚动定位。

`RootFileSummary` 始终独立于目录树和 `minSizeMB` 展示。它只解释无法归属到一级目录的已知逻辑占用，没有展开箭头、预设或迁移按钮，也不能当作 C 盘已用空间的精确总计。`total_known_size_bytes` 用直接文件和已知系统元数据做 `saturating_add`。filesystem 降级无法可靠读取 NTFS 系统元文件时设置 `incomplete=true`；MFT 模式只要存在无法归类的 skipped record 也保守标为 incomplete，前端显示汇总可能不完整。

`recommended` 只收录满足以下全部条件的节点：命中预设、没有任何 `scan_status`、不是 reparse point、`access_state != Inaccessible`。`auto_migrate=false` 的预设仍可出现，但必须显示“需确认风险”；最终迁移资格仍由现有 precheck 决定。已迁移、待处理、链接异常和已有链接节点保留在树及链接管理中，不出现在“推荐迁移”区。没有推荐项时整块区域隐藏。

**ScanItem 的去留**：`TreeNode` 取代 `ScanItem` 作为扫描结果载体。`ScanItemStatus` 枚举保留不动（语义通用）。现有 `annotate_migrations` 改为标注树；fallback 中的 reparse/inaccessible 分支改为构造带 `reparse_tag` / `access_state` 的目录节点。

## 6. 迁移状态全树递归标注（改造 `annotate_migrations`）

现有 `annotate_migrations` 只给扁平列表打标。改为在发布 `TreeStore` 前完成三类标注：

1. **精确命中**：节点 path == 某 active migration.source → 标 `Migrated`（或 `LinkBroken`，复用现有 `junction::verify`）；`linked_target_size_bytes` 用 `dir_size(target)`，但不把目标盘大小写入代表 C 盘占用的 `size_bytes`。
2. **向上传播**：对每个命中节点，沿 parent 链向上把祖先标 `ContainsMigrated`（已有该状态的节点跳过，避免覆盖更具体的 `Migrated`）。
3. **junction 子树**：同理，junction 路径的祖先标 `ContainsLink`。

精确状态优先级为 `LinkBroken > MigrationPending > Migrated > ExistingLink`；祖先同时包含迁移记录和其他 reparse link 时优先 `ContainsMigrated`。复用现有 `is_descendant`、`represents_migrated_link`、`normalize` 工具函数，逻辑从“遍历扁平 items”改成树上父链传播。

### 6.1 文件系统变化后的失效规则

迁移、还原、断开链接、清理失效链接都会改变源路径形态或 C 盘占用，不能只更新徽章：

1. 操作成功落盘后，后端立即从 `AppState` 移除当前 `TreeStore`，生成 `dayu://tree-invalidated` 事件；旧 `scan_id` 的展开请求随后统一返回 `StaleScan`。
2. scan store 在应用启动时全局注册该事件，而不是只在 `ScanView` 挂载时监听。收到事件后同步清空 roots、filteredRootCount、rootFileSummary、recommended、所有展开页和高亮状态，不显示旧体积；即使事件投递失败，下一次读取也会因 `StaleScan` 收敛到同一状态。
3. 上次扫描源为 MFT 且当前进程有卷读取权限时，前端自动调用 `scan_drive(Mft)`；上次为 filesystem 降级时展示“结果已失效”状态，由用户决定是否执行耗时重扫。
4. 失败或已回滚的迁移不使树失效；处于 pending/manual-confirm 的操作按实际源路径是否已变化决定，并必须有对应测试。

## 7. IPC 命令与事件（改造 `commands.rs`）

| 命令 | 替代/状态 | 行为 |
|------|----------|------|
| `scan_drive(mode: ScanMode)` | 替代 `scan_drives()` | `Auto` 先试 MFT，权限不足返回 `NeedsElevation`，其他不可用原因返回 `FastScanUnavailable`；`Mft` 只走 MFT；`Filesystem` 只走降级。成功返回 `Complete(ScanSnapshot)`。 |
| `restart_elevated()` | 新增 | 用 `runas --elevated-scan` 启动新实例；仅当 `ShellExecuteW` 成功后退出当前实例，UAC 取消或启动失败时保留当前实例。 |
| `take_startup_scan_intent()` | 新增 | 提权实例前端挂载并注册事件监听后调用；一次性返回是否应自动执行 `scan_drive(Mft)`，避免后端启动事件早于前端监听。 |
| `expand_node(scan_id, path, offset, limit)` | 新增 | 返回直接子目录的 `ChildPage`，后端分页并按 size 倒排；scan_id 不匹配返回 `StaleScan`。 |
| `reveal_node(scan_id, path, limit)` | 新增 | 返回推荐节点的祖先链及各层定位页，支持可靠展开和滚动定位。 |
| `list_recommended(scan_id)` | 新增 | 返回当前可发起迁移的预设目录；scan_id 不匹配返回 `StaleScan`。 |
| `cancel_scan()` | 保留 | 设 `AtomicBool`；MFT 和 filesystem worker 在每次记录/目录循环检查。 |

返回类型：

```rust
pub enum ScanMode { Auto, Mft, Filesystem }

pub enum ScanDriveResult {
    NeedsElevation,
    FastScanUnavailable { reason: FastScanFailure },
    Complete(ScanSnapshot),
}

pub enum FastScanFailure {
    UnsupportedFilesystem { actual: String },
    UnsupportedNtfsVersion { major: u16, minor: u16 },
    InvalidVolumeData,
    RootRecordMissing,
    ExcessiveRecordErrors { skipped: u64, scanned: u64 },
    Io { code: Option<i32> },
}

pub struct ScanSnapshot {
    pub scan_id: String,
    pub source: ScanSource,
    pub roots: Vec<TreeNode>,
    pub filtered_root_count: u32,
    pub root_file_summary: RootFileSummary,
    pub diagnostics: ScanDiagnostics,
}

pub struct ScanDiagnostics {
    pub scanned_records: u64,
    pub scanned_dirs: u64,
    pub scanned_files: u64,
    pub skipped_records: u64,
    pub orphan_entries: u64,
    pub hard_link_entries: u64,
}
```

**事件**：`dayu://scan-progress` 字段改为 `{ scanned_records, scanned_dirs, scanned_files, estimated_record_slots, current_phase: "reading_mft" | "aggregating" | "annotating" | "walking_fs" }`。不适用的计数为 0；`estimated_record_slots` 是槽位估算，不伪装成精确在用记录总数。新增 `dayu://tree-invalidated`，字段为 `{ reason, auto_rescan }`。提权需求使用 `scan_drive` 的结构化返回值，不再用无法响应的单向事件。

**取消/互斥**：保留现有 `scan_slot` 互斥锁（同一时刻一个扫描任务）。`scan_drive(Auto)` 返回需要用户选择的结果前必须清理 cancel token 并释放锁；用户选择发生在另一次 IPC 调用中。`expand_node` / `reveal_node` / `list_recommended` 只读已发布快照，不占扫描锁。

## 8. 前端：树形视图（改造 `ScanView.vue` + `stores/scan.ts`）

### 8.1 store 改造（`stores/scan.ts`）

```ts
const roots = ref<TreeNode[]>([])            // 一级节点
const scanId = ref<string | null>(null)
const filteredRootCount = ref(0)
const rootFileSummary = ref<RootFileSummary | null>(null)
const expanded = ref<Map<string, ChildPage[]>>(new Map()) // path -> 已加载页
const expandedKeys = ref<Set<string>>(new Set())
const recommended = ref<TreeNode[]>([])      // 当前可发起迁移的预设目录

async function scan(mode: ScanMode = 'auto') {
  const result = await ipc.scanDrive(mode)
  if (result.kind === 'needs_elevation') {
    const accepted = await confirmElevation()
    if (accepted) await ipc.restartElevated()
    else await scan('filesystem')
    return
  }
  if (result.kind === 'fast_scan_unavailable') {
    const accepted = await confirmFilesystemFallback(result.reason)
    if (accepted) await scan('filesystem')
    else showFastScanFailure(result.reason)
    return
  }

  expanded.value.clear()
  expandedKeys.value.clear()
  scanId.value = result.snapshot.scanId
  roots.value = result.snapshot.roots
  filteredRootCount.value = result.snapshot.filteredRootCount
  rootFileSummary.value = result.snapshot.rootFileSummary
  recommended.value = await ipc.listRecommended(scanId.value)
}

async function toggle(path: string) {
  if (!scanId.value) return
  if (expandedKeys.value.has(path)) {
    expandedKeys.value.delete(path)
  } else {
    if (!expanded.value.has(path)) {
      const first = await ipc.expandNode(scanId.value, path, 0, 200)
      expanded.value.set(path, [first])
    }
    expandedKeys.value.add(path)
  }
}
```

“显示更多”读取 `nextOffset` 对应的下一页并追加到该 path 的页缓存。推荐节点点击调用 `revealNode(scanId, path, 200)`，把返回的每层定位页写入缓存后展开祖先链。任何 `StaleScan` 都走统一处理：清空 `scanId`、roots、filteredRootCount、rootFileSummary、recommended、expanded 和 expandedKeys，并显示结果已失效状态。

### 8.2 视图改造（`ScanView.vue`）

- 顶部新增**"推荐迁移"快捷区**：横向徽章列出命中预设节点（如"微信文件 32.4 GB"），点击后用 `reveal_node` 返回的定位页展开父链、滚动定位并高亮。
- 结果工具栏展示不可操作的“C 盘根目录文件”汇总，包括直接文件大小；MFT 模式另计 NTFS 系统元数据，降级模式标注汇总不完整。
- `filtered_root_count>0` 时显示“另有 N 个一级目录低于当前阈值”；若 roots 为空，仍保留根文件汇总并显示“没有目录达到当前阈值”，而不是显示成扫描失败。
- 主区域改为**缩进树表**：每行 = 展开箭头（`child_count>0` 才显示）+ 名称 + 路径 + 大小 + 类别 + 状态徽章 + 迁移按钮。展开箭头点击触发 `toggle`。
- `filtered_child_count>0` 时在目录计数旁显示“另有 N 个较小目录”，但不提供绕过阈值的临时展开；用户调低 `minSizeMB` 后重扫，保证同一快照的过滤语义稳定。
- 现有 `migrate(item)` 跳转逻辑**完全保留**（`TreeNode` 也有 `path` 和 `matchedPreset`）。
- 每层默认分页 200 条；分页由后端完成，前端“显示更多”才拉下一页，避免单层目录过多时产生大 IPC payload。

### 8.3 IPC types（`ipc/types.ts`）

新增 `TreeNode`、`RootFileSummary`、`ChildPage`、`ScanSnapshot`、`ScanDriveResult`，修改 `ScanProgressEvent` 字段，并新增 `list_recommended`、`expand_node`、`reveal_node`、`restart_elevated`、`take_startup_scan_intent` 的 invoke 封装。Rust 枚举和 TypeScript discriminated union 都使用固定的 serde tag，并用合约测试锁定 JSON 形状。

## 9. 权限与降级（新增 `win32.rs` 封装 + `commands.rs` 编排）

### 9.1 提权与快速失败流程

1. `scan_drive(Auto)` 取得扫描锁后尝试 `open_volume("C")`。成功则走 MFT。
2. `VolumeError::AccessDenied` 时清理 cancel token、释放扫描锁并返回 `ScanDriveResult::NeedsElevation`；命令不等待前端响应，也不 emit 单向提权事件。
3. 前端弹原生确认框。用户同意则调用 `restart_elevated()`；用户拒绝则发起新的 `scan_drive(Filesystem)`。
4. `restart_elevated()` 用 `ShellExecuteW(..., "runas", 当前exe, "--elevated-scan")`。只有返回值确认新进程成功创建时才退出旧实例；UAC 取消或启动失败返回错误，旧实例继续可用并提供降级选项。
5. 新实例前端完成挂载和事件注册后调用 `take_startup_scan_intent()`；返回 true 时再调用 `scan_drive(Mft)`，避免启动阶段事件丢失。
6. 非 NTFS、版本不支持、卷结构无效、根记录缺失、坏记录过多或不可恢复 I/O 错误返回 `FastScanUnavailable`。前端用面向用户的原因说明询问是否改用普通扫描；用户确认后另行调用 `scan_drive(Filesystem)`，拒绝则保留旧结果或空状态。

快速扫描绝不静默切换到 filesystem，否则用户无法理解耗时为何突然从秒级变成数分钟。`NeedsElevation` 和 `FastScanUnavailable` 都是正常的结构化分支，不写成笼统异常；filesystem 自身失败才进入扫描错误状态。

### 9.2 降级实现要点

`build_tree_from_fs` 产出**同样的 `TreeStore`**，节点仍然只包含目录，`child_count` 仍然只统计直接可见子目录。它必须沿用现有“不跟随 reparse point、排除路径整棵跳过、AccessDenied 记录为节点但不终止”的语义，并在完整聚合后应用相同的 `minSizeMB` 可见性规则。

filesystem 模式统计可读取的 `C:\` 直接普通文件；由于无法可靠取得 NTFS 保留元文件大小，`RootFileSummary.system_metadata_size_bytes=None` 且 `incomplete=true`，不得填 0 冒充完整结果。

并发采用有界任务队列，默认 worker 数为 `min(available_parallelism, 4)`，配置上限不超过 8；机械硬盘和网络/可移动卷不得盲目并发。每个 worker 共用 cancel token，目录调度和后序聚合分离，不能同时对同一父节点做无保护累加。

### 9.3 提权重启机制（整体提权，无跨进程中转）

采用**整体提权重启**，不引入跨进程 TreeStore 中转的复杂度：

- 主实例收到 `NeedsElevation` 后再询问用户，扫描命令此时已经结束且没有占用锁。
- 用户同意后启动带 `--elevated-scan` 的新实例；仅在启动成功后退出当前实例。
- 提权实例由前端通过 `take_startup_scan_intent()` 拉取一次性启动意图，再触发 MFT 扫描并把结果直接填入自身 `TreeStore`。
- 用户拒绝或 UAC 取消时不重启，显式调用 filesystem 降级路径。

**取舍**：整体重启会丢失当前窗口 UI 状态，并使后续命令运行在高完整性进程中。首版接受这一安全取舍，但提权实例必须禁止远程页面导航、禁止从 WebView 传入任意可执行文件/命令行，并继续执行现有迁移安全校验。若后续引入远程内容或插件，必须在此之前改成最小权限 helper + 有认证的本地 IPC，不能继续整体提权。

## 10. 测试策略

| 层 | 测试 | 风格 |
|----|------|------|
| MFT 枚举 | mock `DeviceIoControl` 输出：请求空洞时返回较小记录，断言按返回号递减、无重复、0 不下溢、取消生效；根 5 缺失和坏记录超阈值拒绝发布 | `mft.rs` 单测 |
| `mft.rs` 解析 | 真实小 NTFS fixture 必须覆盖根记录 5、USA fixup、namespace 1/2/3、父 sequence、resident/non-resident DATA、多长名/硬链接、ATTRIBUTE_LIST、reparse tag、损坏长度 | fixture + 边界单测；手工字节只能补充，不能替代真实 fixture |
| 解析健壮性 | 任意截断/损坏记录不得 panic 或越界；属性长度为 0 必须终止并报错 | `cargo-fuzz` 或 `proptest` |
| 聚合 + 标注 | 给定 `MftIndex`，断言只产出目录节点、根 5 保留、陈旧 sequence 成为 orphan、硬链接按入口归属、排除子树不计入祖先、链接目标大小不污染 C 盘大小 | `scanner.rs` 单测 |
| 阈值可见性 | 大于等于 `minSizeMB` 可见；普通小目录隐藏；小预设/状态/无法访问节点及祖先保留；排除优先；`minSizeMB=0` 展示全部目录；child_count/filtered_child_count/filtered_root_count 分别正确 | `scanner.rs` 单测 |
| 根文件汇总 | MFT 模式分别统计直接文件和系统元数据；filesystem 模式 `incomplete=true`；汇总永远不可迁移且不受 minSizeMB 过滤 | 后端模型 + 前端组件测试 |
| 推荐列表 | 只返回无状态、非 reparse、非 Inaccessible 的预设目录；低于阈值仍推荐；已迁移/待处理/异常链接不推荐 | scanner + commands 单测 |
| IPC | 分页边界、`limit` 上限、`reveal_node` 跨页定位、旧 `scan_id` 返回 `StaleScan`、新快照原子替换、各种 `FastScanFailure` JSON 合约 | commands 合约测试 |
| 前端 | 树形展开/追加页、根文件汇总、阈值后的空结果、推荐定位、`NeedsElevation` 与 `FastScanUnavailable` 的接受/拒绝、UAC 失败、`StaleScan` 和 tree-invalidated 清缓存 | 扩展 `ScanView.test.ts` |
| 降级 | `build_tree_from_fs` 与 MFT 产出结构一致（接口契约测试） | 新增 |
| 提权流程 | `NeedsElevation` 返回前释放锁、UAC 成功才退出、取消后仍可降级、启动意图只消费一次 | 新增 |
| 失效流程 | 迁移/还原/断链成功清除快照，失败不清；MFT 自动重扫，filesystem 不展示旧树 | 新增 |
| 性能门槛 | 真实 NTFS 卷 10 万/100 万/500 万在用记录，记录耗时、吞吐、峰值内存和跳过数 | 实现前 spike，非 CI |

## 11. 与现有代码的衔接清单

- **保留**：`dir_size`（降级用）、`annotate_migrations`（改树形）、`matches_preset`/`expand_env`/`normalize`/`is_descendant`、`ScanContext`、`ScanItemStatus`、`scan_slot` 互斥、`dayu://scan-progress` 事件名。
- **改造**：`scan_drives` → 带 `ScanMode`/结构化结果的 `scan_drive`；`ScanView.vue` 扁平表 → 阈值过滤树表 + 根文件汇总；`stores/scan.ts` 扁平 → 带 scan_id 的分页树；`models.rs` 新增目录专用 `TreeNode` 和 `RootFileSummary`；`ipc/types.ts` 新增快照、分页与错误类型。
- **新增**：`mft.rs`（MFT 枚举/解析）、`win32.rs` 的 MFT/UAC/卷句柄封装、不可变 `TreeStore` 快照、`restart_elevated`/`take_startup_scan_intent`/`expand_node`/`reveal_node`/`list_recommended`、有界并发 `build_tree_from_fs`、`--elevated-scan` 启动意图、tree-invalidated 事件。
- **废弃**：`scan_root`（后序遍历，被 MFT/降级聚合取代）、`ScanItem` 作为扫描结果（被 `TreeNode` 取代）、`push_if_big_or_preset`（由 `DirectoryGraph` 的树形可见性过滤取代，`minSizeMB` 继续生效）、前端对扁平全量结果做 `pageSize=200` 截断的机制（改为后端按层级分页）。

## 12. 实现顺序建议（供 writing-plans 参考）

1. 独立 MFT spike：正确枚举、解析最小字段、跑真实性能门槛；形成 go/no-go 结论。
2. 选定的 MFT 路线 + 真实 fixture、边界/健壮性测试。
3. `MftIndex` + 完整 `DirectoryGraph` 聚合/标注 + `minSizeMB` 可见性规则 + 根文件汇总单测。
4. 仅含可见目录的 `TreeNode` / 不可变 `TreeStore` + 推荐资格计算。
5. `commands.rs`：结构化 `scan_drive`、scan_id、分页、reveal、快照原子替换。
6. 提权闭环与快速失败降级：`restart_elevated`、启动意图、UAC/快速引擎失败路径。
7. 有界并发 `build_tree_from_fs` 降级和契约测试。
8. 文件系统操作后的 tree-invalidated 与自动/手动重扫策略。
9. 前端 IPC 类型、分页树 store、根文件汇总、推荐定位和失效处理。
10. `ScanView.vue` 树表 + 推荐快捷区及前端测试。
11. e2e：MFT / 提权 / 降级 / 阈值过滤 / 迁移后失效与树形交互。
