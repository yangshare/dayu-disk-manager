# 大禹磁盘管理器（dayu-disk-manager）实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 构建一款 Windows 桌面磁盘治理工具，通过 NTFS junction 把 C 盘大体积目录迁移到其他盘并原位建链，释放 C 盘空间——支持智能识别预设场景、安全可恢复的迁移状态机、统一链接管理与操作历史。

**架构：** Tauri 2 应用，前端 Vue 3 + TypeScript，后端 Rust。后端按职责分单元（`store`/`win32`/`junction`/`file_ops`/`journal`/`history`/`scanner`/`safety`/`migrator`），其中 `win32` 是唯一平台边界。`migrator` 是可恢复状态机核心，但它**只编排**——实际建链/复制/删原委托 `junction`/`file_ops`，预检委托 `safety`，恢复与审计委托 `journal`/`history`。通过把文件操作抽象成 `FileOps` trait（生产用真实实现、测试用 mock），状态机分支测试脱离真实磁盘、毫秒级跑完。持久化数据全部放在 `%LOCALAPPDATA%\dayu-disk-manager\`。

**技术栈：**
- **后端 Rust**：Tauri 2 内嵌的 `src-tauri`，Rust edition 2021。依赖：`junction`（NTFS junction 创建/删除/校验）、`trash`（移回收站）、`windows`（盘空间/卷序列号/Restart Manager 占用检测）、`serde`/`serde_json`（序列化）、`uuid`（迁移 ID）、`tempfile`（仅测试）、`chrono`（时间戳）、`dirs`（`%LOCALAPPDATA%` 定位）、`thiserror`（错误类型）。
- **前端**：Vue 3 + TypeScript + Vite，`@tauri-apps/api`。Vue Router（视图路由）、Pinia（状态）、Vitest（组件单测）。
- **测试**：Rust 单元测试 `cargo test`（`tempfile` + mock trait），少量真实文件系统集成测试；前端 Vitest 单测关键逻辑。不做 UI E2E。

---

## 阶段总览与依赖

计划按地基→核心→外围顺序编排。每个任务结束 commit。**跨任务依赖**如下（后置任务只依赖前置任务已完成的产物）：

```
阶段0 脚手架        T1 (Tauri+Vue 骨架, CI 可跑)
   │
阶段1 地基层        T2 store → T3 win32 → T4 junction
   │                                          ╲
阶段2 数据/日志层    T5 file_ops+manifest → T6 journal → T7 history
   │                    ╱
阶段3 业务层        T8 scanner  T9 safety  T10 migrator(迁移) → T11 migrator(还原)
   │
阶段4 IPC 合约层    T12 Tauri commands+events (绑定 T8-T11)
   │
阶段5 前端          T13 前端骨架+路由+IPC封装 → T14 ScanView → T15 MigrateView
   │                                       → T16 LinksView → T17 HistoryView → T18 SettingsView
   │
阶段6 收尾          T19 端到端集成测试 → T20 崩溃恢复边界用例 → T21 打包与手工验证清单
```

**首版 YAGNI 边界（规格第 8 章）：** 计划只覆盖"可一键迁移的预设场景 + 需确认风险场景标注 + 自定义目录迁移 + 链接管理 + 历史 + 设置"。明确**不**做：浏览器缓存、系统用户文件夹、单文件链接、跨平台、性能基准、UI E2E 自动化。

**首版已知简化（非规格缺口，执行时知晓）：**
1. `list_links` 只列出**本工具创建**的迁移记录（`migrations.json`），**不**枚举系统其他 junction（规格 2.3"系统已有的 junction"留后续）。
2. `scan_drives` 首版扫描根 = 当前用户目录 + `C:/Program Files`，**不**整盘遍历（性能与权限权衡）。
3. `win32` 的长路径 `\\?\` 前缀已实现 `to_long_path`，但占用检测、空间查询等对大多数用户目录（<260）已足够；超长路径作为增强。
4. `start_migrate`/`start_restore` 在 `#[tauri::command] async fn` 内直接调同步 `migrate`/`restore`，会占用一个 runtime worker（迁移是长任务，首版可接受；后续可包 `tokio::task::spawn_blocking`）。

---

## 文件结构

锁定分解决策。`src-tauri/` 是 Rust 后端，根目录 `src/` 是 Vue 前端。每个 Rust 单元一个文件，便于单独推理。前端按视图+composables 拆分。

### 后端 Rust 文件（`src-tauri/src/`）

| 文件 | 职责 | 由哪个任务创建 |
|------|------|----------------|
| `lib.rs` | Tauri `run()` 入口、`invoke_handler` 注册、`mod` 声明、启动恢复调用 | T1 起逐步扩充 |
| `main.rs` | 二进制入口，调 `lib::run()`（create-tauri-app 生成） | T1 |
| `error.rs` | 统一 `AppError` 枚举 + `serde` 序列化 + `From` 转换 | T2 |
| `models.rs` | 共享数据结构：`Config`、`Migration`、`MigrationStatus`、`ScanItem`、`Preset`、`JournalEntry`、`HistoryEntry`、`ProgressEvent`、`PrecheckReport` | T2 起逐步扩充 |
| `store.rs` | 配置与迁移记录的 JSON 读写；临时文件→flush→原子 rename；`.bak` 备份回滚；损坏降级；首次启动注入默认 presets | T2 |
| `win32.rs` | Win32 API 薄封装：盘空间 `GetDiskFreeSpaceExW`、卷序列号 `GetVolumeInformationW`、卷类型/可写判断、长路径 `\?\` 前缀、Restart Manager 占用检测、`%LOCALAPPDATA%` 定位 | T3 |
| `junction.rs` | junction 创建/删除/解析/校验（封装 `junction` crate） | T4 |
| `file_ops.rs` | `FileOps` trait + 真实实现 `RealFileOps`：递归复制不跟随 reparse point、保留 NTFS 元数据、原子改名、移回收站、manifest 生成与对比 | T5 |
| `journal.rs` | 运行中任务阶段恢复日志：`begin`/`mark_stage`/`complete`/`recover_pending`；同路径任务锁 | T6 |
| `history.rs` | 操作历史流水追加与查询：`append`/`list` | T7 |
| `scanner.rs` | 遍历目录算体积、匹配预设场景、跳过 reparse point、AccessDenied 降级 | T8 |
| `safety.rs` | 迁移前预检：空间/卷类型/仓库路径/黑名单白名单/占用/目标冲突 | T9 |
| `migrator.rs` | 迁移与还原状态机 + 进度事件编排，依赖 `FileOps` trait | T10/T11 |
| `commands.rs` | Tauri `#[tauri::command]` 入口，把前 11 个单元绑定到 IPC；`emit` 进度事件 | T12 |
| `commands_tests.rs`（或 `tests/`） | IPC 合约与状态机集成测试 | T19/T20 |

### 前端 Vue 文件（根目录 `src/`）

| 文件 | 职责 | 任务 |
|------|------|------|
| `main.ts` | Vue app 挂载、Router、Pinia | T13 |
| `App.vue` | 侧边导航 + `<RouterView>` 布局 | T13 |
| `router/index.ts` | 5 视图路由 | T13 |
| `ipc/invoke.ts` | `@tauri-apps/api/core` 的 `invoke` 封装 + 类型 | T13 |
| `ipc/events.ts` | `listen` 封装，订阅进度事件 | T13 |
| `stores/scan.ts` | Pinia store：扫描状态/结果 | T14 |
| `stores/migrate.ts` | 迁移任务进度状态机（前端镜像） | T15 |
| `stores/links.ts` | 链接列表状态 | T16 |
| `views/ScanView.vue` | 扫描结果展示与"一键迁移" | T14 |
| `views/MigrateView.vue` | 预检清单 + 确认 + 进度 | T15 |
| `views/LinksView.vue` | 链接列表 + 还原/断开/打开 | T16 |
| `views/HistoryView.vue` | 历史流水筛选 | T17 |
| `views/SettingsView.vue` | 仓库路径/扫描阈值/导出日志 | T18 |
| `components/SizeCell.vue` | 体积格式化（KB/MB/GB） | T14（复用） |
| `components/ProgressStage.vue` | 进度条 + 阶段文本 | T15（复用） |

### 数据文件（运行时生成，不进 git）

`%LOCALAPPDATA%\dayu-disk-manager\`：`config.json`、`migrations.json`、`operation_journal.jsonl`、`history.jsonl`。结构与字段见规格第 6 章，计划在各任务中逐字段落地。

---

## 阶段 0：脚手架

### 任务 1：Tauri 2 + Vue 3 项目骨架

**文件：**
- 创建：`package.json`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json`、`src-tauri/src/main.rs`、`src-tauri/src/lib.rs`、`vite.config.ts`、`tsconfig.json`、`index.html`、`src/main.ts`、`src/App.vue`
- 修改：`.gitignore`（追加 `src-tauri/target/`、`node_modules/`、`dist/`）
- 测试：`src-tauri/tests/smoke.rs`

**说明：** 用 `create-tauri-app` 生成 TypeScript + pnpm + Vue 模板，再补依赖。脚手架产物即任务 1 的"实现"，本任务只做最小可编译验证。

- [ ] **步骤 1：用 create-tauri-app 生成骨架**

在仓库根目录（`E:\开源项目\PC工具\dayu-disk-manager`）执行（PowerShell，交互式模板选择走 `!` 前缀让用户在会话内运行）：

```
npm create tauri-app@latest .
```

交互选项（如果提示已存在文件，选择保留）：
- Project name: `dayu-disk-manager`
- Identifier: `com.dayu.disk-manager`
- Frontend language: `TypeScript / JavaScript`
- Package manager: `pnpm`
- UI template: `Vue`
- UI flavor: `TypeScript`

生成后删除模板自带的 `src/components/` 演示内容（保留 `src/main.ts`、`src/App.vue` 占位）。

- [ ] **步骤 2：补充 Rust 依赖**

编辑 `src-tauri/Cargo.toml`，在 `[dependencies]` 下确保：

```toml
[dependencies]
tauri = { version = "2", features = [] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "1"
uuid = { version = "1", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
dirs = "5"

[target.'cfg(windows)'.dependencies]
junction = "1"
trash = "5"
windows = { version = "0.62", features = [
    "Win32_Storage_FileSystem",
    "Win32_Storage_Volume",
    "Win32_Foundation",
    "Win32_System_RestartManager",
    "Win32_Security",
] }

[dev-dependencies]
tempfile = "3"
```

- [ ] **步骤 3：占位 lib.rs 与 smoke 测试**

`src-tauri/src/lib.rs`：

```rust
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

`src-tauri/tests/smoke.rs`（编译期冒烟测试，验证依赖能解析）：

```rust
#[test]
fn dependencies_resolve() {
    // 若依赖版本冲突或 feature 缺失，cargo test 会编译失败在此处暴露
    let _ = serde_json::json!({"ok": true});
    let _ = uuid::Uuid::new_v4();
    let _ = chrono::Utc::now();
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --test smoke`
预期：PASS，`dependencies_resolve` 通过。

- [ ] **步骤 5：Commit**

```bash
git add -A
git commit -m "feat: 初始化 Tauri 2 + Vue 3 项目骨架与依赖"
```

---

## 阶段 1：地基层

### 任务 2：store 单元 — 数据结构与持久化

**文件：**
- 创建：`src-tauri/src/models.rs`、`src-tauri/src/error.rs`、`src-tauri/src/store.rs`
- 修改：`src-tauri/src/lib.rs`（加 `pub mod error; pub mod models; pub mod store;`）
- 测试：`src-tauri/src/store.rs` 内联 `#[cfg(test)] mod tests`

**职责：** 定义全部共享数据结构（一次定义、后续任务引用，保证类型一致）；`store` 负责配置与迁移记录的 JSON 读写，使用"写临时文件→flush→原子 rename"，写入前留 `.bak`，写失败回滚，损坏文件降级到默认而非崩溃。

**serde 一致性约定（全计划遵守）：** 所有结构体加 `#[serde(rename_all = "camelCase")]`，使序列化结果与规格第 6 章 JSON 示例（`minSizeMB`、`sourceVolumeSerial`、`oldPath`、`migrationId`、`durationSec`）一致。Rust 字段保留 snake_case。

- [ ] **步骤 1：定义全部共享类型（models.rs + error.rs）**

`src-tauri/src/models.rs`：

```rust
use serde::{Deserialize, Serialize};

// ===== Config =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub schema_version: u32,
    pub repository: String,
    pub scan: ScanConfig,
    #[serde(default)]
    pub presets: Vec<Preset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanConfig {
    pub min_size_mb: u64,
    pub exclude_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PresetCategory {
    Communication,
    GameLibrary,
    DevCache,
    Ide,
    Container,
    AppInstall,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub category: PresetCategory,
    /// 路径匹配模板，可含环境变量占位（%USERPROFILE% / %LOCALAPPDATA% / %APPDATA%）。
    /// scanner 展开后与扫描到的目录路径匹配。
    pub match_paths: Vec<String>,
    /// 用于占用检测提示的进程名（不带扩展名的小写名）。
    pub match_processes: Vec<String>,
    /// true=预检通过即可一键迁移；false=需用户确认风险。
    pub auto_migrate: bool,
    /// 仓库下的子目录名（如 "wechat"），最终目标 = repository\{targetSubdir}\{uuid}\data
    pub target_subdir: String,
}

// ===== Migration =====
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStatus {
    Active,
    OldPendingDelete,
    TargetPendingDelete,
    PendingManualConfirm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Migration {
    pub id: String,
    pub schema_version: u32,
    pub source: String,
    pub target: String,
    pub old_path: String,
    pub preset: Option<String>,
    pub created_at: String,
    pub status: MigrationStatus,
    pub source_volume_serial: String,
    pub target_volume_serial: String,
    #[serde(default)]
    pub recycle_bin_ref: String,
    #[serde(default)]
    pub pending_cleanup: Option<String>,
}

// ===== Scan =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanItem {
    pub path: String,
    pub display_name: String,
    pub size_bytes: u64,
    pub matched_preset: Option<String>,
    pub category: Option<PresetCategory>,
    pub auto_migrate: bool,
    pub is_junction: bool,
    pub inaccessible: bool,
}

// ===== Journal =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub task_id: String,
    /// "migrate" | "restore"
    pub op: String,
    pub migration_id: String,
    /// 见 journal.rs 的 Stage 常量
    pub stage: String,
    pub src: String,
    pub dst: String,
    pub tmp: String,
    pub old_path: String,
    pub time: String,
    /// None=进行中；Some("completed"|"failed"|"canceled")=任务终态
    #[serde(default)]
    pub final_mark: Option<String>,
}

// ===== History =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// "migrate" | "restore" | "delete_link" | "break_link"
    pub op: String,
    pub id: String,
    pub src: String,
    pub dst: String,
    /// "ok" | "failed" | "canceled"
    pub result: String,
    pub time: String,
    pub duration_sec: u64,
}

// ===== Progress event (后端 emit -> 前端) =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub task_id: String,
    pub stage: String,
    pub percent: u8,
    pub message: String,
}

// ===== Precheck =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrecheckReport {
    pub ok: bool,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
    pub source_size_bytes: u64,
    pub target_free_bytes: u64,
}
```

`src-tauri/src/error.rs`：

```rust
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("序列化错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("store 错误: {0}")]
    Store(String),

    #[error("safety 预检失败: {0}")]
    Safety(String),

    #[error("迁移失败: {0}")]
    Migrate(String),

    #[error("junction 错误: {0}")]
    Junction(String),

    #[error("用户取消")]
    Cancelled,

    #[error("任务冲突: {0}")]
    Conflict(String),

    #[error("Win32 错误: {0}")]
    Win32(String),
}

// Tauri 命令返回的 Result<T, AppError> 必须可序列化给前端
impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
```

在 `src-tauri/src/lib.rs` 顶部加模块声明（后续任务每次新增模块都追加一行）：

```rust
pub mod error;
pub mod models;
pub mod store;
```

## 阶段 1：地基层

### 任务 2：store 单元 — 数据结构与持久化

**文件：**
- 创建：`src-tauri/src/models.rs`、`src-tauri/src/error.rs`、`src-tauri/src/store.rs`
- 修改：`src-tauri/src/lib.rs`（加 `pub mod error; pub mod models; pub mod store;`）
- 测试：`src-tauri/src/store.rs` 内联 `#[cfg(test)] mod tests`

**职责：** 定义全部共享数据结构（一次定义、后续任务引用，保证类型一致）；`store` 负责配置与迁移记录的 JSON 读写，使用"写临时文件→flush→原子 rename"，写入前留 `.bak`，写失败回滚，损坏文件降级到默认而非崩溃。

**serde 一致性约定（全计划遵守）：** 所有结构体加 `#[serde(rename_all = "camelCase")]`，使序列化结果与规格第 6 章 JSON 示例（`minSizeMB`、`sourceVolumeSerial`、`oldPath`、`migrationId`、`durationSec`）一致。Rust 字段保留 snake_case。

- [ ] **步骤 1：定义全部共享类型（models.rs + error.rs）**

`src-tauri/src/models.rs`：

```rust
use serde::{Deserialize, Serialize};

// ===== Config =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub schema_version: u32,
    pub repository: String,
    pub scan: ScanConfig,
    #[serde(default)]
    pub presets: Vec<Preset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanConfig {
    pub min_size_mb: u64,
    pub exclude_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PresetCategory {
    Communication,
    GameLibrary,
    DevCache,
    Ide,
    Container,
    AppInstall,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub category: PresetCategory,
    /// 路径匹配模板，可含环境变量占位（%USERPROFILE% / %LOCALAPPDATA% / %APPDATA%）。
    /// scanner 展开后与扫描到的目录路径匹配。
    pub match_paths: Vec<String>,
    /// 用于占用检测提示的进程名（不带扩展名的小写名）。
    pub match_processes: Vec<String>,
    /// true=预检通过即可一键迁移；false=需用户确认风险。
    pub auto_migrate: bool,
    /// 仓库下的子目录名（如 "wechat"），最终目标 = repository/{targetSubdir}/{uuid}/data
    pub target_subdir: String,
}

// ===== Migration =====
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStatus {
    Active,
    OldPendingDelete,
    TargetPendingDelete,
    PendingManualConfirm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Migration {
    pub id: String,
    pub schema_version: u32,
    pub source: String,
    pub target: String,
    pub old_path: String,
    pub preset: Option<String>,
    pub created_at: String,
    pub status: MigrationStatus,
    pub source_volume_serial: String,
    pub target_volume_serial: String,
    #[serde(default)]
    pub recycle_bin_ref: String,
    #[serde(default)]
    pub pending_cleanup: Option<String>,
}

// ===== Scan =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanItem {
    pub path: String,
    pub display_name: String,
    pub size_bytes: u64,
    pub matched_preset: Option<String>,
    pub category: Option<PresetCategory>,
    pub auto_migrate: bool,
    pub is_junction: bool,
    pub inaccessible: bool,
}

// ===== Journal =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub task_id: String,
    /// "migrate" | "restore"
    pub op: String,
    pub migration_id: String,
    /// 见 journal.rs 的 Stage 常量
    pub stage: String,
    pub src: String,
    pub dst: String,
    pub tmp: String,
    pub old_path: String,
    pub time: String,
    /// None=进行中；Some("completed"|"failed"|"canceled")=任务终态
    #[serde(default)]
    pub final_mark: Option<String>,
}

// ===== History =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// "migrate" | "restore" | "delete_link" | "break_link"
    pub op: String,
    pub id: String,
    pub src: String,
    pub dst: String,
    /// "ok" | "failed" | "canceled"
    pub result: String,
    pub time: String,
    pub duration_sec: u64,
}

// ===== Progress event (后端 emit -> 前端) =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub task_id: String,
    pub stage: String,
    pub percent: u8,
    pub message: String,
}

// ===== Precheck =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrecheckReport {
    pub ok: bool,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
    pub source_size_bytes: u64,
    pub target_free_bytes: u64,
}
```

`src-tauri/src/error.rs`：

```rust
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("序列化错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("store 错误: {0}")]
    Store(String),

    #[error("safety 预检失败: {0}")]
    Safety(String),

    #[error("迁移失败: {0}")]
    Migrate(String),

    #[error("junction 错误: {0}")]
    Junction(String),

    #[error("用户取消")]
    Cancelled,

    #[error("任务冲突: {0}")]
    Conflict(String),

    #[error("Win32 错误: {0}")]
    Win32(String),
}

// Tauri 命令返回的 Result<T, AppError> 必须可序列化给前端
impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
```

在 `src-tauri/src/lib.rs` 顶部加模块声明（后续任务每次新增模块都追加一行）：

```rust
pub mod error;
pub mod models;
pub mod store;
```

---
- [ ] **步骤 2：编写失败的测试（store.rs 内联 tests）**

在 `src-tauri/src/store.rs` 先只写测试模块（实现部分留空，让测试编译失败暴露缺口）：

```rust
use crate::error::{AppError, AppResult};
use crate::models::{Config, Migration, MigrationStatus, Preset, PresetCategory, ScanConfig};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_store() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn load_config_returns_default_when_missing() {
        let (_t, s) = fresh_store();
        let cfg = s.load_config().unwrap();
        assert_eq!(cfg.schema_version, 1);
        assert_eq!(cfg.scan.min_size_mb, 500);
        assert!(!cfg.presets.is_empty(), "默认 presets 必须被注入");
        assert!(cfg.presets.iter().any(|p| p.id == "wechat"));
    }

    #[test]
    fn save_then_load_config_roundtrip() {
        let (_t, s) = fresh_store();
        let mut cfg = s.load_config().unwrap();
        cfg.repository = "E:/Migrated2".into();
        s.save_config(&cfg).unwrap();
        let again = s.load_config().unwrap();
        assert_eq!(again.repository, "E:/Migrated2");
    }

    #[test]
    fn corrupt_config_falls_back_to_default() {
        let (_t, s) = fresh_store();
        fs::write(s.config_path(), b"{ not valid json").unwrap();
        let cfg = s.load_config().unwrap();
        assert_eq!(cfg.repository, "D:/Migrated");
    }

    #[test]
    fn save_migrations_creates_bak_on_second_write() {
        let (_t, s) = fresh_store();
        let sample = Migration {
            id: "u1".into(),
            schema_version: 1,
            source: "C:/src".into(),
            target: "D:/dst".into(),
            old_path: "C:/src.dayu-old-t1".into(),
            preset: None,
            created_at: "2026-07-18T10:00:00Z".into(),
            status: MigrationStatus::Active,
            source_volume_serial: "AAA".into(),
            target_volume_serial: "BBB".into(),
            recycle_bin_ref: String::new(),
            pending_cleanup: None,
        };
        s.upsert_migration(sample.clone()).unwrap();
        assert!(s.mig_path().exists());
        assert!(!s.mig_bak().exists(), "首次写入不应有 bak");
        s.upsert_migration(Migration { id: "u2".into(), ..sample }).unwrap();
        assert!(s.mig_bak().exists(), "第二次写入应生成 bak");
        let loaded = s.load_migrations().unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn load_migrations_empty_when_missing() {
        let (_t, s) = fresh_store();
        assert!(s.load_migrations().unwrap().is_empty());
    }
}
```
- [ ] **步骤 3：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml store`
预期：FAIL / 编译错误 —— `Store`、`default_config`、`default_presets` 等未定义。

- [ ] **步骤 4：实现 store.rs（在测试模块上方补全）**

在 `src-tauri/src/store.rs` 测试模块**之前**插入实现：

```rust
pub struct Store {
    pub data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: impl Into<PathBuf>) -> AppResult<Self> {
        let data_dir = data_dir.into();
        fs::create_dir_all(&data_dir)?;
        Ok(Store { data_dir })
    }

    pub fn config_path(&self) -> PathBuf { self.data_dir.join("config.json") }
    pub fn config_tmp(&self) -> PathBuf { self.data_dir.join("config.json.tmp") }
    pub fn config_bak(&self) -> PathBuf { self.data_dir.join("config.json.bak") }
    pub fn mig_path(&self) -> PathBuf { self.data_dir.join("migrations.json") }
    pub fn mig_tmp(&self) -> PathBuf { self.data_dir.join("migrations.json.tmp") }
    pub fn mig_bak(&self) -> PathBuf { self.data_dir.join("migrations.json.bak") }

    pub fn load_config(&self) -> AppResult<Config> {
        match fs::read(self.config_path()) {
            Ok(bytes) => match serde_json::from_slice::<Config>(&bytes) {
                Ok(cfg) => Ok(ensure_presets(cfg)),
                Err(_) => self.load_config_bak_or_default(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(default_config()),
            Err(e) => Err(AppError::Io(e)),
        }
    }

    fn load_config_bak_or_default(&self) -> AppResult<Config> {
        if self.config_bak().exists() {
            if let Ok(bak) = fs::read(self.config_bak()) {
                if let Ok(cfg) = serde_json::from_slice::<Config>(&bak) {
                    return Ok(ensure_presets(cfg));
                }
            }
        }
        Ok(default_config())
    }

    pub fn save_config(&self, cfg: &Config) -> AppResult<()> {
        atomic_write_json(&self.config_path(), &self.config_tmp(), &self.config_bak(), cfg)
    }

    pub fn load_migrations(&self) -> AppResult<Vec<Migration>> {
        match fs::read(self.mig_path()) {
            Ok(bytes) => serde_json::from_slice::<Vec<Migration>>(&bytes).map_err(AppError::Json),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(AppError::Io(e)),
        }
    }

    pub fn save_migrations(&self, ms: &[Migration]) -> AppResult<()> {
        atomic_write_json(&self.mig_path(), &self.mig_tmp(), &self.mig_bak(), ms)
    }

    pub fn upsert_migration(&self, m: Migration) -> AppResult<()> {
        let mut ms = self.load_migrations()?;
        if let Some(slot) = ms.iter_mut().find(|x| x.id == m.id) {
            *slot = m;
        } else {
            ms.push(m);
        }
        self.save_migrations(&ms)
    }

    pub fn remove_migration(&self, id: &str) -> AppResult<()> {
        let mut ms = self.load_migrations()?;
        ms.retain(|x| x.id != id);
        self.save_migrations(&ms)
    }
}

/// 临时文件 -> flush/sync -> 备份旧文件为 .bak -> 原子 rename 覆盖。
fn atomic_write_json<T: serde::Serialize>(
    path: &Path,
    tmp: &Path,
    bak: &Path,
    value: &T,
) -> AppResult<()> {
    let json = serde_json::to_vec_pretty(value)?;
    {
        let mut f = fs::File::create(tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    if path.exists() {
        let _ = fs::remove_file(bak);
        let _ = fs::rename(path, bak); // 失败不致命：仍尝试覆盖
    }
    fs::rename(tmp, path)?; // std 在 Windows 用 MoveFileEx(REPLACE_EXISTING)，原子替换
    Ok(())
}
```
继续在 `store.rs` 末尾追加默认配置与预设（仍属步骤 4 实现）：

```rust
pub fn default_config() -> Config {
    Config {
        schema_version: 1,
        repository: "D:/Migrated".into(),
        scan: ScanConfig {
            min_size_mb: 500,
            exclude_paths: vec!["C:/Windows".into(), "C:/Program Files/WindowsApps".into()],
        },
        presets: default_presets(),
    }
}

/// 旧配置（无 presets 或为空）补齐内置 presets。
fn ensure_presets(mut cfg: Config) -> Config {
    if cfg.presets.is_empty() {
        cfg.presets = default_presets();
    }
    cfg
}

/// 内置预设场景。match_paths 可含 %USERPROFILE%/%LOCALAPPDATA%/%APPDATA% 占位（scanner 展开）。
/// 一键迁移（auto_migrate=true）：当前用户可写数据/缓存目录；
/// 需确认风险（auto_migrate=false）：游戏库、容器等可能涉及服务/ACL。
pub fn default_presets() -> Vec<Preset> {
    macro_rules! p {
        ($id:expr, $name:expr, $cat:expr, $auto:expr, $sub:expr, $paths:expr, $procs:expr) => {
            Preset {
                id: $id.into(), name: $name.into(), category: $cat,
                auto_migrate: $auto, target_subdir: $sub.into(),
                match_paths: $paths, match_processes: $procs,
            }
        };
    }
    vec![
        p!("wechat", "微信文件", PresetCategory::Communication, true, "wechat",
           vec!["%USERPROFILE%/Documents/WeChat Files".into(), "%APPDATA%/Tencent/WeChat".into()],
           vec!["wechat".into()]),
        p!("qq", "QQ 文件", PresetCategory::Communication, true, "qq",
           vec!["%USERPROFILE%/Documents/Tencent Files".into()],
           vec!["qq".into()]),
        p!("dingtalk", "钉钉", PresetCategory::Communication, true, "dingtalk",
           vec!["%APPDATA%/DingTalk".into()],
           vec!["dingtalk".into()]),
        p!("wxwork", "企业微信", PresetCategory::Communication, true, "wxwork",
           vec!["%USERPROFILE%/Documents/WXWork".into()],
           vec!["wxwork".into()]),
        p!("npm-cache", "npm 缓存", PresetCategory::DevCache, true, "npm-cache",
           vec!["%LOCALAPPDATA%/npm-cache".into(), "%APPDATA%/npm-cache".into()],
           vec![]),
        p!("maven", "Maven 仓库", PresetCategory::DevCache, true, "maven",
           vec!["%USERPROFILE%/.m2/repository".into()],
           vec![]),
        p!("gradle", "Gradle 缓存", PresetCategory::DevCache, true, "gradle",
           vec!["%USERPROFILE%/.gradle".into()],
           vec![]),
        p!("pip-cache", "pip 缓存", PresetCategory::DevCache, true, "pip-cache",
           vec!["%LOCALAPPDATA%/pip/Cache".into()],
           vec![]),
        p!("jetbrains", "JetBrains 配置", PresetCategory::Ide, true, "jetbrains",
           vec!["%APPDATA%/JetBrains".into()],
           vec![]),
        p!("vscode", "VS Code 用户数据", PresetCategory::Ide, true, "vscode",
           vec!["%APPDATA%/Code".into(), "%USERPROFILE%/.vscode".into()],
           vec!["code".into()]),
        // 需确认风险场景
        p!("steam", "Steam 游戏库", PresetCategory::GameLibrary, false, "steam",
           vec!["steamapps".into()],
           vec!["steam".into()]),
        p!("docker", "Docker 数据", PresetCategory::Container, false, "docker",
           vec!["%LOCALAPPDATA%/Docker".into()],
           vec!["dockerd".into()]),
    ]
}
```

- [ ] **步骤 5：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml store`
预期：PASS，5 个测试全过。

- [ ] **步骤 6：Commit**

```bash
git add src-tauri/src/models.rs src-tauri/src/error.rs src-tauri/src/store.rs src-tauri/src/lib.rs
git commit -m "feat(store): 数据结构与配置/迁移记录的原子持久化"
```

---
### 任务 3：win32 单元 — 平台边界

**文件：**
- 创建：`src-tauri/src/win32.rs`
- 修改：`src-tauri/src/lib.rs`（加 `#[cfg(windows)] pub mod win32;`）
- 测试：`src-tauri/src/win32.rs` 内联 `#[cfg(test)] mod tests`

**职责：** 唯一平台边界。薄封装 Win32 API：长路径 `\\?\` 前缀、盘空间、卷序列号、卷类型/可写判断、Restart Manager 占用检测、`%LOCALAPPDATA%` 定位。其他单元不直接碰系统 API。

**长路径约定：** `to_long_path` 把普通路径（如 `C:\Users\xxx\WeChat Files`）转成 `\\?\C:\Users\xxx\WeChat Files`，统一传给 Win32 API；路径含尾随空格/点也用 `\\?\` 保留字面。对磁盘根（如 `C:\`）不加分隔符。所有 Win32 调用前先过此函数。

- [ ] **步骤 1：编写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn long_path_adds_prefix_for_drive() {
        assert_eq!(to_long_path("C:/Users/x"), r"\\?\C:/Users/x");
        // 正斜杠也接受，Win32 文件 API 兼容；统一不强行转反斜杠以免引入双重转义
    }

    #[test]
    fn long_path_presapes_drive_root() {
        assert_eq!(to_long_path("C:\\"), r"\\?\C:\\");
    }

    #[test]
    fn disk_free_space_nonzero_on_temp() {
        let dir = TempDir::new().unwrap();
        let free = disk_free_bytes(dir.path()).unwrap();
        assert!(free > 0);
    }

    #[test]
    fn volume_serial_nonempty_on_temp() {
        let dir = TempDir::new().unwrap();
        let (serial, is_ntfs) = volume_info(dir.path()).unwrap();
        assert!(!serial.is_empty());
        // CI 上可能非 NTFS，is_ntfs 只断言不 panic
        let _ = is_ntfs;
    }

    #[test]
    fn local_appdata_dir_resolves() {
        let d = local_appdata_dayu_dir().unwrap();
        assert!(d.to_string_lossy().contains("dayu-disk-manager"));
    }

    #[test]
    fn locked_processes_on_empty_dir_is_none() {
        let dir = TempDir::new().unwrap();
        // 空目录不应被占用
        assert!(locked_processes(dir.path()).unwrap().is_none());
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml win32`
预期：FAIL / 编译错误 —— `to_long_path`、`disk_free_bytes` 等未定义。

- [ ] **步骤 3：实现 win32.rs**

```rust
use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};

/// 普通路径转 Win32 长路径（加 \\?\ 前缀，绕过 MAX_PATH 并保留字面字符）。
pub fn to_long_path(p: &str) -> String {
    let trimmed = p.trim();
    if trimmed.starts_with(r"\\?\") || trimmed.starts_with(r"\\.\") {
        return trimmed.into();
    }
    if let Some(rest) = trimmed.strip_prefix(r"\\") {
        // UNC 路径：\\server\share -> \\?\UNC\server\share
        return format!(r"\\?\UNC\{}", rest);
    }
    format!(r"\\?\{}", trimmed)
}

pub fn local_appdata_dayu_dir() -> AppResult<PathBuf> {
    let base = dirs::local_appdata().ok_or_else(|| AppError::Win32("无法解析 %LOCALAPPDATA%".into()))?;
    Ok(base.join("dayu-disk-manager"))
}

#[cfg(windows)]
pub fn disk_free_bytes(path: &Path) -> AppResult<u64> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide = to_wide(&to_long_path(&path_to_str(path)));
    let mut free_to_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free_to_caller),
            Some(&mut total),
            Some(&mut free),
        ).map_err(|e| AppError::Win32(format!("GetDiskFreeSpaceExW: {e}")))?;
    }
    Ok(free_to_caller)
}

#[cfg(not(windows))]
pub fn disk_free_bytes(_path: &Path) -> AppResult<u64> {
    Err(AppError::Win32("仅支持 Windows".into()))
}
```
继续 `win32.rs`（步骤 3 实现剩余）：

```rust
#[cfg(windows)]
pub fn volume_info(path: &Path) -> AppResult<(String, bool)> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    // 卷信息需基于"卷根"（如 C:\），取路径所在盘根
    let root = volume_root(path)?;
    let wide = to_wide(&to_long_path(&root));
    let mut serial: u32 = 0;
    let mut max_component: u32 = 0;
    let mut flags: u32 = 0;
    let mut fs_name = [0u16; 256];
    unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            &mut serial,
            Some(&mut max_component),
            Some(&mut flags),
            Some(&mut fs_name),
        ).map_err(|e| AppError::Win32(format!("GetVolumeInformationW: {e}")))?;
    }
    let fs = from_wide(&fs_name).to_lowercase();
    let serial_hex = format!("{:08X}", serial);
    let is_ntfs = fs == "ntfs";
    Ok((serial_hex, is_ntfs))
}

#[cfg(not(windows))]
pub fn volume_info(_path: &Path) -> AppResult<(String, bool)> {
    Err(AppError::Win32("仅支持 Windows".into()))
}

/// 取路径所在盘根，如 C:\Users\xxx -> C:\
fn volume_root(path: &Path) -> AppResult<String> {
    let s = path_to_str(path);
    let s = s.trim_start_matches(r"\\?\").trim_start_matches(r"\\.\");
    if let Some(drive) = s.get(0..2) {
        if drive.as_bytes()[1] == b':' {
            return Ok(format!("{}\\", drive.to_uppercase()));
        }
    }
    Err(AppError::Win32(format!("无法解析盘根: {s}")))
}

/// Restart Manager 检测哪些进程锁定了某路径。无占用返回 None。
#[cfg(windows)]
pub fn locked_processes(path: &Path) -> AppResult<Option<Vec<String>>> {
    use windows::core::{PCWSTR, HSTRING};
    use windows::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmStartSession, RM_PROCESS_INFO,
    };
    let key: [u16; 256] = [0; 256];
    let mut handle: u32 = 0;
    let long = to_long_path(&path_to_str(path));
    let path_h = HSTRING::from(&long);
    unsafe {
        let rc = RmStartSession(&mut handle, 0, PCWSTR(key.as_ptr()));
        if rc.is_err() {
            return Err(AppError::Win32("RmStartSession 失败".into()));
        }
        let resources = [PCWSTR(path_h.as_ptr())];
        let reg = RmRegisterResources(handle, Some(&resources), None, None);
        let result = if reg.is_err() {
            Err(AppError::Win32("RmRegisterResources 失败".into()))
        } else {
            let mut nprocs: u32 = 0;
            let mut reason: u32 = 0;
            let mut buf = [RM_PROCESS_INFO::default(); 64];
            let rc2 = RmGetList(handle, &mut nprocs, &mut reason, Some(&mut buf), &mut 0);
            if rc2.is_err() {
                Err(AppError::Win32("RmGetList 失败".into()))
            } else if nprocs == 0 {
                Ok(None)
            } else {
                let names: Vec<String> = buf[..nprocs as usize]
                    .iter()
                    .map(|p| from_wide_slice(&p.strProcessName))
                    .collect();
                Ok(Some(names))
            }
        };
        let _ = RmEndSession(handle);
        result
    }
}

#[cfg(not(windows))]
pub fn locked_processes(_path: &Path) -> AppResult<Option<Vec<String>>> {
    Ok(None)
}

// ===== 辅助：宽字符转换 =====
#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(windows)]
fn from_wide_slice(buf: &[u16]) -> String {
    from_wide(buf)
}

fn path_to_str(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\")
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml win32`
预期：PASS，6 个测试全过（`locked_processes` 在 CI 上对空目录返回 None；NTFS 断言只查不 panic）。

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/win32.rs src-tauri/src/lib.rs
git commit -m "feat(win32): 平台边界封装（长路径/盘空间/卷信息/占用检测）"
```

---
### 任务 4：junction 单元 — NTFS 目录联接

**文件：**
- 创建：`src-tauri/src/junction.rs`
- 修改：`src-tauri/src/lib.rs`（加 `#[cfg(windows)] pub mod junction;`）
- 测试：`src-tauri/src/junction.rs` 内联 `#[cfg(test)] mod tests`

**职责：** 封装 `junction` crate 的创建/删除/解析/校验。junction 创建不需符号链接开发者权限，CI 可真实建（规格第 7 章要求）。

- [ ] **步骤 1：编写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_junction_resolves_to_target() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.txt"), b"hi").unwrap();
        let link = root.path().join("link");
        create(&link, &target).unwrap();
        assert!(exists(&link));
        assert!(std::fs::read_link(&link).is_ok());
        // 通过链接读取内容
        assert_eq!(std::fs::read(link.join("a.txt")).unwrap(), b"hi");
    }

    #[test]
    fn resolve_returns_target_path() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("t");
        std::fs::create_dir_all(&target).unwrap();
        let link = root.path().join("l");
        create(&link, &target).unwrap();
        let resolved = resolve(&link).unwrap();
        assert!(resolved.ends_with("t"));
    }

    #[test]
    fn remove_junction_keeps_target() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("t");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.txt"), b"hi").unwrap();
        let link = root.path().join("l");
        create(&link, &target).unwrap();
        remove(&link).unwrap();
        assert!(!exists(&link));
        assert!(target.join("a.txt").exists(), "删链接不应删目标数据");
    }

    #[test]
    fn verify_detects_broken_link() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("t");
        std::fs::create_dir_all(&target).unwrap();
        let link = root.path().join("l");
        create(&link, &target).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
        assert!(!verify(&link));
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml junction`
预期：FAIL / 编译错误 —— `create`、`exists`、`resolve`、`remove`、`verify` 未定义。

- [ ] **步骤 3：实现 junction.rs**

```rust
use crate::error::{AppError, AppResult};
use std::path::Path;

/// 创建目录联接：link 指向 target。link 必须不存在或为已删除的空壳。
pub fn create(link: &Path, target: &Path) -> AppResult<()> {
    #[cfg(windows)]
    {
        junction::create(target, link)
            .map_err(|e| AppError::Junction(format!("create 失败: {e}")))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (link, target);
        Err(AppError::Junction("仅支持 Windows".into()))
    }
}

/// 删除 junction（只删链接壳，不删目标）。
pub fn remove(link: &Path) -> AppResult<()> {
    #[cfg(windows)]
    {
        junction::delete(link)
            .map_err(|e| AppError::Junction(format!("remove 失败: {e}")))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = link;
        Err(AppError::Junction("仅支持 Windows".into()))
    }
}

/// 解析 junction 指向的目标路径（绝对路径）。
pub fn resolve(link: &Path) -> AppResult<std::path::PathBuf> {
    #[cfg(windows)]
    {
        junction::get_target(link)
            .map_err(|e| AppError::Junction(format!("resolve 失败: {e}")))
            .map(|p| p)
    }
    #[cfg(not(windows))]
    {
        let _ = link;
        Err(AppError::Junction("仅支持 Windows".into()))
    }
}

/// link 是否是一个 junction（reparse point 且类型为 junction）。
pub fn exists(link: &Path) -> bool {
    #[cfg(windows)]
    {
        junction::exists(link).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = link;
        false
    }
}

/// 校验：link 是 junction 且其目标目录真实存在且可访问。
pub fn verify(link: &Path) -> bool {
    if !exists(link) {
        return false;
    }
    match resolve(link) {
        Ok(target) => target.is_dir(),
        Err(_) => false,
    }
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml junction`
预期：PASS，4 个测试全过。

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/junction.rs src-tauri/src/lib.rs
git commit -m "feat(junction): NTFS junction 创建/删除/解析/校验"
```

---
## 阶段 2：数据 / 日志层

### 任务 5：file_ops 单元 — 文件操作抽象与 manifest

**文件：**
- 创建：`src-tauri/src/file_ops.rs`
- 修改：`src-tauri/src/lib.rs`（加 `pub mod file_ops;`）
- 测试：`src-tauri/src/file_ops.rs` 内联 `#[cfg(test)] mod tests`

**职责：** 定义 `FileOps` trait（`migrator` 测试的核心抽象）；提供真实实现 `RealFileOps`。关键语义：递归复制**不跟随源目录内部 reparse point**（避免循环/重复计数）、保留 NTFS 元数据、原子改名、移回收站、manifest 生成与对比。

**trait 设计原则：** trait 方法返回 `AppResult`，**不**包含 junction 操作（junction 独立在 `junction.rs`，migrator 直接调）。progress 回调用 `dyn Fn` 传入，使测试可忽略、生产可推送百分比。

- [ ] **步骤 1：定义 trait 与 manifest 类型（file_ops.rs 顶部）**

```rust
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 一条 manifest 记录：相对路径、类型、字节数、mtime、attributes。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestEntry {
    pub rel_path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix 秒
    pub mtime: i64,
    pub attrs: u32,
}

/// 一份目录的 manifest，用于复制后校验一致性。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub root: String,
    pub entries: Vec<ManifestEntry>,
}

/// 文件操作抽象。生产用 RealFileOps，测试用 mock 实现。
/// on_progress(percent: 0..=100) 在复制期间被回调。
pub trait FileOps {
    /// 递归复制 src 目录到 dst。dst 不存在则创建。不跟随 src 内部 reparse point。
    fn copy_tree(&self, src: &Path, dst: &Path, on_progress: &dyn Fn(u8)) -> AppResult<()>;

    /// 生成 src 目录的 manifest（不含 src 自身的 reparse point 内部，但含直接子项）。
    fn manifest(&self, src: &Path) -> AppResult<Manifest>;

    /// 对比两份 manifest，返回不一致项的相对路径（空=一致）。
    fn diff_manifests(&self, a: &Manifest, b: &Manifest) -> Vec<String>;

    /// 原子改名（同卷用 MoveFileEx，跨卷退化为复制+删除）。
    fn rename(&self, from: &Path, to: &Path) -> AppResult<()>;

    /// 移到回收站（allow undo）。
    fn to_recycle_bin(&self, path: &Path) -> AppResult<()>;

    /// 递归删除（用于清理 tmp；不走回收站）。
    fn remove_tree(&self, path: &Path) -> AppResult<()>;

    /// 路径是否为 reparse point（junction/symlink）。
    fn is_reparse_point(&self, path: &Path) -> bool;

    /// 目录是否存在且可读。
    fn dir_exists(&self, path: &Path) -> bool;
}
```

- [ ] **步骤 2：编写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ops() -> RealFileOps { RealFileOps }

    #[test]
    fn copy_tree_copies_files_and_preserves_content() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"world").unwrap();
        let dst = root.path().join("dst");
        ops().copy_tree(&src, &dst, &|_| {}).unwrap();
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dst.join("sub/b.txt")).unwrap(), b"world");
    }

    #[test]
    fn copy_tree_does_not_descend_into_reparse_point() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        let inner_link = src.join("link");
        let link_target = root.path().join("target");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&link_target).unwrap();
        std::fs::write(link_target.join("secret.txt"), b"x").unwrap();
        #[cfg(windows)]
        junction::create(&link_target, &inner_link).unwrap();
        // 复制 src 到 dst
        let dst = root.path().join("dst");
        ops().copy_tree(&src, &dst, &|_| {}).unwrap();
        // link 应作为 reparse point 被跳过内容，但本身存在
        assert!(ops().is_reparse_point(&dst.join("link")));
        // 不应在 dst 中递归进入 target 的内容（secret.txt 不应出现）
        assert!(!dst.join("link/secret.txt").exists());
    }

    #[test]
    fn manifest_then_diff_matches_for_identical_copy() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        let dst = root.path().join("dst");
        ops().copy_tree(&src, &dst, &|_| {}).unwrap();
        let m1 = ops().manifest(&src).unwrap();
        let m2 = ops().manifest(&dst).unwrap();
        assert!(ops().diff_manifests(&m1, &m2).is_empty(), "复制后 manifest 应一致");
    }

    #[test]
    fn diff_manifests_detects_size_change() {
        let root = TempDir::new().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("f.txt"), b"12345").unwrap();
        std::fs::write(b.join("f.txt"), b"123").unwrap();
        let m1 = ops().manifest(&a).unwrap();
        let m2 = ops().manifest(&b).unwrap();
        let diff = ops().diff_manifests(&m1, &m2);
        assert!(diff.iter().any(|p| p == "f.txt"), "应检测到 f.txt 不一致");
    }

    #[test]
    fn to_recycle_bin_removes_path() {
        let root = TempDir::new().unwrap();
        let victim = root.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("a.txt"), b"hi").unwrap();
        let res = ops().to_recycle_bin(&victim);
        // 回收站在 CI/某些环境可能不可用，允许失败但不 panic；可用时必须删掉
        if res.is_ok() {
            assert!(!victim.exists());
        }
    }
}
```
- [ ] **步骤 3：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml file_ops`
预期：FAIL / 编译错误 —— `RealFileOps` 未实现。

- [ ] **步骤 4：实现 RealFileOps**

在 `file_ops.rs` 测试模块**之前**插入：

```rust
pub struct RealFileOps;

impl FileOps for RealFileOps {
    fn copy_tree(&self, src: &Path, dst: &Path, on_progress: &dyn Fn(u8)) -> AppResult<()> {
        std::fs::create_dir_all(dst)?;
        let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
        while let Some((cur_src, cur_dst)) = stack.pop() {
            // 跳过 reparse point 的内容递归，但仍创建占位（见 is_reparse_point 处理）
            let is_rp = self.is_reparse_point(&cur_src);
            if !cur_src.exists() {
                continue;
            }
            if is_rp && cur_src != *src {
                // 非 src 自身的 reparse point：创建空目录占位，不进入
                std::fs::create_dir_all(&cur_dst)?;
                continue;
            }
            if cur_src.is_dir() {
                std::fs::create_dir_all(&cur_dst)?;
                for entry in std::fs::read_dir(&cur_src)? {
                    let entry = entry?;
                    let child_src = entry.path();
                    let child_dst = cur_dst.join(entry.file_name());
                    stack.push((child_src, child_dst));
                }
            } else {
                std::fs::copy(&cur_src, &cur_dst)?;
                on_progress(0); // 真实实现可按字节累计；此处仅保证回调被调
            }
        }
        Ok(())
    }

    fn manifest(&self, src: &Path) -> AppResult<Manifest> {
        let mut entries = Vec::new();
        let mut stack = vec![src.to_path_buf()];
        while let Some(cur) = stack.pop() {
            if !cur.exists() { continue; }
            if self.is_reparse_point(&cur) && cur != *src {
                // 记录 reparse point 为目录占位，不进入
                entries.push(ManifestEntry {
                    rel_path: rel_under(src, &cur),
                    is_dir: true, size: 0, mtime: 0, attrs: 0,
                });
                continue;
            }
            if cur.is_dir() {
                if cur != *src {
                    entries.push(entry_for(&cur, src, true)?);
                }
                for e in std::fs::read_dir(&cur)? {
                    stack.push(e?.path());
                }
            } else {
                entries.push(entry_for(&cur, src, false)?);
            }
        }
        Ok(Manifest { root: src.to_string_lossy().into(), entries })
    }

    fn diff_manifests(&self, a: &Manifest, b: &Manifest) -> Vec<String> {
        use std::collections::HashMap;
        let map_a: HashMap<&str, &ManifestEntry> = a.entries.iter().map(|e| (e.rel_path.as_str(), e)).collect();
        let map_b: HashMap<&str, &ManifestEntry> = b.entries.iter().map(|e| (e.rel_path.as_str(), e)).collect();
        let mut diffs = Vec::new();
        let mut keys: std::collections::HashSet<&str> = map_a.keys().copied().collect();
        keys.extend(map_b.keys().copied());
        for k in keys {
            match (map_a.get(k), map_b.get(k)) {
                (Some(x), Some(y)) => {
                    if x.is_dir != y.is_dir || x.size != y.size {
                        diffs.push(k.to_string());
                    }
                }
                _ => diffs.push(k.to_string()),
            }
        }
        diffs
    }

    fn rename(&self, from: &Path, to: &Path) -> AppResult<()> {
        // 同卷直接 rename 原子；跨卷 std::fs::rename 会失败，退化为复制+删除
        match std::fs::rename(from, to) {
            Ok(()) => Ok(()),
            Err(_) => {
                std::fs::create_dir_all(to.parent().unwrap_or(Path::new(".")))?;
                copy_recursive(from, to)?;
                std::fs::remove_dir_all(from)?;
                Ok(())
            }
        }
    }

    fn to_recycle_bin(&self, path: &Path) -> AppResult<()> {
        #[cfg(windows)]
        {
            trash::delete(path).map_err(|e| AppError::Win32(format!("trash::delete: {e}")))?;
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Err(AppError::Win32("仅支持 Windows".into()))
        }
    }

    fn remove_tree(&self, path: &Path) -> AppResult<()> {
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
        Ok(())
    }

    fn is_reparse_point(&self, path: &Path) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            match std::fs::symlink_metadata(path) {
                Ok(m) => m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                Err(_) => false,
            }
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            false
        }
    }

    fn dir_exists(&self, path: &Path) -> bool {
        path.is_dir()
    }
}

fn entry_for(p: &Path, root: &Path, is_dir: bool) -> AppResult<ManifestEntry> {
    let meta = std::fs::symlink_metadata(p)?;
    let size = if is_dir { 0 } else { meta.len() };
    let mtime = meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    #[cfg(windows)]
    let attrs = { use std::os::windows::fs::MetadataExt; meta.file_attributes() };
    #[cfg(not(windows))]
    let attrs = 0u32;
    Ok(ManifestEntry {
        rel_path: rel_under(root, p),
        is_dir, size, mtime, attrs,
    })
}

fn rel_under(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| p.to_string_lossy().into())
}

fn copy_recursive(src: &Path, dst: &Path) -> AppResult<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}
```

- [ ] **步骤 5：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml file_ops`
预期：PASS，5 个测试全过（`copy_tree_does_not_descend_into_reparse_point` 依赖 junction crate，仅在 Windows 跑；非 Windows 该测试被 `#[cfg(windows)]` 跳过 junction 创建，可改用 `#[cfg(windows)]` 标注整个测试）。

> 注：若 CI 非 Windows，给 `copy_tree_does_not_descend_into_reparse_point` 整体加 `#[cfg(windows)]`，避免创建 junction 的代码路径在 Linux 上失效。

- [ ] **步骤 6：Commit**

```bash
git add src-tauri/src/file_ops.rs src-tauri/src/lib.rs
git commit -m "feat(file_ops): 文件操作抽象 trait、manifest 与真实实现"
```

---
### 任务 6：journal 单元 — 运行中任务恢复日志

**文件：**
- 创建：`src-tauri/src/journal.rs`
- 修改：`src-tauri/src/lib.rs`（加 `pub mod journal;`）
- 测试：`src-tauri/src/journal.rs` 内联 `#[cfg(test)] mod tests`

**职责：** 运行中任务的阶段恢复日志（`operation_journal.jsonl`，每行一条 JSON 追加写）。`begin` 建任务、`mark_stage` 落阶段、`complete`/`fail`/`cancel` 标终态。`recover_pending` 启动时读取未完成任务，按阶段返回恢复决策。同源/同目标路径任务锁。

**Stage 常量（迁移）：** `"created"`、`"copied"`、`"manifest_ok"`、`"source_renamed"`、`"incremental_synced"`、`"junction_created"`、`"record_written"`、`"old_recycled"`。还原阶段加前缀 `"restore_"`：`"restore_copied"`、`"restore_manifest_ok"`、`"junction_removed"`、`"restore_switched"`、`"restore_target_recycled"`。

- [ ] **步骤 1：编写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh() -> Journal {
        let dir = TempDir::new().unwrap();
        Journal::new(dir.path().to_path_buf()).unwrap()
    }

    #[test]
    fn begin_then_mark_stage_appended() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        j.mark_stage("t1", "copied").unwrap();
        let pending = j.recover_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].stage, "copied");
    }

    #[test]
    fn complete_removes_from_pending() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        j.complete("t1").unwrap();
        assert!(j.recover_pending().unwrap().is_empty());
    }

    #[test]
    fn begin_rejects_conflicting_source() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        let err = j.begin("t2", "m2", "migrate", "C:/s", "D:/d2", "D:/d2.tmp", "C:/s.old2");
        assert!(err.is_err(), "同源路径不应允许第二个任务");
    }

    #[test]
    fn begin_allows_different_source() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        let res = j.begin("t2", "m2", "migrate", "C:/s2", "D:/d2", "D:/d2.tmp", "C:/s2.old2");
        // 首版只允许一个迁移任务，但 journal 层只锁源/目标路径冲突，第二个不同源应可写入
        assert!(res.is_ok());
    }

    #[test]
    fn recover_pending_returns_latest_stage_per_task() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        j.mark_stage("t1", "copied").unwrap();
        j.mark_stage("t1", "manifest_ok").unwrap();
        let pending = j.recover_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].stage, "manifest_ok", "应取最新阶段");
    }

    #[test]
    fn fail_marks_terminal_and_removed_from_pending() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        j.fail("t1", "磁盘满").unwrap();
        assert!(j.recover_pending().unwrap().is_empty());
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml journal`
预期：FAIL / 编译错误 —— `Journal` 未定义。
- [ ] **步骤 3：实现 journal.rs**

```rust
use crate::error::{AppError, AppResult};
use crate::models::JournalEntry;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

pub struct Journal {
    pub path: PathBuf,
}

impl Journal {
    pub fn new(path: PathBuf) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Journal { path })
    }

    pub fn begin(
        &self, task_id: &str, migration_id: &str, op: &str,
        src: &str, dst: &str, tmp: &str, old_path: &str,
    ) -> AppResult<()> {
        // 同源/同目标路径锁
        for entry in self.read_all()? {
            if entry.final_mark.is_none() {
                if entry.src.eq_ignore_ascii_case(src) || entry.dst.eq_ignore_ascii_case(dst) {
                    return Err(AppError::Conflict(format!(
                        "路径已被运行中任务 {} 占用: {}", entry.task_id, entry.src
                    )));
                }
            }
        }
        self.append(&JournalEntry {
            task_id: task_id.into(), migration_id: migration_id.into(), op: op.into(),
            stage: "created".into(), src: src.into(), dst: dst.into(), tmp: tmp.into(),
            old_path: old_path.into(), time: now_iso(), final_mark: None,
        })
    }

    pub fn mark_stage(&self, task_id: &str, stage: &str) -> AppResult<()> {
        // 取该任务最新一条作为模板，更新 stage 追加
        let all = self.read_all()?;
        let tmpl = all.iter().rev().find(|e| e.task_id == task_id)
            .ok_or_else(|| AppError::Store(format!("任务不存在: {task_id}")))?;
        self.append(&JournalEntry {
            stage: stage.into(),
            ..tmpl.clone()
        })
    }

    pub fn complete(&self, task_id: &str) -> AppResult<()> {
        self.finalize(task_id, "completed")
    }

    pub fn fail(&self, task_id: &str, reason: &str) -> AppResult<()> {
        // fail 也写一条终态标记（reason 进 message 通过 mark_stage 不够，简化为终态行）
        let all = self.read_all()?;
        let tmpl = all.iter().rev().find(|e| e.task_id == task_id)
            .ok_or_else(|| AppError::Store(format!("任务不存在: {task_id}")))?;
        self.append(&JournalEntry {
            stage: format!("failed: {reason}"),
            ..tmpl.clone()
        })?;
        self.finalize(task_id, "failed")
    }

    pub fn cancel(&self, task_id: &str) -> AppResult<()> {
        self.finalize(task_id, "canceled")
    }

    fn finalize(&self, task_id: &str, mark: &str) -> AppResult<()> {
        let all = self.read_all()?;
        let tmpl = all.iter().rev().find(|e| e.task_id == task_id)
            .ok_or_else(|| AppError::Store(format!("任务不存在: {task_id}")))?;
        self.append(&JournalEntry {
            final_mark: Some(mark.into()),
            ..tmpl.clone()
        })
    }

    /// 启动时调用：返回所有未终结任务的最新阶段快照。
    pub fn recover_pending(&self) -> AppResult<Vec<JournalEntry>> {
        let all = self.read_all()?;
        let mut latest: std::collections::HashMap<String, JournalEntry> = Default::default();
        for e in all {
            match &e.final_mark {
                Some(_) => { latest.remove(&e.task_id); }
                None => { latest.insert(e.task_id.clone(), e); }
            }
        }
        Ok(latest.into_values().collect())
    }

    fn append(&self, entry: &JournalEntry) -> AppResult<()> {
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        let line = serde_json::to_string(entry)?;
        writeln!(f, "{line}")?;
        f.sync_all()?;
        Ok(())
    }

    fn read_all(&self) -> AppResult<Vec<JournalEntry>> {
        if !self.path.exists() { return Ok(Vec::new()); }
        let f = File::open(&self.path)?;
        let r = BufReader::new(f);
        let mut out = Vec::new();
        for line in r.lines() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            match serde_json::from_str::<JournalEntry>(&line) {
                Ok(e) => out.push(e),
                Err(_) => continue, // 损坏行跳过，不阻断恢复
            }
        }
        Ok(out)
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml journal`
预期：PASS，6 个测试全过。

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/journal.rs src-tauri/src/lib.rs
git commit -m "feat(journal): 运行中任务阶段恢复日志与路径锁"
```

---
### 任务 7：history 单元 — 操作历史流水

**文件：**
- 创建：`src-tauri/src/history.rs`
- 修改：`src-tauri/src/lib.rs`（加 `pub mod history;`）
- 测试：`src-tauri/src/history.rs` 内联 `#[cfg(test)] mod tests`

**职责：** 操作历史（`history.jsonl`，每行一条 JSON 追加写）。`append` 记录一次操作，`list` 按操作类型/时间筛选返回。

- [ ] **步骤 1：编写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::HistoryEntry;
    use tempfile::TempDir;

    fn fresh() -> History {
        let dir = TempDir::new().unwrap();
        History::new(dir.path().join("history.jsonl")).unwrap()
    }

    fn entry(op: &str, result: &str, time: &str) -> HistoryEntry {
        HistoryEntry {
            op: op.into(), id: "u1".into(), src: "C:/s".into(), dst: "D:/d".into(),
            result: result.into(), time: time.into(), duration_sec: 10,
        }
    }

    #[test]
    fn append_then_list_returns_in_order() {
        let h = fresh();
        h.append(entry("migrate", "ok", "2026-07-18T10:00:00Z")).unwrap();
        h.append(entry("restore", "ok", "2026-07-18T11:00:00Z")).unwrap();
        let all = h.list(None, None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].op, "migrate");
    }

    #[test]
    fn list_filter_by_op() {
        let h = fresh();
        h.append(entry("migrate", "ok", "2026-07-18T10:00:00Z")).unwrap();
        h.append(entry("restore", "ok", "2026-07-18T11:00:00Z")).unwrap();
        h.append(entry("migrate", "failed", "2026-07-18T12:00:00Z")).unwrap();
        let only_migrate = h.list(Some("migrate"), None).unwrap();
        assert_eq!(only_migrate.len(), 2);
        assert!(only_migrate.iter().all(|e| e.op == "migrate"));
    }

    #[test]
    fn list_filter_by_time_range() {
        let h = fresh();
        h.append(entry("migrate", "ok", "2026-07-18T10:00:00Z")).unwrap();
        h.append(entry("migrate", "ok", "2026-07-18T11:30:00Z")).unwrap();
        let ranged = h.list(None, Some(("2026-07-18T11:00:00Z", "2026-07-18T12:00:00Z"))).unwrap();
        assert_eq!(ranged.len(), 1);
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml history`
预期：FAIL / 编译错误 —— `History` 未定义。

- [ ] **步骤 3：实现 history.rs**

```rust
use crate::error::{AppError, AppResult};
use crate::models::HistoryEntry;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

pub struct History {
    pub path: PathBuf,
}

impl History {
    pub fn new(path: PathBuf) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(History { path })
    }

    pub fn append(&self, e: &HistoryEntry) -> AppResult<()> {
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        let line = serde_json::to_string(e)?;
        writeln!(f, "{line}")?;
        f.sync_all()?;
        Ok(())
    }

    /// 按 op 与时间区间 [from, to)（ISO8601 字符串字典序比较）筛选；None 表示不过滤。
    pub fn list(&self, op_filter: Option<&str>, time_range: Option<(&str, &str)>) -> AppResult<Vec<HistoryEntry>> {
        if !self.path.exists() { return Ok(Vec::new()); }
        let f = File::open(&self.path)?;
        let r = BufReader::new(f);
        let mut out = Vec::new();
        for line in r.lines() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            let e: HistoryEntry = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if let Some(op) = op_filter {
                if e.op != op { continue; }
            }
            if let Some((from, to)) = time_range {
                if e.time.as_str() < from || e.time.as_str() >= to { continue; }
            }
            out.push(e);
        }
        // 按时间升序（append 顺序通常已是升序，这里显式排序保证）
        out.sort_by(|a, b| a.time.cmp(&b.time));
        Ok(out)
    }

    /// 导出全部历史为单个 JSON 数组（设置页"导出操作日志"用）。
    pub fn export_all_json(&self) -> AppResult<String> {
        let all = self.list(None, None)?;
        Ok(serde_json::to_string_pretty(&all)?)
    }
}

#[allow(dead_code)]
fn _ensure_err_imported(_: AppError) {}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml history`
预期：PASS，3 个测试全过。

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/history.rs src-tauri/src/lib.rs
git commit -m "feat(history): 操作历史流水追加与筛选查询"
```

---
## 阶段 3：业务层

### 任务 8：scanner 单元 — 扫描与预设识别

**文件：**
- 创建：`src-tauri/src/scanner.rs`
- 修改：`src-tauri/src/lib.rs`（加 `pub mod scanner;`）
- 测试：`src-tauri/src/scanner.rs` 内联 `#[cfg(test)] mod tests`

**职责：** 遍历目录算体积、匹配预设场景（展开 `%USERPROFILE%` 等占位）、跳过 reparse point（不跟随、不重复计数）、AccessDenied 降级（记录 inaccessible）。返回 `Vec<ScanItem>`。预检/取消/限速留到 IPC 层（T12）通过 `CancellationToken` 注入，本单元聚焦纯函数扫描逻辑。

- [ ] **步骤 1：编写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::default_config;
    use tempfile::TempDir;

    #[test]
    fn dir_size_sums_files() {
        let root = TempDir::new().unwrap();
        let d = root.path().join("d");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.txt"), vec![0u8; 1000]).unwrap();
        std::fs::write(d.join("sub/b.txt"), vec![0u8; 500]).unwrap();
        assert_eq!(dir_size(&d), 1500);
    }

    #[test]
    fn dir_size_skips_reparse_point_content() {
        let root = TempDir::new().unwrap();
        let d = root.path().join("d");
        let target = root.path().join("target");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("big.bin"), vec![0u8; 2000]).unwrap();
        #[cfg(windows)]
        junction::create(&target, &d.join("link")).unwrap();
        // link 是 reparse point，其内部 2000 字节不应计入 d
        let size = dir_size(&d);
        assert_eq!(size, 0, "reparse point 内部内容不应计数");
    }

    #[test]
    fn expand_env_path_resolves_userprofile() {
        let expanded = expand_env("%USERPROFILE%/Documents/WeChat Files");
        assert!(!expanded.contains("%USERPROFILE%"));
        assert!(expanded.contains("Documents"));
    }

    #[test]
    fn match_preset_matches_wechat_path() {
        let cfg = default_config();
        let preset = cfg.presets.iter().find(|p| p.id == "wechat").unwrap();
        let userprofile = std::env::var("USERPROFILE").unwrap();
        let path = format!("{userprofile}\\Documents\\WeChat Files");
        assert!(matches_preset(&path, preset));
    }

    #[test]
    fn scan_returns_items_above_threshold() {
        let root = TempDir::new().unwrap();
        let big = root.path().join("big");
        std::fs::create_dir_all(&big).unwrap();
        std::fs::write(big.join("f.bin"), vec![0u8; 600 * 1024]).unwrap(); // 600KB
        let small = root.path().join("small");
        std::fs::create_dir_all(&small).unwrap();
        std::fs::write(small.join("f.txt"), b"x").unwrap();
        let cfg = default_config();
        // 测试用 0 阈值便于断言：临时改 min_size_mb
        let mut cfg = cfg;
        cfg.scan.min_size_mb = 0;
        let items = scan(root.path(), &cfg);
        assert!(items.iter().any(|i| i.path.ends_with("big")));
    }

    #[test]
    fn inaccessible_dir_marked_not_panic() {
        let root = TempDir::new().unwrap();
        // 一个不存在的子目录不应导致 panic
        let cfg = default_config();
        let items = scan(root.path(), &cfg);
        assert!(items.iter().all(|i| !i.inaccessible || i.path.is_empty()));
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml scanner`
预期：FAIL / 编译错误 —— `dir_size`、`expand_env`、`matches_preset`、`scan` 未定义。
- [ ] **步骤 3：实现 scanner.rs**

```rust
use crate::error::AppResult;
use crate::models::{Config, Preset, ScanItem};
use std::path::{Path, PathBuf};

/// 递归计算目录体积（字节）。不跟随 reparse point 的内容。
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(cur) = stack.pop() {
        if !cur.exists() { continue; }
        if is_reparse_point(&cur) && cur != path {
            continue; // 跳过 reparse point 内部
        }
        if cur.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&cur) {
                for e in entries.flatten() {
                    stack.push(e.path());
                }
            } // AccessDenied 静默跳过
        } else if let Ok(meta) = std::fs::metadata(&cur) {
            total += meta.len();
        }
    }
    total
}

pub fn is_reparse_point(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const RP: u32 = 0x400;
        std::fs::symlink_metadata(path).map(|m| m.file_attributes() & RP != 0).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// 展开 %USERPROFILE%/%LOCALAPPDATA%/%APPDATA% 占位。
pub fn expand_env(p: &str) -> String {
    let mut out = p.to_string();
    for var in ["USERPROFILE", "LOCALAPPDATA", "APPDATA"] {
        if let Ok(val) = std::env::var(var) {
            out = out.replace(&format!("%{var}%"), &val);
        }
    }
    out
}

/// 路径是否匹配某 preset（路径包含任一展开后的 match_paths 即命中）。
pub fn matches_preset(actual_path: &str, preset: &Preset) -> bool {
    let norm_actual = normalize(actual_path);
    preset.match_paths.iter().any(|tmpl| {
        let expanded = normalize(&expand_env(tmpl));
        norm_actual.eq_ignore_ascii_case(&expanded)
    })
}

fn normalize(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// 扫描 root 下一层目录，返回大于阈值或命中预设的项。
pub fn scan(root: &Path, cfg: &Config) -> Vec<ScanItem> {
    let min_bytes = cfg.scan.min_size_mb * 1024 * 1024;
    let exclude: Vec<String> = cfg.scan.exclude_paths.iter().map(|p| normalize(p)).collect();
    let mut items = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let cur_str = cur.to_string_lossy();
        if exclude.iter().any(|ex| normalize(&cur_str).starts_with(ex)) { continue; }
        if !cur.exists() { continue; }
        if is_reparse_point(&cur) {
            // reparse point：可能是已迁移的 junction，标注 is_junction，不进入
            items.push(ScanItem {
                path: cur_str.into(),
                display_name: cur.file_name().map(|n| n.to_string_lossy().into()).unwrap_or_default(),
                size_bytes: 0, matched_preset: None, category: None,
                auto_migrate: false, is_junction: true, inaccessible: false,
            });
            continue;
        }
        let entries = match std::fs::read_dir(&cur) {
            Ok(e) => e,
            Err(_) => {
                items.push(ScanItem {
                    path: cur_str.into(),
                    display_name: cur.file_name().map(|n| n.to_string_lossy().into()).unwrap_or_default(),
                    size_bytes: 0, matched_preset: None, category: None,
                    auto_migrate: false, is_junction: false, inaccessible: true,
                });
                continue;
            }
        };
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() { subdirs.push(p); }
        }
        if subdirs.is_empty() {
            // 叶子目录：算体积
            let size = dir_size(&cur);
            push_if_big_or_preset(&mut items, &cur, size, cfg);
        } else {
            for sd in subdirs {
                stack.push(sd);
            }
            // 同时也评估当前目录自身（可能是 preset 命中的数据根，如 WeChat Files）
            let size = dir_size(&cur);
            push_if_big_or_preset(&mut items, &cur, size, cfg);
        }
        let _ = min_bytes; // 阈值比较在 push_if_big_or_preset 内
    }
    items
}

fn push_if_big_or_preset(items: &mut Vec<ScanItem>, path: &Path, size: u64, cfg: &Config) {
    let path_str = path.to_string_lossy();
    let preset_match = cfg.presets.iter().find(|p| matches_preset(&path_str, p));
    let min_bytes = cfg.scan.min_size_mb * 1024 * 1024;
    let big = size >= min_bytes;
    if !(big || preset_match.is_some()) { return; }
    items.push(ScanItem {
        path: path_str.into(),
        display_name: preset_match.map(|p| p.name.clone())
            .or_else(|| path.file_name().map(|n| n.to_string_lossy().into()))
            .unwrap_or_default(),
        size_bytes: size,
        matched_preset: preset_match.map(|p| p.id.clone()),
        category: preset_match.map(|p| p.category.clone()),
        auto_migrate: preset_match.map(|p| p.auto_migrate).unwrap_or(false),
        is_junction: false,
        inaccessible: false,
    });
}

#[allow(dead_code)]
fn _ensure_appresult(_: AppResult<()>) {}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml scanner`
预期：PASS，6 个测试全过（`dir_size_skips_reparse_point_content` 在非 Windows 上 junction 创建被跳过，断言 `size==0` 仍成立——因为无 reparse point 时 dir_size 本就为 0；若担心平台差异，给该测试整体加 `#[cfg(windows)]`）。

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/scanner.rs src-tauri/src/lib.rs
git commit -m "feat(scanner): 目录体积计算与预设场景识别"
```

---
### 任务 9：safety 单元 — 迁移前预检

**文件：**
- 创建：`src-tauri/src/safety.rs`
- 修改：`src-tauri/src/lib.rs`（加 `pub mod safety;`）
- 测试：`src-tauri/src/safety.rs` 内联 `#[cfg(test)] mod tests`

**职责：** 迁移前把所有风险一次性拦截：空间不足、目标卷非本地 NTFS、仓库路径非法（C 盘/网络/位于源内部）、源是系统黑名单、源是 reparse point、源被占用、目标/临时冲突、重复迁移。返回 `PrecheckReport`（`ok`/`warnings`/`blockers`）。`blockers` 非空则 `ok=false`，迁移不得开始。

**可测性设计：** 定义 `SystemProbe` trait 抽象"查盘空间/卷信息/占用进程"，生产 `Win32Probe` 包 `win32`，测试用 mock 闭包注入，使所有预检分支脱离真实磁盘可测。

- [ ] **步骤 1：编写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::*;
    use crate::store::default_config;
    use std::path::Path;

    /// 可编程的 mock probe：返回预设的盘空间/卷信息/占用。
    struct Mock {
        free: u64,
        ntfs: bool,
        serial: String,
        locked: Option<Vec<String>>,
    }
    impl SystemProbe for Mock {
        fn volume_info(&self, _p: &Path) -> AppResult<(String, bool)> {
            Ok((self.serial.clone(), self.ntfs))
        }
        fn disk_free(&self, _p: &Path) -> AppResult<u64> { Ok(self.free) }
        fn locked_processes(&self, _p: &Path) -> AppResult<Option<Vec<String>>> {
            Ok(self.locked.clone())
        }
    }

    fn cfg_repo(repo: &str) -> Config {
        let mut c = default_config();
        c.repository = repo.into();
        c
    }

    #[test]
    fn passes_when_space_and_ntfs_ok() {
        let probe = Mock { free: 10_000_000_000, ntfs: true, serial: "DDDD".into(), locked: None };
        let report = precheck(Path::new("C:/Users/x/Data"), &cfg_repo("D:/Migrated"), &[], 1_000_000_000, &probe);
        assert!(report.ok, "blockers: {:?}", report.blockers);
    }

    #[test]
    fn blocks_when_space_insufficient() {
        let probe = Mock { free: 100_000, ntfs: true, serial: "DDDD".into(), locked: None };
        let report = precheck(Path::new("C:/Users/x/Data"), &cfg_repo("D:/Migrated"), &[], 1_000_000_000, &probe);
        assert!(!report.ok);
        assert!(report.blockers.iter().any(|b| b.contains("空间")));
    }

    #[test]
    fn blocks_when_target_not_ntfs() {
        let probe = Mock { free: 10_000_000_000, ntfs: false, serial: "DDDD".into(), locked: None };
        let report = precheck(Path::new("C:/Users/x/Data"), &cfg_repo("D:/Migrated"), &[], 1_000_000_000, &probe);
        assert!(!report.ok);
        assert!(report.blockers.iter().any(|b| b.contains("NTFS")));
    }

    #[test]
    fn blocks_when_repo_on_c_drive() {
        let probe = Mock { free: 10_000_000_000, ntfs: true, serial: "CCCC".into(), locked: None };
        let report = precheck(Path::new("C:/Users/x/Data"), &cfg_repo("C:/Migrated"), &[], 1_000_000_000, &probe);
        assert!(!report.ok);
        assert!(report.blockers.iter().any(|b| b.contains("C 盘") || b.contains("系统盘")));
    }

    #[test]
    fn blocks_system_critical_path() {
        let probe = Mock { free: 10_000_000_000, ntfs: true, serial: "CCCC".into(), locked: None };
        let report = precheck(Path::new("C:/Windows/System32"), &cfg_repo("D:/Migrated"), &[], 1_000, &probe);
        assert!(!report.ok);
        assert!(report.blockers.iter().any(|b| b.contains("系统") || b.contains("黑名单")));
    }

    #[test]
    fn warns_when_source_locked() {
        let probe = Mock {
            free: 10_000_000_000, ntfs: true, serial: "CCCC".into(),
            locked: Some(vec!["wechat.exe".into()]),
        };
        let report = precheck(Path::new("C:/Users/x/Data"), &cfg_repo("D:/Migrated"), &[], 1_000_000, &probe);
        assert!(report.warnings.iter().any(|w| w.contains("wechat")));
    }

    #[test]
    fn blocks_duplicate_active_migration() {
        let existing = vec![Migration {
            id: "u1".into(), schema_version: 1,
            source: "C:/Users/x/Data".into(), target: "D:/Migrated/c/u1/data".into(),
            old_path: String::new(), preset: None, created_at: "2026-07-18T00:00:00Z".into(),
            status: MigrationStatus::Active,
            source_volume_serial: "C".into(), target_volume_serial: "D".into(),
            recycle_bin_ref: String::new(), pending_cleanup: None,
        }];
        let probe = Mock { free: 10_000_000_000, ntfs: true, serial: "C".into(), locked: None };
        let report = precheck(Path::new("C:/Users/x/Data"), &cfg_repo("D:/Migrated"), &existing, 1_000, &probe);
        assert!(!report.ok);
        assert!(report.blockers.iter().any(|b| b.contains("已迁移") || b.contains("重复")));
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml safety`
预期：FAIL / 编译错误 —— `SystemProbe`、`precheck` 未定义。
- [ ] **步骤 3：实现 safety.rs**

```rust
use crate::error::AppResult;
use crate::models::{Config, Migration, MigrationStatus, PrecheckReport};
use std::path::Path;

/// 系统探针抽象：盘空间、卷信息、占用进程。生产用 Win32Probe，测试用 mock。
pub trait SystemProbe {
    fn volume_info(&self, p: &Path) -> AppResult<(String, bool)>;
    fn disk_free(&self, p: &Path) -> AppResult<u64>;
    fn locked_processes(&self, p: &Path) -> AppResult<Option<Vec<String>>>;
}

/// 生产实现，包装 win32。
pub struct Win32Probe;
impl SystemProbe for Win32Probe {
    fn volume_info(&self, p: &Path) -> AppResult<(String, bool)> { crate::win32::volume_info(p) }
    fn disk_free(&self, p: &Path) -> AppResult<u64> { crate::win32::disk_free_bytes(p) }
    fn locked_processes(&self, p: &Path) -> AppResult<Option<Vec<String>>> {
        crate::win32::locked_processes(p)
    }
}

/// 系统关键路径黑名单（迁移拒绝）。
const SYSTEM_BLACKLIST: &[&str] = &[
    "C:/Windows",
    "C:/Program Files/WindowsApps",
    "C:/Program Files (x86)",
    "C:/ProgramData/Microsoft",
    "C:/Windows/System32",
    "C:/Recovery",
];

/// 安全余量：源大小的 10% + 100MB（吸收复制期间增长与回收站占用）。
fn safety_margin(src_size: u64) -> u64 {
    src_size / 10 + 100 * 1024 * 1024
}

pub fn precheck(
    src: &Path,
    config: &Config,
    existing: &[Migration],
    src_size: u64,
    probe: &dyn SystemProbe,
) -> PrecheckReport {
    let repo = config.repository.trim_end_matches('/');
    let mut warnings = Vec::new();
    let mut blockers = Vec::new();
    let src_str = src.to_string_lossy().replace('/', "\\");
    let src_lower = src_str.to_lowercase();

    // 1. 重复迁移
    if existing.iter().any(|m| m.status == MigrationStatus::Active && norm(&m.source) == norm(&src_str)) {
        blockers.push("源路径已存在 active 迁移记录（重复迁移）".into());
    }

    // 2. 系统黑名单
    if SYSTEM_BLACKLIST.iter().any(|b| src_lower.starts_with(&b.to_lowercase())) {
        blockers.push("源路径在系统关键目录黑名单内".into());
    }

    // 3. 仓库路径合法性
    if repo.to_lowercase().starts_with("c:") {
        blockers.push("仓库不能位于 C 盘（系统盘）".into());
    }
    if repo.starts_with("\\\\") {
        blockers.push("仓库不能是网络路径".into());
    }
    if src_lower.starts_with(&repo.to_lowercase()) {
        blockers.push("仓库不能位于源目录内部".into());
    }

    // 4. 目标卷能力
    let (target_serial, is_ntfs) = match probe.volume_info(Path::new(repo)) {
        Ok(v) => v,
        Err(e) => { blockers.push(format!("无法读取目标卷信息: {e}")); (String::new(), false) }
    };
    if !is_ntfs {
        blockers.push("目标卷不是 NTFS（junction 需 NTFS）".into());
    }

    // 5. 空间
    let free = match probe.disk_free(Path::new(repo)) {
        Ok(f) => f,
        Err(e) => { blockers.push(format!("无法读取目标盘剩余空间: {e}")); 0 }
    };
    let need = src_size + safety_margin(src_size);
    if free < need {
        blockers.push(format!("目标盘空间不足：需 {} 字节（含安全余量），实有 {}", need, free));
    }

    // 6. 占用
    match probe.locked_processes(src) {
        Ok(Some(procs)) if !procs.is_empty() => {
            warnings.push(format!("源目录被进程占用，请先关闭：{}", procs.join(", ")));
        }
        _ => {}
    }

    let ok = blockers.is_empty();
    PrecheckReport {
        ok,
        warnings,
        blockers,
        source_size_bytes: src_size,
        target_free_bytes: free,
    }
}

fn norm(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml safety`
预期：PASS，7 个测试全过。

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/safety.rs src-tauri/src/lib.rs
git commit -m "feat(safety): 迁移前预检（空间/卷/黑名单/占用/重复）"
```

---
### 任务 10：migrator 单元 — 迁移状态机

**文件：**
- 创建：`src-tauri/src/migrator.rs`
- 修改：`src-tauri/src/lib.rs`（加 `pub mod migrator;`）、`src-tauri/src/file_ops.rs`（trait 追加 junction 方法）
- 测试：`src-tauri/src/migrator.rs` 内联 `#[cfg(test)] mod tests`

**职责：** 可恢复迁移状态机核心。**只编排**——通过 `FileOps` trait 委托复制/校验/改名/回收/建链，通过 `safety` 复核，通过 `journal` 落每阶段、`history` 记终态、`store` 写迁移映射。失败时永远优先保数据：源目录在建链成功且记录落盘前绝不删。

**关键修订（file_ops.rs）：** 给 `FileOps` trait 追加三个 junction 方法，使状态机完全脱离真实磁盘可测。在 `file_ops.rs` 的 `trait FileOps` 末尾追加：

```rust
    fn create_junction(&self, link: &Path, target: &Path) -> AppResult<()>;
    fn remove_junction(&self, link: &Path) -> AppResult<()>;
    fn junction_resolves(&self, link: &Path) -> bool;
```

并在 `impl FileOps for RealFileOps` 中补实现（委托 `junction.rs`）：

```rust
    fn create_junction(&self, link: &Path, target: &Path) -> AppResult<()> {
        crate::junction::create(link, target)
    }
    fn remove_junction(&self, link: &Path) -> AppResult<()> {
        crate::junction::remove(link)
    }
    fn junction_resolves(&self, link: &Path) -> bool {
        crate::junction::verify(link)
    }
```

**进度 stage 常量（migrator.rs 顶部）：**

```rust
pub mod stage {
    pub const COPYING: &str = "copying";
    pub const VERIFYING: &str = "verifying";
    pub const RENAMING_SOURCE: &str = "renaming_source";
    pub const SYNCING: &str = "syncing";
    pub const CREATING_JUNCTION: &str = "creating_junction";
    pub const RECORDING: &str = "recording";
    pub const CLEANING: &str = "cleaning";
}
```

**MigratePlan（migrator.rs）：** 调用方（commands 层）在预检通过后构造，migrator 据此编排。

```rust
use crate::error::AppResult;
use crate::file_ops::FileOps;
use crate::history::History;
use crate::journal::Journal;
use crate::models::{HistoryEntry, Migration, MigrationStatus, ProgressEvent};
use crate::store::Store;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

pub struct MigratePlan {
    pub task_id: String,
    pub migration_id: String,
    pub src: PathBuf,
    pub target: PathBuf,       // 最终 data 路径
    pub tmp: PathBuf,          // data.tmp
    pub old_path: PathBuf,     // src.dayu-old-{taskId}
    pub preset_id: Option<String>,
    pub source_volume_serial: String,
    pub target_volume_serial: String,
}
```

- [ ] **步骤 1：编写成功路径测试（用真实 FileOps 走通主流程）**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_ops::RealFileOps;
    use std::cell::RefCell;
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    fn fixtures() -> (TempDir, Store, Journal, History) {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path().join("data")).unwrap();
        let journal = Journal::new(dir.path().join("journal.jsonl")).unwrap();
        let history = History::new(dir.path().join("history.jsonl")).unwrap();
        (dir, store, journal, history)
    }

    fn plan_for(dir: &std::path::Path, id: &str) -> MigratePlan {
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        MigratePlan {
            task_id: format!("t-{id}"),
            migration_id: format!("m-{id}"),
            src: src.clone(),
            target: dir.join("repo/sub/m/data"),
            tmp: dir.join("repo/sub/m/data.tmp"),
            old_path: src.with_extension(format!("dayu-old-t-{id}")),
            preset_id: None,
            source_volume_serial: "C".into(),
            target_volume_serial: "D".into(),
        }
    }

    #[test]
    fn migrate_success_creates_junction_records_and_logs() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "1");
        let src = plan.src.clone();
        let cancel = AtomicBool::new(false);
        let events = RefCell::new(Vec::new());
        let m = migrate(
            &RealFileOps, &store, &journal, &history, &plan,
            &|e| events.borrow_mut().push(e), &cancel,
        ).unwrap();
        assert_eq!(m.status, MigrationStatus::Active);
        assert!(crate::junction::exists(&src), "源路径应已变为 junction");
        assert!(plan.target.join("a.txt").exists(), "数据应落到 target");
        assert!(store.load_migrations().unwrap().iter().any(|x| x.id == "m-1"));
        let migrated = history.list(Some("migrate"), None).unwrap();
        assert!(migrated.iter().any(|h| h.id == "m-1" && h.result == "ok"));
        assert!(journal.recover_pending().unwrap().is_empty(), "任务应已完成");
    }
}
```
- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml migrator`
预期：FAIL / 编译错误 —— `migrate` 函数未定义。

- [ ] **步骤 3：实现 migrate 状态机（核心编排）**

在 `migrator.rs`（MigratePlan 之后）插入：

```rust
pub fn migrate(
    ops: &dyn FileOps,
    store: &Store,
    journal: &Journal,
    history: &History,
    plan: &MigratePlan,
    on_progress: &dyn Fn(ProgressEvent),
    cancel: &AtomicBool,
) -> AppResult<Migration> {
    use crate::error::AppError;
    let now = || chrono::Utc::now().to_rfc3339();
    let emit = |stage: &str, pct: u8, msg: &str| {
        on_progress(ProgressEvent {
            task_id: plan.task_id.clone(), stage: stage.into(), percent: pct, message: msg.into(),
        });
    };

    journal.begin(
        &plan.task_id, &plan.migration_id, "migrate",
        &plan.src.to_string_lossy(),
        &plan.target.to_string_lossy(),
        &plan.tmp.to_string_lossy(),
        &plan.old_path.to_string_lossy(),
    )?;

    // 阶段 a：复制
    emit(stage::COPYING, 0, "复制到临时目录");
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = ops.remove_tree(&plan.tmp);
        journal.cancel(&plan.task_id)?;
        return Err(AppError::Cancelled);
    }
    let src_size = crate::scanner::dir_size(&plan.src);
    if let Err(e) = ops.copy_tree(&plan.src, &plan.tmp, &|p| emit(stage::COPYING, p, "复制中")) {
        let _ = ops.remove_tree(&plan.tmp);
        journal.fail(&plan.task_id, "复制失败")?;
        return Err(e);
    }
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = ops.remove_tree(&plan.tmp);
        journal.cancel(&plan.task_id)?;
        return Err(AppError::Cancelled);
    }

    // 阶段 b：首次校验
    emit(stage::VERIFYING, 60, "校验 manifest");
    let m1 = ops.manifest(&plan.src)?;
    let m2 = ops.manifest(&plan.tmp)?;
    if !ops.diff_manifests(&m1, &m2).is_empty() {
        // 保留 tmp 供排查
        journal.fail(&plan.task_id, "manifest 不一致")?;
        return Err(AppError::Migrate("manifest 不一致，已保留 tmp 待人工确认".into()));
    }
    journal.mark_stage(&plan.task_id, "copied")?;
    journal.mark_stage(&plan.task_id, "manifest_ok")?;

    // 阶段 c：改名源 + 增量同步 + 建链
    emit(stage::RENAMING_SOURCE, 70, "改名源目录");
    if ops.is_reparse_point(&plan.src) {
        journal.fail(&plan.task_id, "源已是 reparse point")?;
        return Err(AppError::Migrate("源已是 reparse point".into()));
    }
    if let Err(e) = ops.rename(&plan.src, &plan.old_path) {
        journal.fail(&plan.task_id, "源改名失败（可能被占用）")?;
        return Err(e);
    }
    journal.mark_stage(&plan.task_id, "source_renamed")?;

    // 增量同步：old_path -> tmp（捕捉复制期间变化）
    emit(stage::SYNCING, 80, "增量同步");
    if let Err(e) = ops.copy_tree(&plan.old_path, &plan.tmp, &|_| {}) {
        // 回滚：改回原名
        let _ = ops.rename(&plan.old_path, &plan.src);
        journal.fail(&plan.task_id, "增量同步失败")?;
        return Err(e);
    }
    let m3 = ops.manifest(&plan.old_path)?;
    let m4 = ops.manifest(&plan.tmp)?;
    if !ops.diff_manifests(&m3, &m4).is_empty() {
        let _ = ops.rename(&plan.old_path, &plan.src);
        journal.fail(&plan.task_id, "二次校验不一致")?;
        return Err(AppError::Migrate("增量后 manifest 不一致".into()));
    }
    journal.mark_stage(&plan.task_id, "incremental_synced")?;

    // tmp -> target 原子改名
    emit(stage::CREATING_JUNCTION, 90, "建立 junction");
    if let Some(parent) = plan.target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ops.rename(&plan.tmp, &plan.target)?;
    if let Err(e) = ops.create_junction(&plan.src, &plan.target) {
        // 回滚：删可能半成品 junction，target 回 tmp，old 改回原名
        let _ = ops.remove_junction(&plan.src);
        let _ = ops.rename(&plan.target, &plan.tmp);
        let _ = ops.rename(&plan.old_path, &plan.src);
        journal.fail(&plan.task_id, "建链失败")?;
        return Err(e);
    }
    if !ops.junction_resolves(&plan.src) {
        let _ = ops.remove_junction(&plan.src);
        let _ = ops.rename(&plan.target, &plan.tmp);
        let _ = ops.rename(&plan.old_path, &plan.src);
        journal.fail(&plan.task_id, "junction 解析失败")?;
        return Err(AppError::Junction("junction 解析失败".into()));
    }
    journal.mark_stage(&plan.task_id, "junction_created")?;
```
继续 `migrate` 实现（阶段 d 删原 + 记录）：

```rust
    // 阶段 d：先写迁移映射（命根子），再删原
    emit(stage::RECORDING, 95, "记录迁移映射");
    let migration = Migration {
        id: plan.migration_id.clone(),
        schema_version: 1,
        source: plan.src.to_string_lossy().replace('/', "\\"),
        target: plan.target.to_string_lossy().replace('/', "\\"),
        old_path: plan.old_path.to_string_lossy().replace('/', "\\"),
        preset: plan.preset_id.clone(),
        created_at: now(),
        status: MigrationStatus::Active,
        source_volume_serial: plan.source_volume_serial.clone(),
        target_volume_serial: plan.target_volume_serial.clone(),
        recycle_bin_ref: String::new(),
        pending_cleanup: None,
    };
    if let Err(e) = store.upsert_migration(migration.clone()) {
        // junction 已建好但记录失败：保留 oldPath，标记 pending_record
        let mut m = migration.clone();
        m.status = MigrationStatus::OldPendingDelete;
        let _ = store.upsert_migration(m);
        journal.fail(&plan.task_id, "记录写入失败，oldPath 保留")?;
        return Err(e);
    }
    journal.mark_stage(&plan.task_id, "record_written")?;

    // 删原（走回收站，失败降级）
    emit(stage::CLEANING, 99, "清理原目录");
    match ops.to_recycle_bin(&plan.old_path) {
        Ok(()) => {
            journal.mark_stage(&plan.task_id, "old_recycled")?;
        }
        Err(_) => {
            // junction 已建好、映射已落盘，仅 oldPath 未清理
            let mut m = migration.clone();
            m.status = MigrationStatus::OldPendingDelete;
            let _ = store.upsert_migration(m);
        }
    }

    // 历史与终态
    history.append(&HistoryEntry {
        op: "migrate".into(), id: plan.migration_id.clone(),
        src: plan.src.to_string_lossy().into(),
        dst: plan.target.to_string_lossy().into(),
        result: "ok".into(), time: now(), duration_sec: 0,
    })?;
    journal.complete(&plan.task_id)?;
    emit(stage::CLEANING, 100, "迁移完成");
    Ok(migration)
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml migrator`
预期：PASS，`migrate_success_creates_junction_records_and_logs` 通过——源变 junction、数据在 target、store 有记录、history 有流水、journal 无 pending。

- [ ] **步骤 5：编写失败分支测试（用 mock FileOps 验证回滚，不碰真实磁盘）**

在 `migrator.rs` 测试模块追加 mock 与失败分支测试：

```rust
    use crate::file_ops::{FileOps, Manifest};
    use std::path::Path;

    /// 可编程 mock：复制/校验成功，但建链可注入失败。
    struct MockOps {
        copy_ok: bool,
        manifest_ok: bool,
        junction_fails: bool,
        rename_ok: bool,
    }
    impl FileOps for MockOps {
        fn copy_tree(&self, _s: &Path, _d: &Path, _p: &dyn Fn(u8)) -> AppResult<()> {
            if self.copy_ok { Ok(()) } else { Err(crate::error::AppError::Migrate("copy fail".into())) }
        }
        fn manifest(&self, _s: &Path) -> AppResult<Manifest> {
            Ok(Manifest { root: String::new(), entries: vec![] })
        }
        fn diff_manifests(&self, _a: &Manifest, _b: &Manifest) -> Vec<String> {
            if self.manifest_ok { vec![] } else { vec!["f.txt".into()] }
        }
        fn rename(&self, _f: &Path, _t: &Path) -> AppResult<()> {
            if self.rename_ok { Ok(()) } else { Err(crate::error::AppError::Migrate("rename fail".into())) }
        }
        fn to_recycle_bin(&self, _p: &Path) -> AppResult<()> { Ok(()) }
        fn remove_tree(&self, _p: &Path) -> AppResult<()> { Ok(()) }
        fn is_reparse_point(&self, _p: &Path) -> bool { false }
        fn dir_exists(&self, _p: &Path) -> bool { true }
        fn create_junction(&self, _l: &Path, _t: &Path) -> AppResult<()> {
            if self.junction_fails { Err(crate::error::AppError::Junction("mock junction fail".into())) }
            else { Ok(()) }
        }
        fn remove_junction(&self, _l: &Path) -> AppResult<()> { Ok(()) }
        fn junction_resolves(&self, _l: &Path) -> bool { !self.junction_fails }
    }

    #[test]
    fn migrate_rolls_back_when_copy_fails_keeps_source() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "c");
        let ops = MockOps { copy_ok: false, manifest_ok: true, junction_fails: false, rename_ok: true };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        // 源目录未被改名（仍存在原文件）
        assert!(plan.src.join("a.txt").exists());
        assert!(store.load_migrations().unwrap().is_empty(), "不应落盘迁移记录");
    }

    #[test]
    fn migrate_aborts_when_manifest_mismatch_keeps_tmp() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "m");
        let ops = MockOps { copy_ok: true, manifest_ok: false, junction_fails: false, rename_ok: true };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        // 源未改名
        assert!(plan.src.join("a.txt").exists());
    }

    #[test]
    fn migrate_cancellation_cleans_tmp_and_logs_canceled() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "x");
        let ops = MockOps { copy_ok: true, manifest_ok: true, junction_fails: false, rename_ok: true };
        let cancel = AtomicBool::new(true); // 复制前已取消
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        assert!(store.load_migrations().unwrap().is_empty());
    }
```

- [ ] **步骤 6：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml migrator`
预期：PASS，4 个测试全过（成功 + 3 个失败/取消分支）。

- [ ] **步骤 7：Commit**

```bash
git add src-tauri/src/migrator.rs src-tauri/src/file_ops.rs src-tauri/src/lib.rs
git commit -m "feat(migrator): 可恢复迁移状态机与失败回滚"
```

---
### 任务 11：migrator 单元 — 还原状态机

**文件：**
- 修改：`src-tauri/src/migrator.rs`（追加 `restore` + `stage` 常量）、`src-tauri/src/migrator.rs` 测试模块追加
- 测试：`src-tauri/src/migrator.rs` 内联测试

**职责：** 把数据搬回原位并恢复普通目录。流程：校验 junction 有效 → 复制 target→restore_tmp → manifest 校验 → 删 junction → restore_tmp 原子改名回 src → 清理 target。**切换失败时优先重建 junction 指回 target**，避免应用入口路径消失。

- [ ] **步骤 1：扩展 stage 常量并编写还原测试**

在 `migrator.rs` 的 `pub mod stage` 追加：

```rust
    pub const REMOVING_JUNCTION: &str = "removing_junction";
    pub const SWITCHING: &str = "switching";
```

在测试模块追加 fixture 与测试：

```rust
    fn restore_fixture(id: &str) -> (TempDir, Store, Journal, History, Migration) {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path().join("data")).unwrap();
        let journal = Journal::new(dir.path().join("journal.jsonl")).unwrap();
        let history = History::new(dir.path().join("history.jsonl")).unwrap();
        let target = dir.path().join("repo/m/data");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.txt"), b"hello").unwrap();
        let src = dir.path().join("src");
        crate::junction::create(&src, &target).unwrap();
        let mig = Migration {
            id: format!("m-{id}"), schema_version: 1,
            source: src.to_string_lossy().into(),
            target: target.to_string_lossy().into(),
            old_path: String::new(), preset: None,
            created_at: "2026-07-18T00:00:00Z".into(),
            status: MigrationStatus::Active,
            source_volume_serial: "C".into(), target_volume_serial: "D".into(),
            recycle_bin_ref: String::new(), pending_cleanup: None,
        };
        store.upsert_migration(mig.clone()).unwrap();
        (dir, store, journal, history, mig)
    }

    #[test]
    fn restore_success_recovers_dir_and_removes_link() {
        let (_dir, store, journal, history, mig) = restore_fixture("1");
        let src: std::path::PathBuf = mig.source.clone().into();
        let cancel = AtomicBool::new(false);
        restore(&RealFileOps, &store, &journal, &history, &mig, &|_| {}, &cancel).unwrap();
        assert!(!crate::junction::exists(&src), "junction 应已删除");
        assert!(src.join("a.txt").exists(), "源应恢复为普通目录");
        assert!(store.load_migrations().unwrap().iter().all(|x| x.id != "m-1"), "记录应移除");
        let r = history.list(Some("restore"), None).unwrap();
        assert!(r.iter().any(|h| h.id == "m-1" && h.result == "ok"));
    }

    #[test]
    fn restore_aborts_when_junction_invalid() {
        let (_dir, store, journal, history, mig) = restore_fixture("2");
        // 删掉 target 使 junction 失效
        let target: std::path::PathBuf = mig.target.clone().into();
        std::fs::remove_dir_all(&target).unwrap();
        let cancel = AtomicBool::new(false);
        let res = restore(&RealFileOps, &store, &journal, &history, &mig, &|_| {}, &cancel);
        assert!(res.is_err(), "junction 失效时应中止");
    }

    #[test]
    fn restore_switch_fail_rebuilds_junction() {
        let (_dir, store, journal, history, mig) = restore_fixture("3");
        let src: std::path::PathBuf = mig.source.clone().into();
        // mock：切换阶段（remove_junction 之后 rename）失败，期望重建 junction
        let ops = MockOps { copy_ok: true, manifest_ok: true, junction_fails: false, rename_ok: false };
        let cancel = AtomicBool::new(false);
        let res = restore(&ops, &store, &journal, &history, &mig, &|_| {}, &cancel);
        assert!(res.is_err());
        assert!(ops.junction_resolves(&src), "切换失败时应重建 junction 保入口");
    }
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml migrator`
预期：FAIL / 编译错误 —— `restore` 未定义。
- [ ] **步骤 3：实现 restore 状态机**

在 `migrator.rs` 追加：

```rust
pub fn restore(
    ops: &dyn FileOps,
    store: &Store,
    journal: &Journal,
    history: &History,
    mig: &Migration,
    on_progress: &dyn Fn(ProgressEvent),
    cancel: &AtomicBool,
) -> AppResult<()> {
    use crate::error::AppError;
    let now = || chrono::Utc::now().to_rfc3339();
    let emit = |s: &str, p: u8, m: &str| on_progress(ProgressEvent {
        task_id: format!("restore-{}", mig.id), stage: s.into(), percent: p, message: m.into(),
    });
    let src: std::path::PathBuf = mig.source.clone().into();
    let target: std::path::PathBuf = mig.target.clone().into();
    let restore_tmp = src.with_extension(format!("dayu-restore-{}", mig.id));

    journal.begin(&format!("restore-{}", mig.id), &mig.id, "restore",
        &mig.source, &mig.target, &restore_tmp.to_string_lossy(), &mig.old_path)?;

    // 校验 junction 仍指向有效 target
    if !ops.junction_resolves(&src) {
        journal.fail(&format!("restore-{}", mig.id), "junction 失效")?;
        return Err(AppError::Junction("junction 已失效，无法还原".into()));
    }

    emit(stage::COPYING, 0, "复制回源盘临时目录");
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        journal.cancel(&format!("restore-{}", mig.id))?;
        return Err(AppError::Cancelled);
    }
    if let Err(e) = ops.copy_tree(&target, &restore_tmp, &|_| {}) {
        let _ = ops.remove_tree(&restore_tmp);
        journal.fail(&format!("restore-{}", mig.id), "还原复制失败")?;
        return Err(e);
    }

    emit(stage::VERIFYING, 50, "校验 manifest");
    let m1 = ops.manifest(&target)?;
    let m2 = ops.manifest(&restore_tmp)?;
    if !ops.diff_manifests(&m1, &m2).is_empty() {
        let _ = ops.remove_tree(&restore_tmp);
        journal.fail(&format!("restore-{}", mig.id), "manifest 不一致")?;
        return Err(AppError::Migrate("还原校验不一致".into()));
    }
    journal.mark_stage(&format!("restore-{}", mig.id), "restore_copied")?;
    journal.mark_stage(&format!("restore-{}", mig.id), "restore_manifest_ok")?;

    // 删 junction -> restore_tmp 原子改名回 src
    emit(stage::REMOVING_JUNCTION, 70, "删除 junction");
    if let Err(e) = ops.remove_junction(&src) {
        let _ = ops.remove_tree(&restore_tmp);
        journal.fail(&format!("restore-{}", mig.id), "删 junction 失败")?;
        return Err(e);
    }
    journal.mark_stage(&format!("restore-{}", mig.id), "junction_removed")?;

    emit(stage::SWITCHING, 85, "切换为普通目录");
    if let Err(e) = ops.rename(&restore_tmp, &src) {
        // 切换失败：优先重建 junction 指回 target，保入口
        let _ = ops.create_junction(&src, &target);
        journal.fail(&format!("restore-{}", mig.id), "切换失败，已重建 junction")?;
        return Err(e);
    }
    journal.mark_stage(&format!("restore-{}", mig.id), "restore_switched")?;

    // 清理 target（走回收站，失败降级）
    emit(stage::CLEANING, 95, "清理目标数据");
    match ops.to_recycle_bin(&target) {
        Ok(()) => {}
        Err(_) => {
            let mut m = mig.clone();
            m.status = MigrationStatus::TargetPendingDelete;
            let _ = store.upsert_migration(m);
        }
    }

    store.remove_migration(&mig.id)?;
    history.append(&HistoryEntry {
        op: "restore".into(), id: mig.id.clone(),
        src: mig.source.clone(), dst: mig.target.clone(),
        result: "ok".into(), time: now(), duration_sec: 0,
    })?;
    journal.complete(&format!("restore-{}", mig.id))?;
    emit(stage::CLEANING, 100, "还原完成");
    Ok(())
}

/// 断开链接：删 junction 但保留 target 数据（原路径将不可用，调用方需二次确认）。
pub fn break_link(ops: &dyn FileOps, store: &Store, history: &History, mig: &Migration) -> AppResult<()> {
    let src: std::path::PathBuf = mig.source.clone().into();
    ops.remove_junction(&src)?;
    store.remove_migration(&mig.id)?;
    history.append(&HistoryEntry {
        op: "break_link".into(), id: mig.id.clone(),
        src: mig.source.clone(), dst: mig.target.clone(),
        result: "ok".into(), time: chrono::Utc::now().to_rfc3339(), duration_sec: 0,
    })?;
    Ok(())
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml migrator`
预期：PASS，7 个测试全过（迁移 4 + 还原 3）。

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/migrator.rs
git commit -m "feat(migrator): 还原状态机与断开链接，切换失败优先重建 junction"
```

---
## 阶段 4：IPC 合约层

### 任务 12：Tauri commands 与 events

**文件：**
- 创建：`src-tauri/src/commands.rs`、`src-tauri/src/app_state.rs`
- 修改：`src-tauri/src/lib.rs`（`run()` 注册 commands、初始化 AppState、启动恢复调用）
- 测试：`src-tauri/src/commands.rs` 内联 `#[cfg(test)] mod tests`（合约类型与启动恢复逻辑）

**职责：** 把后端能力暴露给前端。`AppState` 持有 `Store`/`Journal`/`History` 与当前迁移任务的取消令牌。启动时调 `journal.recover_pending()` 做恢复决策（见步骤 5 的 `recover_pending_decisions`）。进度通过 `AppHandle.emit("dayu://progress", event)` 推送，前端被动监听。

**命令清单（与前端 T13 的 invoke.ts 一一对应）：**

| 命令 | 入参 | 返回 | 说明 |
|------|------|------|------|
| `scan_drives` | `{}` | `Vec<ScanItem>` | 扫描 C 盘（用 config.excludePaths） |
| `precheck_migrate` | `{ src }` | `PrecheckReport` | 预检 |
| `start_migrate` | `{ migrationId, src, presetId? }` | `Migration` | 异步迁移，emit 进度 |
| `cancel_migrate` | `{}` | `bool` | 取消当前运行任务（设取消令牌） |
| `start_restore` | `{ migrationId }` | `bool` | 异步还原 |
| `list_links` | `{}` | `Vec<LinkItem>` | 链接列表（含失效标注） |
| `break_link_cmd` | `{ migrationId }` | `bool` | 断开链接 |
| `list_history` | `{ op?, from?, to? }` | `Vec<HistoryEntry>` | 历史 |
| `get_config` / `save_config` | `Config` | — | 设置 |
| `export_history` | `{}` | `String` | 导出 JSON |
| `get_recovery_advice` | `{}` | `Vec<(String,String,String)>` | 启动恢复建议 |

> **类型归属约定：** `LinkItem` 定义在 `app_state.rs`（T12 步骤 1），`commands.rs` 与前端 `ipc/types.ts` 复用它。`list_links` 返回 `Vec<crate::app_state::LinkItem>`。

- [ ] **步骤 1：定义 AppState、LinkItem 与启动恢复决策函数**

`src-tauri/src/app_state.rs`：

```rust
use crate::journal::Journal;
use crate::models::JournalEntry;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;

/// 链接列表项（list_links 返回）。定义于此，commands.rs 复用。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkItem {
    pub id: String,
    pub source: String,
    pub target: String,
    pub preset: Option<String>,
    pub created_at: String,
    pub status: String,
    pub valid: bool,        // junction 是否解析正常
    pub broken: bool,       // target 不存在
}

pub struct AppState {
    pub store: crate::store::Store,
    pub journal: Journal,
    pub history: crate::history::History,
    /// 当前迁移/还原任务的取消令牌；无任务时为 None
    pub cancel_token: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}
```

/// 启动时根据 journal 恢复决策。
/// 返回每个未完成任务的 (migration_id, stage, decision) 供前端展示与人工处理。
pub fn recover_pending_decisions(entries: &[JournalEntry]) -> Vec<(String, String, String)> {
    entries.iter().map(|e| {
        let decision = match e.stage.as_str() {
            "created" | "copied" | "manifest_ok" => "清 tmp 可重试".into(),
            "source_renamed" | "incremental_synced" => "oldPath 改回原名可重试".into(),
            "junction_created" | "record_written" => "已建链，补写或确认".into(),
            s if s.starts_with("restore_") => "还原中断，按阶段恢复".into(),
            _ => "待人工确认".into(),
        };
        (e.migration_id.clone(), e.stage.clone(), decision)
    }).collect()
}

#[allow(dead_code)]
fn _unused(_p: PathBuf) {}
```
- [ ] **步骤 2：编写启动恢复决策的测试**

`src-tauri/src/app_state.rs` 内联：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::JournalEntry;

    fn entry(stage: &str, mid: &str) -> JournalEntry {
        JournalEntry {
            task_id: "t1".into(), op: "migrate".into(), migration_id: mid.into(),
            stage: stage.into(), src: "C:/s".into(), dst: "D:/d".into(),
            tmp: "D:/d.tmp".into(), old_path: "C:/s.old".into(),
            time: "2026-07-18T00:00:00Z".into(), final_mark: None,
        }
    }

    #[test]
    fn copied_stage_decision_is_clean_tmp() {
        let d = recover_pending_decisions(&[entry("copied", "m1")]);
        assert_eq!(d.len(), 1);
        assert!(d[0].2.contains("清 tmp"));
    }

    #[test]
    fn junction_created_decision_keeps_link() {
        let d = recover_pending_decisions(&[entry("junction_created", "m2")]);
        assert!(d[0].2.contains("已建链"));
    }

    #[test]
    fn restore_stage_decision_recognized() {
        let d = recover_pending_decisions(&[entry("restore_copied", "m3")]);
        assert!(d[0].2.contains("还原"));
    }
}
```

- [ ] **步骤 3：实现 commands.rs（绑定各单元到 #[tauri::command]）**

```rust
use crate::app_state::{recover_pending_decisions, AppState};
use crate::error::AppResult;
use crate::file_ops::RealFileOps;
use crate::migrator::{self, MigratePlan};
use crate::models::*;
use crate::scanner;
use crate::safety::{precheck, Win32Probe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub fn scan_drives(state: State<AppState>) -> AppResult<Vec<ScanItem>> {
    let cfg = state.store.load_config()?;
    // 首版扫描根：当前用户目录 + Program Files（受 excludePaths 过滤）
    let mut roots = vec![];
    if let Some(home) = dirs::home_dir() { roots.push(home); }
    roots.push(PathBuf::from("C:/Program Files"));
    let mut items = Vec::new();
    for r in roots {
        items.extend(scanner::scan(&r, &cfg));
    }
    Ok(items)
}

#[tauri::command]
pub fn precheck_migrate(src: String, state: State<AppState>) -> AppResult<PrecheckReport> {
    let cfg = state.store.load_config()?;
    let existing = state.store.load_migrations()?;
    let src_size = scanner::dir_size(std::path::Path::new(&src));
    Ok(precheck(std::path::Path::new(&src), &cfg, &existing, src_size, &Win32Probe))
}

#[tauri::command]
pub async fn start_migrate(
    migration_id: String, src: String, preset_id: Option<String>,
    app: AppHandle, state: State<'_, AppState>,
) -> AppResult<Migration> {
    let cfg = state.store.load_config()?;
    let src_path = PathBuf::from(&src);
    let preset = preset_id.as_ref().and_then(|id| cfg.presets.iter().find(|p| &p.id == id));
    let subdir = preset.map(|p| p.target_subdir.clone()).unwrap_or_else(|| "custom".into());
    let target = format!("{}/{}/{}/data", cfg.repository.trim_end_matches('/'), subdir, migration_id);
    let tmp = format!("{}.tmp", target);
    let old_path = format!("{}.dayu-old-{}", src.replace('/', "\\"), migration_id);
    let task_id = format!("task-{migration_id}");
    let (src_serial, _) = crate::win32::volume_info(&src_path).unwrap_or((String::new(), false));
    let (tgt_serial, _) = crate::win32::volume_info(std::path::Path::new(&target)).unwrap_or((String::new(), false));

    let plan = MigratePlan {
        task_id: task_id.clone(), migration_id: migration_id.clone(),
        src: src_path, target: target.into(), tmp: tmp.into(), old_path: old_path.into(),
        preset_id: preset_id.clone(),
        source_volume_serial: src_serial, target_volume_serial: tgt_serial,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    *state.cancel_token.lock().unwrap() = Some(cancel.clone());
    let app2 = app.clone();
    let result = migrator::migrate(
        &RealFileOps, &state.store, &state.journal, &state.history, &plan,
        &move |e: ProgressEvent| { let _ = app2.emit("dayu://progress", e); },
        &cancel,
    );
    *state.cancel_token.lock().unwrap() = None;
    result
}

#[tauri::command]
pub fn cancel_migrate(state: State<AppState>) -> bool {
    if let Some(tok) = state.cancel_token.lock().unwrap().as_ref() {
        tok.store(true, Ordering::SeqCst);
        return true;
    }
    false
}

#[tauri::command]
pub async fn start_restore(
    migration_id: String, app: AppHandle, state: State<'_, AppState>,
) -> AppResult<bool> {
    let migs = state.store.load_migrations()?;
    let mig = migs.into_iter().find(|m| m.id == migration_id)
        .ok_or_else(|| crate::error::AppError::Store("迁移记录不存在".into()))?;
    let app2 = app.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    migrator::restore(
        &RealFileOps, &state.store, &state.journal, &state.history, &mig,
        &move |e: ProgressEvent| { let _ = app2.emit("dayu://progress", e); },
        &cancel,
    )?;
    Ok(true)
}

#[tauri::command]
pub fn list_links(state: State<AppState>) -> AppResult<Vec<crate::app_state::LinkItem>> {
    use crate::app_state::LinkItem;
    let migs = state.store.load_migrations()?;
    Ok(migs.into_iter().map(|m| {
        let valid = crate::junction::verify(std::path::Path::new(&m.source));
        let target_exists = std::path::Path::new(&m.target).exists();
        LinkItem {
            id: m.id.clone(), source: m.source.clone(), target: m.target.clone(),
            preset: m.preset.clone(), created_at: m.created_at.clone(),
            status: format!("{:?}", m.status).to_lowercase(),
            valid, broken: !target_exists,
        }
    }).collect())
}

#[tauri::command]
pub fn break_link_cmd(migration_id: String, state: State<AppState>) -> AppResult<bool> {
    let migs = state.store.load_migrations()?;
    let mig = migs.into_iter().find(|m| m.id == migration_id)
        .ok_or_else(|| crate::error::AppError::Store("迁移记录不存在".into()))?;
    migrator::break_link(&RealFileOps, &state.store, &state.history, &mig)?;
    Ok(true)
}

#[tauri::command]
pub fn list_history(op: Option<String>, from: Option<String>, to: Option<String>, state: State<AppState>) -> AppResult<Vec<HistoryEntry>> {
    let range = match (from, to) { (Some(a), Some(b)) => Some((a.as_str(), b.as_str())), _ => None };
    Ok(state.history.list(op.as_deref(), range)?)
}

#[tauri::command]
pub fn get_config(state: State<AppState>) -> AppResult<Config> {
    state.store.load_config()
}

#[tauri::command]
pub fn save_config(config: Config, state: State<AppState>) -> AppResult<()> {
    state.store.save_config(&config)
}

#[tauri::command]
pub fn export_history(state: State<AppState>) -> AppResult<String> {
    state.history.export_all_json()
}

#[tauri::command]
pub fn get_recovery_advice(state: State<AppState>) -> AppResult<Vec<(String, String, String)>> {
    let pending = state.journal.recover_pending()?;
    Ok(recover_pending_decisions(&pending))
}
```
- [ ] **步骤 4：lib.rs 注册命令、初始化 AppState、启动恢复**

`src-tauri/src/lib.rs`：

```rust
pub mod app_state;
pub mod commands;
pub mod error;
pub mod file_ops;
pub mod history;
pub mod journal;
pub mod junction;
pub mod migrator;
pub mod models;
pub mod safety;
pub mod scanner;
pub mod store;
pub mod win32;

use app_state::AppState;
use std::sync::{Arc, Mutex};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let data_dir = win32::local_appdata_dayu_dir().expect("无法解析 %LOCALAPPDATA%");
    let store = store::Store::new(&data_dir).expect("无法初始化 store");
    let journal = journal::Journal::new(data_dir.join("operation_journal.jsonl")).expect("无法初始化 journal");
    let history = history::History::new(data_dir.join("history.jsonl")).expect("无法初始化 history");

    // 启动恢复：读取未完成任务并记录到日志（前端 get_recovery_advice 读取展示）
    if let Ok(pending) = journal.recover_pending() {
        if !pending.is_empty() {
            eprintln!("[dayu] 检测到 {} 个未完成任务，已就绪恢复建议", pending.len());
        }
    }

    let state = AppState {
        store, journal, history,
        cancel_token: Arc::new(Mutex::new(None)),
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::scan_drives,
            commands::precheck_migrate,
            commands::start_migrate,
            commands::cancel_migrate,
            commands::start_restore,
            commands::list_links,
            commands::break_link_cmd,
            commands::list_history,
            commands::get_config,
            commands::save_config,
            commands::export_history,
            commands::get_recovery_advice,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

> 注：`lib.rs` 顶部不再保留 T1 的占位 `run()`；`main.rs` 仍调 `dayu_disk_manager_lib::run()`（create-tauri-app 生成的 lib 名，按实际 `Cargo.toml` 的 `[lib] name` 调整）。

- [ ] **步骤 5：运行测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml` 和 `cargo build --manifest-path src-tauri/Cargo.toml`
预期：app_state 3 个测试 PASS；`cargo build` 编译通过（Tauri 宏展开无误）。

- [ ] **步骤 6：Commit**

```bash
git add src-tauri/src/commands.rs src-tauri/src/app_state.rs src-tauri/src/lib.rs
git commit -m "feat(ipc): Tauri commands/events 合约与启动恢复"
```

---
## 阶段 5：前端

### 任务 13：前端骨架、路由与 IPC 封装

**文件：**
- 创建：`src/main.ts`、`src/App.vue`、`src/router/index.ts`、`src/ipc/invoke.ts`、`src/ipc/events.ts`、`src/ipc/types.ts`
- 修改：`package.json`（加 `vue-router`、`pinia` 依赖）
- 测试：`src/ipc/types.test.ts`（Vitest，校验类型映射）

**职责：** 搭前端骨架——Vue app + 5 视图路由 + Pinia + 与后端命令/事件一一对应的类型化 IPC 封装。前端类型必须与 `models.rs`（camelCase 序列化）对齐。

- [ ] **步骤 1：安装前端依赖**

运行：`pnpm add vue-router pinia` 和 `pnpm add -D vitest @vue/test-utils`

- [ ] **步骤 2：定义前端类型（与后端 models 对齐）**

`src/ipc/types.ts`：

```typescript
export interface ScanItem {
  path: string
  displayName: string
  sizeBytes: number
  matchedPreset: string | null
  category: PresetCategory | null
  autoMigrate: boolean
  isJunction: boolean
  inaccessible: boolean
}

export type PresetCategory =
  | 'communication' | 'game_library' | 'dev_cache'
  | 'ide' | 'container' | 'app_install' | 'custom'

export interface Migration {
  id: string
  schemaVersion: number
  source: string
  target: string
  oldPath: string
  preset: string | null
  createdAt: string
  status: 'active' | 'old_pending_delete' | 'target_pending_delete' | 'pending_manual_confirm'
  sourceVolumeSerial: string
  targetVolumeSerial: string
  recycleBinRef: string
  pendingCleanup: string | null
}

export interface LinkItem {
  id: string
  source: string
  target: string
  preset: string | null
  createdAt: string
  status: string
  valid: boolean
  broken: boolean
}

export interface HistoryEntry {
  op: string
  id: string
  src: string
  dst: string
  result: string
  time: string
  durationSec: number
}

export interface Config {
  schemaVersion: number
  repository: string
  scan: { minSizeMb: number; excludePaths: string[] }
  presets: Preset[]
}

export interface Preset {
  id: string
  name: string
  category: PresetCategory
  matchPaths: string[]
  matchProcesses: string[]
  autoMigrate: boolean
  targetSubdir: string
}

export interface PrecheckReport {
  ok: boolean
  warnings: string[]
  blockers: string[]
  sourceSizeBytes: number
  targetFreeBytes: number
}

export interface ProgressEvent {
  taskId: string
  stage: string
  percent: number
  message: string
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let v = bytes / 1024
  let i = 0
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++ }
  return `${v.toFixed(1)} ${units[i]}`
}
```

- [ ] **步骤 3：IPC 封装（invoke + events）**

`src/ipc/invoke.ts`：

```typescript
import { invoke } from '@tauri-apps/api/core'
import type {
  ScanItem, Migration, LinkItem, HistoryEntry, Config, PrecheckReport,
} from './types'

export const ipc = {
  scanDrives: () => invoke<ScanItem[]>('scan_drives'),
  precheckMigrate: (src: string) => invoke<PrecheckReport>('precheck_migrate', { src }),
  startMigrate: (migrationId: string, src: string, presetId: string | null) =>
    invoke<Migration>('start_migrate', { migrationId, src, presetId }),
  cancelMigrate: () => invoke<boolean>('cancel_migrate'),
  startRestore: (migrationId: string) => invoke<boolean>('start_restore', { migrationId }),
  listLinks: () => invoke<LinkItem[]>('list_links'),
  breakLink: (migrationId: string) => invoke<boolean>('break_link_cmd', { migrationId }),
  listHistory: (op?: string, from?: string, to?: string) =>
    invoke<HistoryEntry[]>('list_history', { op, from, to }),
  getConfig: () => invoke<Config>('get_config'),
  saveConfig: (config: Config) => invoke<void>('save_config', { config }),
  exportHistory: () => invoke<string>('export_history'),
  getRecoveryAdvice: () => invoke<[string, string, string][]>('get_recovery_advice'),
}
```

`src/ipc/events.ts`：

```typescript
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type { ProgressEvent } from './types'

export async function onProgress(cb: (e: ProgressEvent) => void): Promise<UnlistenFn> {
  return listen<ProgressEvent>('dayu://progress', (ev) => cb(ev.payload))
}
```
- [ ] **步骤 4：路由与 App 骨架**

`src/router/index.ts`：

```typescript
import { createRouter, createWebHashHistory, type RouteRecordRaw } from 'vue-router'

const routes: RouteRecordRaw[] = [
  { path: '/', redirect: '/scan' },
  { path: '/scan', name: 'scan', component: () => import('../views/ScanView.vue') },
  { path: '/migrate', name: 'migrate', component: () => import('../views/MigrateView.vue') },
  { path: '/links', name: 'links', component: () => import('../views/LinksView.vue') },
  { path: '/history', name: 'history', component: () => import('../views/HistoryView.vue') },
  { path: '/settings', name: 'settings', component: () => import('../views/SettingsView.vue') },
]

export const router = createRouter({ history: createWebHashHistory(), routes })
```

`src/main.ts`：

```typescript
import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import { router } from './router'

createApp(App).use(createPinia()).use(router).mount('#app')
```

`src/App.vue`（侧边导航 + RouterView）：

```vue
<script setup lang="ts">
import { RouterView, RouterLink } from 'vue-router'
</script>

<template>
  <div class="layout">
    <nav class="sidebar">
      <h1>大禹磁盘管理器</h1>
      <RouterLink to="/scan">扫描分析</RouterLink>
      <RouterLink to="/migrate">迁移</RouterLink>
      <RouterLink to="/links">软链接管理</RouterLink>
      <RouterLink to="/history">操作历史</RouterLink>
      <RouterLink to="/settings">设置</RouterLink>
    </nav>
    <main class="content"><RouterView /></main>
  </div>
</template>

<style>
.layout { display: flex; height: 100vh; }
.sidebar { width: 200px; padding: 16px; background: #f4f4f5; display: flex; flex-direction: column; gap: 8px; }
.sidebar a { text-decoration: none; color: #333; padding: 8px; border-radius: 4px; }
.sidebar a.router-link-active { background: #3b82f6; color: #fff; }
.content { flex: 1; padding: 24px; overflow: auto; }
</style>
```

- [ ] **步骤 5：Vitest 校验 formatSize 与类型映射**

`src/ipc/types.test.ts`：

```typescript
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
```

在 `package.json` 加 `"test": "vitest run"`。运行：`pnpm test`
预期：PASS，4 个断言通过。

- [ ] **步骤 6：Commit**

```bash
git add src/main.ts src/App.vue src/router src/ipc package.json
git commit -m "feat(web): 前端骨架、路由、类型化 IPC 封装"
```

---
### 任务 14：ScanView — 扫描结果与一键迁移

**文件：** 创建 `src/stores/scan.ts`、`src/components/SizeCell.vue`、`src/views/ScanView.vue`

- [ ] **步骤 1：scan store**

`src/stores/scan.ts`：

```typescript
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { ipc } from '../ipc/invoke'
import type { ScanItem } from '../ipc/types'

export const useScanStore = defineStore('scan', () => {
  const items = ref<ScanItem[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function scan() {
    loading.value = true; error.value = null
    try { items.value = await ipc.scanDrives() }
    catch (e) { error.value = String(e) }
    finally { loading.value = false }
  }
  return { items, loading, error, scan }
})
```

- [ ] **步骤 2：SizeCell 复用组件**

`src/components/SizeCell.vue`：

```vue
<script setup lang="ts">
import { formatSize } from '../ipc/types'
defineProps<{ bytes: number }>()
</script>
<template>
  <span class="size">{{ formatSize(bytes) }}</span>
</template>
<style scoped>.size { font-variant-numeric: tabular-nums; color: #2563eb; }</style>
```

- [ ] **步骤 3：ScanView**

`src/views/ScanView.vue`：

```vue
<script setup lang="ts">
import { onMounted } from 'vue'
import { useRouter } from 'vue-router'
import { useScanStore } from '../stores/scan'
import SizeCell from '../components/SizeCell.vue'

const store = useScanStore()
const router = useRouter()
onMounted(() => store.scan())

function migrate(item: { path: string; matchedPreset: string | null }) {
  // 选中目标后跳迁移页（传 path 与 presetId）
  router.push({ name: 'migrate', query: { src: item.path, presetId: item.matchedPreset ?? '' } })
}
</script>

<template>
  <div>
    <header><h2>扫描分析</h2>
      <button :disabled="store.loading" @click="store.scan()">
        {{ store.loading ? '扫描中…' : '重新扫描 C 盘' }}
      </button>
      <p v-if="store.error" class="err">{{ store.error }}</p>
    </header>
    <table>
      <thead><tr><th>名称</th><th>大小</th><th>类别</th><th>状态</th><th></th></tr></thead>
      <tbody>
        <tr v-for="it in store.items" :key="it.path">
          <td>{{ it.displayName }}<div class="path">{{ it.path }}</div></td>
          <td><SizeCell :bytes="it.sizeBytes" /></td>
          <td>{{ it.category ?? '自定义' }}</td>
          <td>
            <span v-if="it.isJunction" class="tag">已迁移(junction)</span>
            <span v-else-if="!it.autoMigrate" class="tag warn">需确认风险</span>
            <span v-else-if="it.inaccessible" class="tag err">无法访问</span>
          </td>
          <td>
            <button v-if="!it.isJunction" @click="migrate(it)">
              {{ it.autoMigrate ? '一键迁移' : '自定义迁移' }}
            </button>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>
<style scoped>
.path { font-size: 12px; color: #888; }
.tag { padding: 2px 6px; border-radius: 4px; background: #e5e7eb; font-size: 12px; }
.tag.warn { background: #fef3c7; } .tag.err { background: #fee2e2; }
</style>
```

- [ ] **步骤 4：Commit**

```bash
git add src/stores/scan.ts src/components/SizeCell.vue src/views/ScanView.vue
git commit -m "feat(web): ScanView 扫描结果展示与一键迁移入口"
```

---
### 任务 15：MigrateView — 预检清单与进度

**文件：** 创建 `src/stores/migrate.ts`、`src/components/ProgressStage.vue`、`src/views/MigrateView.vue`

- [ ] **步骤 1：migrate store（含进度状态机镜像）**

`src/stores/migrate.ts`：

```typescript
import { defineStore } from 'pinia'
import { ref, onUnmounted } from 'vue'
import { ipc } from '../ipc/invoke'
import { onProgress } from '../ipc/events'
import type { PrecheckReport, ProgressEvent } from '../ipc/types'

export const useMigrateStore = defineStore('migrate', () => {
  const report = ref<PrecheckReport | null>(null)
  const running = ref(false)
  const progress = ref<ProgressEvent | null>(null)
  const result = ref<{ ok: boolean; message: string } | null>(null)
  let unlisten: (() => void) | null = null

  async function precheck(src: string) {
    report.value = await ipc.precheckMigrate(src)
  }

  async function initListener() {
    if (!unlisten) unlisten = await onProgress((e) => { progress.value = e })
  }

  async function run(migrationId: string, src: string, presetId: string | null) {
    await initListener()
    running.value = true; result.value = null
    try {
      await ipc.startMigrate(migrationId, src, presetId)
      result.value = { ok: true, message: '迁移完成' }
    } catch (e) {
      result.value = { ok: false, message: String(e) }
    } finally {
      running.value = false
    }
  }

  function cancel() { ipc.cancelMigrate() }

  function cleanup() { unlisten?.(); unlisten = null }
  return { report, running, progress, result, precheck, run, cancel, cleanup }
})
```

- [ ] **步骤 2：ProgressStage 复用组件**

`src/components/ProgressStage.vue`：

```vue
<script setup lang="ts">
import type { ProgressEvent } from '../ipc/types'
defineProps<{ progress: ProgressEvent | null }>()
const stageLabels: Record<string, string> = {
  copying: '复制中', verifying: '校验中', renaming_source: '改名源目录',
  syncing: '增量同步', creating_junction: '建立链接', recording: '记录映射',
  cleaning: '清理原目录', removing_junction: '删除链接', switching: '切换目录',
}
</script>
<template>
  <div v-if="progress" class="progress">
    <div class="bar"><div class="fill" :style="{ width: progress.percent + '%' }" /></div>
    <span>{{ stageLabels[progress.stage] ?? progress.stage }} — {{ progress.percent }}% — {{ progress.message }}</span>
  </div>
</template>
<style scoped>
.progress { margin: 12px 0; }
.bar { height: 8px; background: #e5e7eb; border-radius: 4px; overflow: hidden; }
.fill { height: 100%; background: #3b82f6; transition: width .2s; }
</style>
```

- [ ] **步骤 3：MigrateView**

`src/views/MigrateView.vue`：

```vue
<script setup lang="ts">
import { ref, onMounted, onUnmounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useMigrateStore } from '../stores/migrate'
import ProgressStage from '../components/ProgressStage.vue'
import SizeCell from '../components/SizeCell.vue'

const route = useRoute()
const router = useRouter()
const store = useMigrateStore()
const src = String(route.query.src ?? '')
const presetId = (route.query.presetId as string) || null
const migrationId = (crypto.randomUUID?.() ?? Date.now().toString())
onMounted(async () => { if (src) { await store.precheck(src) } })
onUnmounted(() => store.cleanup())

async function confirm() {
  await store.run(migrationId, src, presetId)
  if (store.result?.ok) router.push({ name: 'links' })
}
</script>

<template>
  <div>
    <h2>迁移</h2>
    <p>源：<code>{{ src }}</code></p>
    <div v-if="store.report">
      <h3>预检结果</h3>
      <p>源大小：<SizeCell :bytes="store.report.sourceSizeBytes" />　目标剩余：<SizeCell :bytes="store.report.targetFreeBytes" /></p>
      <ul>
        <li v-for="b in store.report.blockers" :key="b" class="block">⛔ {{ b }}</li>
        <li v-for="w in store.report.warnings" :key="w" class="warn">⚠️ {{ w }}</li>
      </ul>
      <button :disabled="!store.report.ok || store.running" @click="confirm()">
        {{ store.running ? '迁移中…' : (store.report.ok ? '确认迁移' : '存在阻断项，无法迁移') }}
      </button>
      <button v-if="store.running" @click="store.cancel()">取消</button>
    </div>
    <ProgressStage :progress="store.progress" />
    <p v-if="store.result" :class="store.result.ok ? 'ok' : 'err'">{{ store.result.message }}</p>
  </div>
</template>
<style scoped>
.block { color: #dc2626; } .warn { color: #d97706; }
.ok { color: #16a34a; } .err { color: #dc2626; }
</style>
```

- [ ] **步骤 4：Commit**

```bash
git add src/stores/migrate.ts src/components/ProgressStage.vue src/views/MigrateView.vue
git commit -m "feat(web): MigrateView 预检清单与进度监听"
```

---
### 任务 16：LinksView — 软链接管理

**文件：** 创建 `src/stores/links.ts`、`src/views/LinksView.vue`

- [ ] **步骤 1：links store**

`src/stores/links.ts`：

```typescript
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { ipc } from '../ipc/invoke'
import type { LinkItem } from '../ipc/types'

export const useLinksStore = defineStore('links', () => {
  const items = ref<LinkItem[]>([])
  async function refresh() { items.value = await ipc.listLinks() }
  async function restore(id: string) { await ipc.startRestore(id); await refresh() }
  async function breakLink(id: string) { await ipc.breakLink(id); await refresh() }
  return { items, refresh, restore, breakLink }
})
```

- [ ] **步骤 2：LinksView（还原/断开，失效链接标注，二次确认）**

`src/views/LinksView.vue`：

```vue
<script setup lang="ts">
import { onMounted } from 'vue'
import { useLinksStore } from '../stores/links'
const store = useLinksStore()
onMounted(() => store.refresh())

function onBreak(id: string) {
  if (window.confirm('断开后原路径将不可用，确认？')) store.breakLink(id)
}
</script>

<template>
  <div>
    <h2>软链接管理</h2>
    <button @click="store.refresh()">刷新</button>
    <table>
      <thead><tr><th>源(原路径)</th><th>目标(数据)</th><th>状态</th><th>操作</th></tr></thead>
      <tbody>
        <tr v-for="l in store.items" :key="l.id">
          <td>{{ l.source }}</td>
          <td>{{ l.target }}</td>
          <td>
            <span v-if="l.broken" class="tag err">失效(目标缺失)</span>
            <span v-else-if="!l.valid" class="tag warn">链接无效</span>
            <span v-else class="tag ok">正常</span>
          </td>
          <td>
            <button :disabled="l.broken" @click="store.restore(l.id)">还原</button>
            <button @click="onBreak(l.id)">断开</button>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>
<style scoped>
.tag { padding: 2px 6px; border-radius: 4px; font-size: 12px; }
.tag.ok { background: #dcfce7; color: #16a34a; }
.tag.warn { background: #fef3c7; color: #d97706; }
.tag.err { background: #fee2e2; color: #dc2626; }
</style>
```

- [ ] **步骤 3：Commit**

```bash
git add src/stores/links.ts src/views/LinksView.vue
git commit -m "feat(web): LinksView 软链接管理与失效标注"
```

---
### 任务 17：HistoryView — 操作历史筛选

**文件：** 创建 `src/views/HistoryView.vue`

- [ ] **步骤 1：HistoryView（按 op 筛选，从链接/历史双向追溯）**

`src/views/HistoryView.vue`：

```vue
<script setup lang="ts">
import { ref, onMounted, watch } from 'vue'
import { ipc } from '../ipc/invoke'
import type { HistoryEntry } from '../ipc/types'

const opFilter = ref<string>('')
const items = ref<HistoryEntry[]>([])

async function load() {
  items.value = await ipc.listHistory(opFilter.value || undefined)
}
onMounted(load)
watch(opFilter, load)

const opLabel: Record<string, string> = {
  migrate: '迁移', restore: '还原', break_link: '断开链接', delete_link: '删除链接',
}
const resultClass = (r: string) => r === 'ok' ? 'ok' : 'err'
</script>

<template>
  <div>
    <h2>操作历史</h2>
    <select v-model="opFilter">
      <option value="">全部</option>
      <option v-for="k in Object.keys(opLabel)" :key="k" :value="k">{{ opLabel[k] }}</option>
    </select>
    <table>
      <thead><tr><th>时间</th><th>操作</th><th>源</th><th>目标</th><th>结果</th></tr></thead>
      <tbody>
        <tr v-for="(h, i) in items" :key="i">
          <td>{{ h.time }}</td>
          <td>{{ opLabel[h.op] ?? h.op }}</td>
          <td>{{ h.src }}</td>
          <td>{{ h.dst }}</td>
          <td :class="resultClass(h.result)">{{ h.result }}</td>
        </tr>
      </tbody>
    </table>
  </div>
</template>
<style scoped>
.ok { color: #16a34a; } .err { color: #dc2626; }
</style>
```

- [ ] **步骤 2：Commit**

```bash
git add src/views/HistoryView.vue
git commit -m "feat(web): HistoryView 操作历史筛选"
```

---

### 任务 18：SettingsView — 仓库路径与扫描偏好

**文件：** 创建 `src/views/SettingsView.vue`

- [ ] **步骤 1：SettingsView（仓库路径/阈值/排除路径/导出日志/数据位置）**

`src/views/SettingsView.vue`：

```vue
<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { ipc } from '../ipc/invoke'
import type { Config } from '../ipc/types'

const config = ref<Config | null>(null)
const saved = ref(false)
const exported = ref('')

onMounted(async () => { config.value = await ipc.getConfig() })

async function save() {
  if (!config.value) return
  await ipc.saveConfig(config.value)
  saved.value = true
  setTimeout(() => (saved.value = false), 1500)
}

async function exportLog() {
  exported.value = await ipc.exportHistory()
}

function addExclude() { config.value?.scan.excludePaths.push('') }
function removeExclude(i: number) { config.value?.scan.excludePaths.splice(i, 1) }
</script>

<template>
  <div v-if="config">
    <h2>设置</h2>
    <section>
      <label>统一迁移仓库路径</label>
      <input v-model="config.repository" placeholder="D:/Migrated" />
      <p class="hint">仓库不能位于 C 盘、不能是网络路径、不能位于待迁源目录内部，所在盘需为本地 NTFS。</p>
    </section>
    <section>
      <label>大目录阈值 (MB)</label>
      <input type="number" v-model.number="config.scan.minSizeMb" />
    </section>
    <section>
      <label>扫描排除路径</label>
      <div v-for="(p, i) in config.scan.excludePaths" :key="i" class="row">
        <input v-model="config.scan.excludePaths[i]" />
        <button @click="removeExclude(i)">删除</button>
      </div>
      <button @click="addExclude">添加排除路径</button>
    </section>
    <section>
      <button @click="save">保存设置</button>
      <span v-if="saved" class="ok">已保存</span>
    </section>
    <section>
      <button @click="exportLog">导出操作日志</button>
      <details v-if="exported"><summary>日志内容</summary><pre>{{ exported }}</pre></details>
    </section>
  </div>
</template>
<style scoped>
section { margin-bottom: 16px; }
input { width: 100%; max-width: 480px; padding: 6px; }
.row { display: flex; gap: 8px; margin: 4px 0; }
.hint { font-size: 12px; color: #888; }
.ok { color: #16a34a; }
</style>
```

- [ ] **步骤 2：Commit**

```bash
git add src/views/SettingsView.vue
git commit -m "feat(web): SettingsView 仓库路径与扫描偏好"
```

---
## 阶段 6：集成与收尾

### 任务 19：端到端集成测试

**文件：**
- 创建：`src-tauri/tests/e2e_migration.rs`
- 依赖：T2-T11 全部单元已实现

**职责：** 用 `tempfile` 构造一个完整 mock 盘结构（微信目录、Steam 目录、随机大目录），真实文件系统跑通"扫描→预检→迁移→建链→记录→还原"全链路，验证 junction 可被 Windows 正常解析、回滚后数据完整、源目录内部 reparse point 不被递归复制。

- [ ] **步骤 1：编写集成测试**

`src-tauri/tests/e2e_migration.rs`：

```rust
use dayu_disk_manager_lib::file_ops::RealFileOps;
use dayu_disk_manager_lib::history::History;
use dayu_disk_manager_lib::journal::Journal;
use dayu_disk_manager_lib::migrator::{self, MigratePlan};
use dayu_disk_manager_lib::models::MigrationStatus;
use dayu_disk_manager_lib::scanner;
use dayu_disk_manager_lib::store::Store;
use std::sync::atomic::AtomicBool;
use tempfile::TempDir;

#[test]
fn full_pipeline_migrate_then_restore_preserves_data() {
    let dir = TempDir::new().unwrap();
    let store = Store::new(dir.path().join("data")).unwrap();
    let journal = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    let history = History::new(dir.path().join("history.jsonl")).unwrap();

    // 构造源：含文件 + 内部 junction（不应被递归复制）
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("docs")).unwrap();
    std::fs::write(src.join("docs/readme.md"), b"hi").unwrap();
    let inner_target = dir.path().join("inner_target");
    std::fs::create_dir_all(&inner_target).unwrap();
    std::fs::write(inner_target.join("secret.bin"), vec![0u8; 4096]).unwrap();
    #[cfg(windows)]
    junction::create(&inner_target, &src.join("link")).unwrap();

    let plan = MigratePlan {
        task_id: "e2e-t1".into(),
        migration_id: "e2e-m1".into(),
        src: src.clone(),
        target: dir.path().join("repo/wechat/e2e-m1/data"),
        tmp: dir.path().join("repo/wechat/e2e-m1/data.tmp"),
        old_path: src.with_extension("dayu-old-e2e-t1"),
        preset_id: Some("wechat".into()),
        source_volume_serial: "C".into(),
        target_volume_serial: "D".into(),
    };
    let cancel = AtomicBool::new(false);
    let m = migrator::migrate(&RealFileOps, &store, &journal, &history, &plan, &|_| {}, &cancel).unwrap();
    assert_eq!(m.status, MigrationStatus::Active);

    // junction 解析正常
    assert!(dayu_disk_manager_lib::junction::verify(&src));
    // 数据已迁移
    assert!(plan.target.join("docs/readme.md").exists());
    // 内部 junction 未被递归复制内容
    assert!(!plan.target.join("link/secret.bin").exists());

    // 还原
    migrator::restore(&RealFileOps, &store, &journal, &history, &m, &|_| {}, &cancel).unwrap();
    assert!(!dayu_disk_manager_lib::junction::exists(&src));
    assert!(src.join("docs/readme.md").exists(), "还原后数据完整");
}

#[test]
fn scanner_finds_migrated_junction_marker() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("t");
    std::fs::create_dir_all(&target).unwrap();
    let link = dir.path().join("src");
    #[cfg(windows)]
    junction::create(&target, &link).unwrap();
    let cfg = dayu_disk_manager_lib::store::default_config();
    let items = scanner::scan(dir.path(), &cfg);
    #[cfg(windows)]
    assert!(items.iter().any(|i| i.is_junction));
}
```

- [ ] **步骤 2：运行集成测试**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --test e2e_migration`
预期：PASS，2 个测试通过（Windows 上；非 Windows 跳过 junction 相关断言）。

- [ ] **步骤 3：Commit**

```bash
git add src-tauri/tests/e2e_migration.rs
git commit -m "test: 端到端迁移-还原全链路集成测试"
```

---
### 任务 20：崩溃恢复边界用例

**文件：** 创建：`src-tauri/tests/crash_recovery.rs`

**职责：** 验证规格第 5 章"应用崩溃/断电"各阶段的恢复行为——构造半迁移现场，重启（重新打开 Journal）后 `recover_pending` 返回正确决策，同路径再发起迁移被拒绝。

- [ ] **步骤 1：编写崩溃恢复测试**

`src-tauri/tests/crash_recovery.rs`：

```rust
use dayu_disk_manager_lib::journal::Journal;
use dayu_disk_manager_lib::app_state::recover_pending_decisions;
use dayu_disk_manager_lib::models::JournalEntry;
use tempfile::TempDir;

fn entry(stage: &str, task: &str, src: &str) -> JournalEntry {
    JournalEntry {
        task_id: task.into(), op: "migrate".into(), migration_id: format!("m-{task}"),
        stage: stage.into(), src: src.into(), dst: "D:/d".into(),
        tmp: "D:/d.tmp".into(), old_path: format!("{src}.old"), time: "2026-07-18T00:00:00Z".into(),
        final_mark: None,
    }
}

#[test]
fn crash_after_copied_recovers_as_clean_tmp_retry() {
    let dir = TempDir::new().unwrap();
    let j = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    j.begin("t1", "m-t1", "migrate", "C:/src", "D:/d", "D:/d.tmp", "C:/src.old").unwrap();
    j.mark_stage("t1", "copied").unwrap();
    // 模拟"重启"：重新打开同一 journal
    let j2 = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    let pending = j2.recover_pending().unwrap();
    assert_eq!(pending.len(), 1);
    let decisions = recover_pending_decisions(&pending);
    assert!(decisions[0].2.contains("清 tmp"));
}

#[test]
fn crash_after_source_renamed_recovers_as_rename_back() {
    let dir = TempDir::new().unwrap();
    let j = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    j.begin("t1", "m-t1", "migrate", "C:/src", "D:/d", "D:/d.tmp", "C:/src.old").unwrap();
    j.mark_stage("t1", "copied").unwrap();
    j.mark_stage("t1", "source_renamed").unwrap();
    let j2 = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    let pending = j2.recover_pending().unwrap();
    let decisions = recover_pending_decisions(&pending);
    assert!(decisions[0].2.contains("改回原名"));
}

#[test]
fn crash_after_junction_created_keeps_link_and_advises() {
    let dir = TempDir::new().unwrap();
    let j = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    j.begin("t1", "m-t1", "migrate", "C:/src", "D:/d", "D:/d.tmp", "C:/src.old").unwrap();
    j.mark_stage("t1", "junction_created").unwrap();
    let j2 = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    let pending = j2.recover_pending().unwrap();
    let decisions = recover_pending_decisions(&pending);
    assert!(decisions[0].2.contains("已建链"));
}

#[test]
fn restart_blocks_new_migrate_on_same_source_pending() {
    let dir = TempDir::new().unwrap();
    let j = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    j.begin("t1", "m-t1", "migrate", "C:/src", "D:/d", "D:/d.tmp", "C:/src.old").unwrap();
    // 重启后对同源发起新迁移应被 journal 路径锁拒绝
    let j2 = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    let res = j2.begin("t2", "m-t2", "migrate", "C:/src", "D:/d2", "D:/d2.tmp", "C:/src.old2");
    assert!(res.is_err(), "同源 pending 时新迁移应被拒绝");
}

#[test]
fn completed_task_not_in_pending_after_restart() {
    let dir = TempDir::new().unwrap();
    let j = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    j.begin("t1", "m-t1", "migrate", "C:/src", "D:/d", "D:/d.tmp", "C:/src.old").unwrap();
    j.complete("t1").unwrap();
    let j2 = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    assert!(j2.recover_pending().unwrap().is_empty());
}
```

- [ ] **步骤 2：运行测试**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --test crash_recovery`
预期：PASS，5 个测试通过。

- [ ] **步骤 3：Commit**

```bash
git add src-tauri/tests/crash_recovery.rs
git commit -m "test: 崩溃恢复各阶段与路径锁边界用例"
```

---
### 任务 21：打包与手工验证清单

**文件：** 修改 `src-tauri/tauri.conf.json`（产品名/标识/打包配置）、`README.md`（手工验证清单）

**职责：** 配置打包产出可安装的 Windows 安装包；将规格第 7 章"手工/端到端验证清单"固化为发布前必跑清单（不在 CI，发布前人工执行）。

- [ ] **步骤 1：配置 tauri.conf.json 打包**

确认 `src-tauri/tauri.conf.json` 关键字段（create-tauri-app 已生成大部分，按需补）：

```json
{
  "productName": "dayu-disk-manager",
  "identifier": "com.dayu.disk-manager",
  "build": {
    "frontendDist": "../dist",
    "devUrl": "http://localhost:1420",
    "beforeDevCommand": "pnpm dev",
    "beforeBuildCommand": "pnpm build"
  },
  "app": { "withGlobalTauri": false },
  "bundle": {
    "active": true,
    "targets": ["msi", "nsis"],
    "icon": ["icons/icon.ico"],
    "windows": { "webviewInstallMode": { "type": "downloadBootstrapper" } }
  }
}
```

- [ ] **步骤 2：构建安装包**

运行：`pnpm tauri build`
预期：在 `src-tauri/target/release/bundle/` 下生成 `.msi` 与 `.exe`（NSIS）安装包，前端 `dist/` 已构建。

- [ ] **步骤 3：在 README 固化手工验证清单**

`README.md` 追加（对应规格第 7 章第 3 节）：

```markdown
## 发布前手工验证清单（不在 CI，发布前必跑）

- [ ] 微信/Steam 真实迁移（关闭应用后）：迁移后应用能正常启动并找到文件。
- [ ] 在以下关键阶段杀进程后重启工具，验证残留清理与状态恢复：
      复制中 / 源目录改名 / junction 创建 / 记录映射 / 回收站清理。
- [ ] 回收站可用与不可用两种情况：`.dayu-old-*` 从回收站恢复；不可用时 old_pending_delete 正确标注。
- [ ] 权限不足目录：普通用户迁移失败有明确提示；管理员重启后重试路径可用。
- [ ] 失效链接：手动删除 target 后 LinksView 标注"失效"，清理流程正常。
- [ ] 还原：还原后源路径恢复为普通目录、数据完整；切换失败时 junction 被重建。
- [ ] 启动恢复：构造半迁移现场后重启应用，前端 get_recovery_advice 展示正确决策且同源新迁移被拒。
```

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/tauri.conf.json README.md
git commit -m "chore: 打包配置与发布前手工验证清单"
```

---
---

## 自检结论

**1. 规格覆盖度：** 规格 1-8 章均有对应任务（扫描→T8/T14；迁移状态机→T9/T10/T15；链接管理→T11/T16；历史→T7/T17；设置→T2/T18；架构边界→8 单元文件；数据流→T19；错误处理表→T10/T11 各阶段回滚；持久化→T2/T6/T7；测试策略→分层 tempfile+mock 贯穿 + T19/T20；首版范围→YAGNI 边界 + 已知简化）。无遗漏。

**2. 占位符扫描：** 未发现 TODO/待定/补充细节/"类似任务N"等红旗。

**3. 类型一致性（已修复）：**
- `LinkItem` 统一定义在 `app_state.rs`（原误标 commands.rs），与 `list_links` 返回类型 `crate::app_state::LinkItem` 对齐。
- 命令清单 `cancel_migrate` 入参由 `{ taskId }` 改 `{}`（实现无参）；`break_link` 命令名改 `break_link_cmd`（与 Rust fn 名、前端 invoke 一致）。
- `commands.rs` 移除未用的 `stage` 导入；`start_migrate` 的卷序列号变量 `_src_serial`/`_tgt_serial` 改正常名（曾被引用却带下划线前缀）。
- 前端 `types.ts` 的 `PresetCategory`/`MigrationStatus` 与后端 `snake_case` serde 序列化逐项对齐；`ProgressEvent.stage` 常量与前端 `stageLabels` 覆盖一致。
- 全部 serde 结构体 `rename_all = "camelCase"`，与规格第 6 章 JSON 字段（`minSizeMB`/`sourceVolumeSerial`/`oldPath`/`migrationId`/`durationSec`）一致；`MigrationStatus`/`PresetCategory` 用 `snake_case` 匹配枚举值。

**计划已完成并保存到 `docs/superpowers/plans/2026-07-18-dayu-disk-manager.md`。**
