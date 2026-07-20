# WizTree 式按需展开扫描 — 设计规格

> 日期：2026-07-20
> 状态：已批准，待实现计划
> 主题：把 dayu-disk-manager 的扫描从"全盘递归 + 扁平阈值过滤"改造为"MFT 秒级建树 + 树形按需展开"

## 0. 决策汇总

本设计基于头脑风暴阶段已确认的 8 项关键决策：

| # | 决策点 | 选定 |
|---|--------|------|
| 1 | 扫描引擎 | MFT 顺序直读（解析 `$FILE_NAME` 建树、`$DATA` 算体积）|
| 2 | 权限策略 | 弹 UAC 请求提权；用户拒绝则退回多线程 std::fs 遍历 |
| 3 | 扫描根 | `C:\` 全部一级目录，按子树总大小倒排 |
| 4 | 预设场景呈现 | 徽章（节点上标注）+ 顶部"推荐迁移"快捷区（后端 `list_preset_hits()` 一次性返回命中节点）|
| 5 | 迁移状态标注 | 全树递归标注（祖先链传播"包含已迁移/包含软链接"）|
| 6 | 数据传输模型 | 树常驻后端内存 + 前端按需拉子节点 |
| 7 | 失效策略 | 手动重扫刷新；迁移成功后仅局部更新节点徽章，不重走 MFT |
| 8 | "拿大小"路线 | `FSCTL_GET_NTFS_FILE_RECORD` 顺序遍历 MFT（USN 无大小字段，已排除 USN 单独建树方案）|

## 1. 背景与问题

当前扫描实现（`src-tauri/src/scanner.rs` 的 `scan_root`）是**后序深度遍历整棵目录树**，边遍历边累加父目录大小，再用 `push_if_big_or_preset` 把"size ≥ min_size **或** 命中预设"的目录全部摊平返回。

由此产生两个问题：

- **粒度太细**：父目录和它的子目录同时进结果列表（只要各自都超阈值），用户看到一个扁平的大列表，里面混着 `C:\Users`、`C:\Users\xxx\AppData`、`C:\Users\xxx\AppData\Local\npm-cache` 等不同层级的目录。
- **效率不高**：部分因粒度细导致 IPC 传输和前端渲染了海量深层节点；底层遍历仍需走遍所有文件。

参考 WizTree 的做法：先扫 `C:\` 一级目录、按大小倒排，用户对感兴趣的大目录逐层展开下级，直到定位到适合软链接的目录再迁移。

## 2. 目标与非目标

**目标：**

- 首次扫描达到秒级（MFT 直读），消除"等几分钟"的体验。
- 结果以树形按需展开呈现，粒度由用户的展开行为决定，默认只看一级。
- 保留"智能识别预设场景 + 一键迁移"核心价值。
- 保留迁移状态标注（已迁移/包含已迁移/链接异常等）。

**非目标（YAGNI）：**

- USN Journal 增量追踪文件变化（手动重扫足够）。
- 多卷扫描（只针对 C 盘）。
- 单文件链接（README 已明确排除）。
- 扫描结果磁盘缓存（内存树已足够，重扫即重建）。
- 压缩/稀疏文件大小的完全精确（取逻辑大小用于排序足够）。

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
│  │                list_preset_hits / cancel_scan  │
│  └─ win32        MFT/UAC/卷句柄 封装（扩展）       │
└─────────────────────────────────────────────────┘
```

**各单元职责与接口：**

| 单元 | 职责 | 对外接口 | 依赖 |
|------|------|---------|------|
| `mft` | 解析 MFT 记录为 `MftRecord` 流 | `scan_volume(letter) -> Result<Vec<MftRecord>, MftError>` | `win32` |
| `scanner` | 把 `MftRecord` 聚合成 `TreeStore`；预设命中；全树递归状态标注 | `build_tree(records, cfg) -> TreeStore`、`annotate_tree(tree, migrations, ...)`、`build_tree_from_fs(cfg) -> TreeStore`（降级） | `store`（读配置/迁移记录）、`junction` |
| `TreeStore` | 内存树：节点表 + 父子索引 + roots | `roots()`、`children_of(path)`、`node(path)`、`preset_hits()` | 无 |
| `commands` | IPC 编排：触发扫描、提权、按需拉子 | `scan_drive`、`expand_node`、`list_preset_hits`、`cancel_scan` | `mft`、`scanner`、`TreeStore`、`win32` |
| `win32` | 卷句柄、MFT DeviceIoControl、UAC 提权 | `open_volume(letter) -> Result<Handle, VolumeError>`、`read_mft_record(handle, ref) -> Vec<u8>`、`request_elevation()` | 无（平台边界）|

## 4. 后端：MFT 扫描引擎（新增 `src-tauri/src/mft.rs`）

整个方案的技术重心与最高风险部分。

### 4.1 数据结构

```rust
pub struct MftRecord {
    pub file_ref: u64,           // MFT 记录的 File Reference Number
    pub parent_ref: u64,         // $FILE_NAME 里的父目录引用号（建父子树关键）
    pub name: String,            // 长文件名（取 namespace=Win32/POSIX，跳过 8.3 短名）
    pub size: u64,               // $DATA 属性的逻辑大小
    pub is_dir: bool,            // $STANDARD_INFORMATION 属性标志
    pub is_reparse: bool,        // reparse point 标志
    pub is_system_meta: bool,    // $MFT/$LogFile/$Bitmap 等前 16 条系统元文件
}

pub enum MftError {
    NeedElevation,               // CreateFileW ERROR_ACCESS_DENIED
    UnsupportedNtfsVersion(u32), // 非 NTFS 3.1
    Io(std::io::Error),
    BadRecord { ref_no: u64 },
}
```

### 4.2 扫描流程

1. `CreateFileW("\\.\C:", GENERIC_READ, FILE_SHARE_READ|FILE_SHARE_WRITE, OPEN_EXISTING)` 拿卷句柄。`ERROR_ACCESS_DENIED` → 返回 `NeedElevation`，上层触发 UAC。
2. `FSCTL_GET_NTFS_VOLUME_DATA` 拿 `BytesPerFileRecordSegment`、`MftValidDataLength`、`NumberOfFileRecords`、`BytesPerCluster`、`NtfsVersion`。`NtfsVersion != 3.1` → `UnsupportedNtfsVersion`。
3. 循环 `FSCTL_GET_NTFS_FILE_RECORD`（file reference number 0..`NumberOfFileRecords`-1）逐条读记录。
4. 解析每条记录的属性流：
   - `$STANDARD_INFORMATION` → 属性标志（reparse point 检测）
   - `$FILE_NAME` → 文件名 + 父目录引用号
   - `$DATA` → 逻辑大小（读属性头的 size，**不跟随 Data Run**）
5. 跳过 `is_system_meta`（file_ref 0..15 的系统元文件）。
6. 每 4096 条检查一次 `cancel` AtomicBool，emit 进度。
7. 输出 `Vec<MftRecord>` 给 `scanner::build_tree`。

### 4.3 已知风险与对策（规格明确写明）

- **`$DATA` 非驻留属性**：大文件 `$DATA` 用 Data Run 而非内联，但逻辑大小写在属性头里，不需跟随 Data Run。✅ 安全。
- **多 `$FILE_NAME` 属性**：一条记录可能有 8.3 短名 + 长名。取 namespace 为 Win32 或 POSIX 的长名。
- **硬链接重复计数**：同一文件多个 `$FILE_NAME` 会被多次计入，导致体积偏大。首版接受此近似（用户数据目录硬链接少见），不额外去重。
- **NTFS 版本**：首版只支持 NTFS 3.1（`FSCTL_GET_NTFS_VOLUME_DATA` 的 `NtfsVersion` 校验）。
- **解析正确性**：MFT 二进制偏移易错，必须用真实小 NTFS 镜像做 fixture 测试（见第 9 节）。

### 4.4 聚合（放 `scanner.rs`，复用现有工具）

- `HashMap<u64 file_ref, Node>` 建索引；file_ref 5 是根（`C:\`，NTFS 里根的 file_ref=5）。
- 两遍后序累加：先建节点，再从叶子向根 `saturating_add` 体积、文件数、目录数。
- 展开路径：根 path = `C:\`，子节点 path = `parent.path + "\" + name`。
- 预设命中/排除沿用 `ScanContext::preset_index` / `excluded`（已规范化、已展开环境变量）。
- 聚合产出 `TreeStore`（结构见第 5 节）。

## 5. 内存树数据结构（新增 `TreeNode` + `TreeStore`）

现有 `ScanItem` 是扁平无父子概念。新增 `TreeNode`，**扁平存储 + 父子索引**（便于按 path 拉子）。

`src-tauri/src/models.rs`：

```rust
pub struct TreeNode {
    pub path: String,              // 完整绝对路径，唯一 key
    pub display_name: String,      // 末级名（预设命中时显示预设名）
    pub size_bytes: u64,           // 子树总大小（MFT 聚合）
    pub file_count: u64,           // 子树文件数
    pub dir_count: u64,
    pub depth: u32,                // 相对 C:\ 深度（C:\=0，一级=1）
    pub is_junction: bool,         // MFT reparse 检测
    pub inaccessible: bool,
    pub matched_preset: Option<String>,
    pub category: Option<PresetCategory>,
    pub auto_migrate: bool,
    pub scan_status: Option<ScanItemStatus>,   // 全树递归标注后填入
    pub migration_id: Option<String>,
    pub child_count: u32,          // 直接子节点数；>0 时前端显示展开箭头
}
```

`src-tauri/src/app_state.rs` 新增 `TreeStore`（`Mutex<Option<Arc<TreeStore>>>`）：

```rust
pub struct TreeStore {
    nodes: HashMap<String, TreeNode>,                       // path -> node
    children: HashMap<String, Vec<String>>,                  // parent path -> child paths（按 size 倒排）
    roots: Vec<String>,                                      // C:\ 一级子目录 path（按 size 倒排）
    preset_hits: Vec<String>,                                // 命中预设的节点 path
}

impl TreeStore {
    pub fn roots(&self) -> &[TreeNode];
    pub fn children_of(&self, path: &str) -> Vec<TreeNode>;  // 返回直接子节点（已倒排）
    pub fn node(&self, path: &str) -> Option<&TreeNode>;
    pub fn preset_hits(&self) -> Vec<TreeNode>;
}
```

**ScanItem 的去留**：`TreeNode` 取代 `ScanItem` 作为扫描结果载体。`ScanItemStatus` 枚举保留不动（语义通用）。现有 `annotate_migrations`、`junction_item`、`inaccessible_item` 改造为产出 `TreeNode`。

## 6. 迁移状态全树递归标注（改造 `annotate_migrations`）

现有 `annotate_migrations` 只给扁平列表打标。改为在 `TreeStore` 上做两阶段传播：

1. **精确命中**：节点 path == 某 active migration.source → 标 `Migrated`（或 `LinkBroken`，复用现有 `junction::verify`）；size 用 `dir_size(target)`。
2. **向上传播**：对每个命中节点，沿 parent 链向上把祖先标 `ContainsMigrated`（已有该状态的节点跳过，避免覆盖更具体的 `Migrated`）。
3. **junction 子树**：同理，junction 路径的祖先标 `ContainsLink`。

复用现有 `is_descendant`、`represents_migrated_link`、`normalize` 工具函数，逻辑结构不变，只是从"遍历扁平 items"改成"在树上 DFS 传播"。

## 7. IPC 命令与事件（改造 `commands.rs`）

| 命令 | 替代/状态 | 行为 |
|------|----------|------|
| `scan_drive()` | 替代 `scan_drives()` | 触发 MFT 扫描；`NeedElevation` 则 emit 提权请求事件，前端确认后 `runas` 重启提权实例；用户拒绝则退 `build_tree_from_fs`。返回 `Vec<TreeNode>`（仅 C:\ 一级 roots）。扫描完树常驻 `TreeStore`。 |
| `expand_node(path: String)` | 新增 | 从 `TreeStore` 取该节点**直接子节点**返回（已按 size 倒排）。不返回孙节点。只读 `TreeStore`，不受扫描锁限制。 |
| `list_preset_hits()` | 新增 | 返回命中预设的所有节点（后端建树时已算好，O(1) 返回），供前端"推荐迁移"快捷区。 |
| `cancel_scan()` | 保留 | 设 `AtomicBool`，MFT 循环每 4096 条检查一次。 |

**事件**：`dayu://scan-progress` 字段改为 `{ scanned_records, total_records, current_phase: "elevating" \| "reading_mft" \| "aggregating" \| "annotating" }`。MFT 是秒级，进度主要用于提权等待期反馈。新增 `dayu://scan-needs-elevation` 事件触发前端 UAC 确认框。

**取消/互斥**：保留现有 `scan_slot` 互斥锁（同一时刻一个扫描任务）。`expand_node` / `list_preset_hits` 只读 `TreeStore`，不受锁限制。

## 8. 前端：树形视图（改造 `ScanView.vue` + `stores/scan.ts`）

### 8.1 store 改造（`stores/scan.ts`）

```ts
const roots = ref<TreeNode[]>([])            // 一级节点
const expanded = ref<Map<string, TreeNode[]>>(new Map())  // path -> children 缓存
const expandedKeys = ref<Set<string>>(new Set())
const recommended = ref<TreeNode[]>([])      // 命中预设节点（来自 list_preset_hits）

async function scan() {                       // scanDrive 只返回一级 roots
  roots.value = await ipc.scanDrive()
  recommended.value = await ipc.listPresetHits()   // 命中预设节点独立拉取
  expanded.value.clear()
  expandedKeys.value.clear()
}
async function toggle(path: string) {        // 展开时缓存未命中则 await ipc.expandNode(path)
  if (expandedKeys.value.has(path)) {
    expandedKeys.value.delete(path)           // 折叠：仅改集合，不清缓存（便于再次展开秒开）
  } else {
    if (!expanded.value.has(path)) {
      expanded.value.set(path, await ipc.expandNode(path))
    }
    expandedKeys.value.add(path)              // 展开
  }
}
```

### 8.2 视图改造（`ScanView.vue`）

- 顶部新增**"推荐迁移"快捷区**：横向徽章列出命中预设节点（如"微信文件 32.4 GB"），点击 → 展开其父链 + 滚动定位 + 高亮。数据来自 `recommended`（后端 `list_preset_hits`）。
- 主区域改为**缩进树表**：每行 = 展开箭头（`child_count>0` 才显示）+ 名称 + 路径 + 大小 + 类别 + 状态徽章 + 迁移按钮。展开箭头点击触发 `toggle`。
- 现有 `migrate(item)` 跳转逻辑**完全保留**（`TreeNode` 也有 `path` 和 `matchedPreset`）。
- 分页 `pageSize=200` 机制保留（每个展开层级的子列表分页），避免单层子节点过多。

### 8.3 IPC types（`ipc/types.ts`）

新增 `TreeNode`、`ScanProgressEvent` 改字段、新增 `list_preset_hits`/`expand_node` 的 invoke 封装。

## 9. 权限与降级（新增 `win32.rs` 封装 + `commands.rs` 编排）

### 9.1 提权流程

1. `scan_drive` 先试 `open_volume("C")`。成功 → 走 MFT。
2. `VolumeError::AccessDenied` → emit `dayu://scan-needs-elevation`，前端弹原生确认框"需要管理员权限以加速扫描，是否提权？"。
3. 用户同意 → 后端 `request_elevation()` = `ShellExecuteW(..., "runas", 当前exe, "--elevated-scan")` 重启提权实例；提权实例自动触发扫描，非提权实例退出。
4. 用户拒绝 → 退回 `build_tree_from_fs()`：**多线程** `std::fs` 遍历（`rayon` 或手写线程池）+ 内存聚合，复用现有 `dir_size` 语义。前端提示"未提权，扫描较慢"。

### 9.2 降级实现要点

`build_tree_from_fs` 产出**同样的 `TreeStore`**，前端对两种数据源无感知。这是保证树形交互在任何权限下都可用的关键。降级路径只影响首次扫描速度，不影响交互模型。

### 9.3 提权重启机制（整体提权，无跨进程中转）

采用**整体提权重启**，不引入跨进程 TreeStore 中转的复杂度：

- 主实例检测到 `AccessDenied` 后，emit `dayu://scan-needs-elevation`；前端弹原生确认框。
- 用户同意 → 后端 `request_elevation()` = `ShellExecuteW(NULL, "runas", 当前exe路径, "--elevated-scan")`。**当前（非提权）实例随即退出**，新启动的提权实例以 `--elevated-scan` 参数运行。
- 提权实例启动后：识别到 `--elevated-scan` 参数 → 自动发起 MFT 扫描 → 结果直接填入自身进程内的 `TreeStore` → 前端展示。**无需 snapshot 文件中转**（同一进程内直接持有）。
- 用户拒绝 → 不重启，退回 `build_tree_from_fs()` 多线程遍历。

**取舍**：整体重启会丢失当前窗口的 UI 状态（展开/滚动位置等）。但扫描是入口动作，用户点"开始扫描"时通常还没产生需要保留的状态，且新实例会自动跑完扫描直接呈现树，体验可接受。换取的是**实现简单、无跨进程 IPC、无 snapshot 一致性问题**。后续若需保留状态，可在重启前把 UI 状态写入临时文件再恢复（列为未来工作，非首版范围）。

## 10. 测试策略

| 层 | 测试 | 风格 |
|----|------|------|
| `mft.rs` 解析 | 小 NTFS 镜像 fixture（或手工构造的 MFT 字节序列）断言解析出正确的树/大小/父子关系 | 参考 `dir_size_skips_reparse_point_content` 风格 |
| 聚合 + 标注 | 纯函数：给定 `Vec<MftRecord>`，断言 `TreeStore` 体积累加、预设命中、状态传播正确 | 沿用 `#[test]` in `scanner.rs` |
| IPC | `expand_node` / `list_preset_hits` 只读 `TreeStore`，构造 store 后断言返回正确子集 | 沿用现有 e2e 风格 |
| 前端 | 树形展开、徽章渲染、快捷区点击定位 | 扩展现有 `ScanView.test.ts` |
| 降级 | `build_tree_from_fs` 与 MFT 产出结构一致（接口契约测试） | 新增 |
| 提权流程 | `--elevated-scan` 参数识别、整体重启后自动扫描、TreeStore 直接填充 | 新增 |

## 11. 与现有代码的衔接清单

- **保留**：`dir_size`（降级用）、`annotate_migrations`（改树形）、`matches_preset`/`expand_env`/`normalize`/`is_descendant`、`ScanContext`、`ScanItemStatus`、`scan_slot` 互斥、`dayu://scan-progress` 事件名。
- **改造**：`scan_drives` → `scan_drive`；`ScanView.vue` 扁平表 → 树表；`stores/scan.ts` 扁平 → 树形 + 按需拉子；`models.rs` 新增 `TreeNode`；`ipc/types.ts` 新增 `TreeNode` 与新命令类型。
- **新增**：`mft.rs`（MFT 解析）、`win32.rs` 的 MFT/UAC/卷句柄封装、`TreeStore`（`app_state.rs`）、`scan_drive`/`expand_node`/`list_preset_hits` 命令、`build_tree_from_fs`（多线程降级）、提权实例启动参数识别（`--elevated-scan`）。
- **废弃**：`scan_root`（后序遍历，被 MFT/降级聚合取代）、`ScanItem` 作为扫描结果（被 `TreeNode` 取代）、`push_if_big_or_preset`（扁平阈值过滤不再需要）、`pageSize=200` 的扁平全量分页（改为按层级分页）。

## 12. 实现顺序建议（供 writing-plans 参考）

1. `TreeNode` + `TreeStore` 数据结构 + 单测。
2. `mft.rs` 解析 + fixture 单测（最高风险，先攻）。
3. `scanner::build_tree` 聚合 + `annotate_tree` 状态传播 + 单测。
4. `win32.rs` MFT/UAC/卷句柄封装。
5. `commands.rs`：`scan_drive` + `expand_node` + `list_preset_hits` + 提权流程 + 降级 `build_tree_from_fs`。
6. 前端 `ipc/types.ts` + `invoke.ts` 新增类型与封装。
7. `stores/scan.ts` 树形 + 按需拉子。
8. `ScanView.vue` 树表 + 推荐快捷区。
9. 前端测试扩展。
10. e2e：提权 / 降级 / 树形交互。
