# 迁移板块提速实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 在保持迁移全部正确性契约（journal 阶段、`source_changed` 跟踪、回滚路径、VSS 分支、回收站降级、前端进度契约）的前提下，把非 VSS 迁移的目录遍历次数 6→2、文件数据全量读取 2→1，并引入文件级并发掩盖小文件元数据延迟。

**架构：** 用两阶段流程替换 `measure_tree`+`copy_tree`+双 `manifest`：阶段①单线程 stat-only 扫描建树、建目录、收集文件任务并记录目录/reparse 占位条目；阶段②`std::thread::scope` 内 N 个 worker 从共享 `VecDeque` 取文件任务并发复制，记录"实际读入"的 `copied_manifest` 与"实际落盘"的 `dst_manifest`，主线程跑 100ms 进度汇报器。`copy_tree` 返回 `CopyOutcome`，migrator 的 b' 用两份 outcome manifest diff（零额外遍历），c' 用 `copied_manifest` vs 改名后的 `old_manifest` diff 判断源是否变化——空则跳过补传（常态零数据读取），非空则清空 tmp 全量补传并二次复核。

**技术栈：** Rust 2021、std 并发原语（`std::thread::scope`、`Arc`/`Mutex`、`AtomicU64`/`AtomicUsize`/`AtomicBool`、`VecDeque`）、`NonZeroUsize`、Tauri 2、tempfile 3（测试）。零新依赖。

---

## 范围

单一子系统（迁移复制/校验路径），无需拆分。改动面：`file_ops.rs`（核心）、`migrator.rs`（阶段重组 + plan 字段 + MockOps）、`commands.rs` 与 `tests/e2e_migration.rs`（plan 构造点补字段）。阶段 d（建链/记录/清理）与回收站降级、前端 `TransferProgress` 结构、restore 流程均不动（restore 仅适配新签名获得复制并发加速，不重组）。

## 文件结构

- **修改** `src-tauri/src/file_ops.rs`
  - 新增 `pub struct CopyOutcome { copied_manifest, dst_manifest, total_bytes, total_files }`。
  - 新增私有 `struct CopyTask { src, dst, rel_path }`。
  - 新增 `pub fn resolve_concurrency(Option<NonZeroUsize>) -> usize`。
  - 新增私有 `fn copy_file_counted(src, dst, should_cancel) -> AppResult<u64>`（返回实际读写字节数，不回调进度）。
  - `trait FileOps::copy_tree` 签名：返回 `AppResult<CopyOutcome>`；`should_cancel` 改 `&(dyn Fn() -> bool + Sync)`；新增 `concurrency: usize` 参数。
  - `RealFileOps::copy_tree` 重写为两阶段（任务1 串行，任务2 并发）；删除 `measure_tree` 与 `copy_file_with_control`（被取代）。
  - `manifest` / `diff_manifests` / `entry_for` / `rel_under` 等保留不变。
- **修改** `src-tauri/src/migrator.rs`
  - `MigratePlan` 新增 `pub copy_concurrency: Option<NonZeroUsize>`。
  - `migrate_with_snapshot` 阶段 a/b 重组为 a'/b'（任务1）；阶段 c 的 else 分支重组为 c'（任务3）。
  - `restore_with_snapshot` 与 migrate c' 的 `copy_tree` 调用补 `concurrency` 参数（流程不变，`Ok(CopyOutcome)` 自动丢弃）。
  - `MockOps::copy_tree` 返回 `CopyOutcome`；任务3 改造其 manifest/diff 以支持 c' 源变化注入。
  - `plan_for` 与受影响测试补 `copy_concurrency`、调整断言。
- **修改** `src-tauri/src/commands.rs`：`MigratePlan` 构造（约 347 行）补 `copy_concurrency: None`。
- **修改** `src-tauri/tests/e2e_migration.rs`：`MigratePlan` 构造（约 27 行）补 `copy_concurrency: None`。

## 设计补全说明（对规格 §4.3 的必要补充）

规格 §4.2 要求 `copy_concurrency` 可覆盖并发度，但 §4.3 接口变更清单未说明 `copy_tree` 如何接收并发度。本计划补全为：**`copy_tree` 增加 `concurrency: usize` 参数**（仅加参数，不加新方法，不违反规格 §5.1"不加新方法"）。`migrator` 通过 `resolve_concurrency(plan.copy_concurrency)` 解析后传入；`restore` 传 `resolve_concurrency(None)`（默认）。`MockOps` 与 `file_ops` 测试忽略该参数或传 `1`。

## 运行与测试约定

- 本机 Windows，优先 PowerShell。所有 cargo 命令带 `--manifest-path src-tauri/Cargo.toml`，不要 `cd`。
- 编译检查：`cargo check --manifest-path src-tauri/Cargo.toml`
- 全量测试：`cargo test --manifest-path src-tauri/Cargo.toml`
- 单测（lib）：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- <测试名>`
- 集成测试：`cargo test --manifest-path src-tauri/Cargo.toml --test e2e_migration`

---

## 任务 1：CopyOutcome + 签名变更 + 串行实现 + 全调用点适配

**目标：** 引入 `CopyOutcome`、`CopyTask`、`resolve_concurrency`、`copy_file_counted`；改 `copy_tree` 签名（返回 `CopyOutcome`、`should_cancel` 加 `Sync`、新增 `concurrency` 参数）；用**串行**两阶段实现替换旧 `measure_tree`+`copy_tree`（正确性优先，并发留给任务2）；适配全部调用点与 `MigratePlan`；migrator 阶段 a/b 重组为 a'/b'（c' 暂保留旧逻辑，仅补参数）。任务末尾全项目编译通过、现有测试全绿，然后一次 commit。

**文件：**
- 修改：`src-tauri/src/file_ops.rs`（新类型、签名、串行实现、删除 `measure_tree` 与 `copy_file_with_control`）
- 修改：`src-tauri/src/migrator.rs`（`MigratePlan` 字段、`MockOps::copy_tree`、阶段 a'/b'、c' 与 restore 补参数、`plan_for`）
- 修改：`src-tauri/src/commands.rs`（`MigratePlan` 构造补字段）
- 修改：`src-tauri/tests/e2e_migration.rs`（`MigratePlan` 构造补字段）

- [ ] **步骤 1：编写 `resolve_concurrency` 的失败测试**

在 `src-tauri/src/file_ops.rs` 的 `#[cfg(test)] mod tests` 内（`fn ops()` 之后）新增：

```rust
    #[test]
    fn resolve_concurrency_defaults_to_available_cores_capped_at_8() {
        let default = resolve_concurrency(None);
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        assert_eq!(default, cores.min(8));
        assert!(default >= 1);
    }

    #[test]
    fn resolve_concurrency_override_is_used_verbatim() {
        // 覆盖值原样使用（不 clamp）；NonZeroUsize 从类型层保证 >= 1。
        assert_eq!(resolve_concurrency(Some(NonZeroUsize::new(1).unwrap())), 1);
        assert_eq!(resolve_concurrency(Some(NonZeroUsize::new(3).unwrap())), 3);
    }
```

并在该测试模块顶部的 `use super::*;` 之后补一行 `use std::num::NonZeroUsize;`。

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- resolve_concurrency`
预期：编译失败，报错 `cannot find function 'resolve_concurrency'` / `cannot find type 'NonZeroUsize'`。

- [ ] **步骤 3：在 `file_ops.rs` 顶部补 import，并新增类型与函数**

把 `src-tauri/src/file_ops.rs:1-5` 的 import 块替换为：

```rust
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
```

在 `Manifest` 结构体定义之后（约 `file_ops.rs:57` 之后、`pub trait FileOps` 之前）新增：

```rust
/// `copy_tree` 的产出：实际读入的源条目与实际落盘的目标条目各一份 manifest，
/// 外加扫描阶段统计的总量。两份 manifest 由 migrator 的 b'/c' 直接 diff，
/// 不再额外遍历目录。
pub struct CopyOutcome {
    /// 实际创建的目录/reparse 占位 + 实际成功读入的文件条目（size=实际读入字节）。
    pub copied_manifest: Manifest,
    /// 对应目标条目创建/写入后取得的实际元数据（文件 size=目标落盘字节）。
    pub dst_manifest: Manifest,
    pub total_bytes: u64,
    pub total_files: u64,
}

/// 并发度解析：`None` 走默认 `min(available_parallelism, 8)`；`Some(n)` 原样使用
/// （`NonZeroUsize` 从类型层保证 >= 1）。
pub fn resolve_concurrency(override_: Option<NonZeroUsize>) -> usize {
    match override_ {
        Some(n) => n.get(),
        None => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8),
    }
}

/// 单个文件的复制任务（扫描阶段产出，并发阶段消费）。
struct CopyTask {
    src: PathBuf,
    dst: PathBuf,
    rel_path: String,
}
```

- [ ] **步骤 4：改 `trait FileOps::copy_tree` 签名**

把 `src-tauri/src/file_ops.rs:63-69` 的 trait 方法签名替换为：

```rust
    fn copy_tree(
        &self,
        src: &Path,
        dst: &Path,
        on_progress: &dyn Fn(&CopyProgress),
        should_cancel: &(dyn Fn() -> bool + Sync),
        concurrency: usize,
    ) -> AppResult<CopyOutcome>;
```

- [ ] **步骤 5：删除旧 `measure_tree` 与 `copy_file_with_control`，新增 `copy_file_counted`**

删除 `src-tauri/src/file_ops.rs:118-174` 的整个 `impl RealFileOps { fn measure_tree ... }` 块（含其外层 `impl RealFileOps {}` 包装若仅含此方法——保留空 `impl RealFileOps {}` 或直接删；当前该块是独立 `impl`，整段删除）。

删除 `src-tauri/src/file_ops.rs:421-452` 的 `fn copy_file_with_control(...)` 整个函数。

在 `copy_recursive` 函数之前（约原 `copy_file_with_control` 的位置）新增：

```rust
/// 复制单个普通文件，返回实际读写的字节数。不回调进度（进度由调用方的主线程汇报器统一发出）。
/// 每读完一个 buffer 块检查取消。
fn copy_file_counted(
    src: &Path,
    dst: &Path,
    should_cancel: &(dyn Fn() -> bool + Sync),
) -> AppResult<u64> {
    const BUFFER_SIZE: usize = 1024 * 1024;
    let mut input = std::fs::File::open(src)?;
    let mut output = std::fs::File::create(dst)?;
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut copied = 0u64;
    loop {
        if should_cancel() {
            return Err(AppError::Cancelled);
        }
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        copied = copied.saturating_add(read as u64);
    }
    output.flush()?;
    std::fs::set_permissions(dst, std::fs::metadata(src)?.permissions())?;
    Ok(copied)
}
```

- [ ] **步骤 6：重写 `RealFileOps::copy_tree` 为串行两阶段实现**

把 `src-tauri/src/file_ops.rs:177-259` 的整个 `fn copy_tree(...)` 实现替换为下面的串行两阶段版本（任务2 会把阶段②改为并发，签名不变）：

```rust
    fn copy_tree(
        &self,
        src: &Path,
        dst: &Path,
        on_progress: &dyn Fn(&CopyProgress),
        should_cancel: &(dyn Fn() -> bool + Sync),
        _concurrency: usize,
    ) -> AppResult<CopyOutcome> {
        let mut copied_entries: Vec<ManifestEntry> = Vec::new();
        let mut dst_entries: Vec<ManifestEntry> = Vec::new();
        let mut tasks: Vec<CopyTask> = Vec::new();
        let mut total_bytes = 0u64;
        let mut total_files = 0u64;

        // 阶段①：单线程 stat-only 扫描，建目录、收集文件任务、记录目录/reparse 占位。
        let mut stack = vec![(src.to_path_buf(), dst.to_path_buf(), String::new())];
        let mut last_emit = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        while let Some((cur_src, cur_dst, rel)) = stack.pop() {
            if should_cancel() {
                return Err(AppError::Cancelled);
            }
            if !cur_src.exists() {
                continue;
            }
            let is_rp = self.is_reparse_point(&cur_src) && cur_src != *src;
            if is_rp {
                // 非 src 自身的 reparse point：建空目录占位；两份 manifest 记完全相同的
                // 占位条目（rel_path/is_dir/size 三者一致），保证该条目 diff 为空。
                std::fs::create_dir_all(&cur_dst)?;
                let placeholder = ManifestEntry {
                    rel_path: rel,
                    is_dir: true,
                    size: 0,
                    mtime: 0,
                    attrs: 0,
                };
                copied_entries.push(placeholder.clone());
                dst_entries.push(placeholder);
                continue;
            }
            if cur_src.is_dir() {
                std::fs::create_dir_all(&cur_dst)?;
                if cur_src != *src {
                    copied_entries.push(ManifestEntry {
                        rel_path: rel.clone(),
                        is_dir: true,
                        size: 0,
                        mtime: 0,
                        attrs: 0,
                    });
                    dst_entries.push(ManifestEntry {
                        rel_path: rel,
                        is_dir: true,
                        size: 0,
                        mtime: 0,
                        attrs: 0,
                    });
                }
                for entry in std::fs::read_dir(&cur_src)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let child_rel = if rel.is_empty() {
                        name.to_string_lossy().replace('\\', "/")
                    } else {
                        format!("{}/{}", rel, name.to_string_lossy())
                    };
                    stack.push((
                        entry.path(),
                        cur_dst.join(&name),
                        child_rel,
                    ));
                }
            } else {
                // 文件：只 stat 预估总量并推入任务；不作为一致性基准。
                total_bytes = total_bytes.saturating_add(std::fs::metadata(&cur_src)?.len());
                total_files += 1;
                tasks.push(CopyTask {
                    src: cur_src,
                    dst: cur_dst,
                    rel_path: rel,
                });
                if last_emit.elapsed() >= Duration::from_millis(200) {
                    on_progress(&CopyProgress {
                        phase: CopyPhase::Preparing,
                        completed_bytes: total_bytes,
                        total_bytes: None,
                        completed_files: total_files,
                        total_files: None,
                        current_path: None,
                    });
                    last_emit = Instant::now();
                }
            }
        }
        on_progress(&CopyProgress {
            phase: CopyPhase::Preparing,
            completed_bytes: total_bytes,
            total_bytes: None,
            completed_files: total_files,
            total_files: None,
            current_path: None,
        });

        // 阶段②（任务1：串行；任务2 改并发）。打开前重新检查：已不存在或已不再是
        // 普通文件（含变为 reparse/目录）则跳过——这是源变化，交由 c' 对账，不算复制错误。
        let mut completed_bytes = 0u64;
        let mut completed_files = 0u64;
        let mut last_emit = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        for task in &tasks {
            if should_cancel() {
                return Err(AppError::Cancelled);
            }
            let still_plain_file = match std::fs::symlink_metadata(&task.src) {
                Ok(m) => m.is_file() && !self.is_reparse_point(&task.src),
                Err(_) => false,
            };
            if !still_plain_file {
                continue;
            }
            let actual = copy_file_counted(&task.src, &task.dst, should_cancel)?;
            completed_bytes = completed_bytes.saturating_add(actual);
            completed_files += 1;
            copied_entries.push(ManifestEntry {
                rel_path: task.rel_path.clone(),
                is_dir: false,
                size: actual,
                mtime: 0,
                attrs: 0,
            });
            let dst_len = std::fs::symlink_metadata(&task.dst)?.len();
            dst_entries.push(ManifestEntry {
                rel_path: task.rel_path.clone(),
                is_dir: false,
                size: dst_len,
                mtime: 0,
                attrs: 0,
            });
            if last_emit.elapsed() >= Duration::from_millis(100)
                || completed_files == total_files
            {
                on_progress(&CopyProgress {
                    phase: CopyPhase::Copying,
                    completed_bytes,
                    total_bytes: Some(total_bytes),
                    completed_files,
                    total_files: Some(total_files),
                    current_path: Some(PathBuf::from(&task.rel_path)),
                });
                last_emit = Instant::now();
            }
        }
        if total_files == 0 {
            on_progress(&CopyProgress {
                phase: CopyPhase::Copying,
                completed_bytes: total_bytes,
                total_bytes: Some(total_bytes),
                completed_files: 0,
                total_files: Some(0),
                current_path: None,
            });
        }

        Ok(CopyOutcome {
            copied_manifest: Manifest {
                root: src.to_string_lossy().into(),
                entries: copied_entries,
            },
            dst_manifest: Manifest {
                root: dst.to_string_lossy().into(),
                entries: dst_entries,
            },
            total_bytes,
            total_files,
        })
    }
```

- [ ] **步骤 7：运行 `resolve_concurrency` 测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- resolve_concurrency`
预期：PASS（两个测试通过）。此时项目整体**尚未编译通过**（trait 签名变了，调用点未适配）——这是预期的，由步骤 8-13 修复。

- [ ] **步骤 8：改 `MockOps::copy_tree` 返回 `CopyOutcome`**

把 `src-tauri/src/migrator.rs:889-918` 的 `MockOps::copy_tree` 实现替换为（签名跟随 trait 改；返回空 manifest 的 `CopyOutcome`，c' diff 为空走"源没变"分支；任务3 再细化 manifest 注入）：

```rust
        fn copy_tree(
            &self,
            s: &Path,
            _d: &Path,
            _p: &dyn Fn(&crate::file_ops::CopyProgress),
            _cancel: &(dyn Fn() -> bool + Sync),
            _concurrency: usize,
        ) -> AppResult<crate::file_ops::CopyOutcome> {
            self.copy_src_calls.borrow_mut().push(s.to_path_buf());
            // 区分阶段：复制阶段 src 是 plan.src，增量阶段 src 是 plan.old_path（含 dayu-old-）
            let is_incremental_stage = s
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.contains("dayu-old-"))
                .unwrap_or(false);
            let ok = if is_incremental_stage {
                self.incremental_copy_ok
            } else {
                self.copy_ok
            };
            if ok {
                Ok(crate::file_ops::CopyOutcome {
                    copied_manifest: Manifest { root: String::new(), entries: vec![] },
                    dst_manifest: Manifest { root: String::new(), entries: vec![] },
                    total_bytes: 0,
                    total_files: 0,
                })
            } else {
                Err(crate::error::AppError::Migrate(
                    if is_incremental_stage { "incremental copy fail" } else { "copy fail" }.into(),
                ))
            }
        }
```

- [ ] **步骤 9：`MigratePlan` 新增 `copy_concurrency` 字段，补全部构造点**

在 `src-tauri/src/migrator.rs:143` 的 `MigratePlan` 结构体里 `pub enable_vss: bool,` 之后新增一行：

```rust
    /// 复制并发度覆盖。`None` 走默认（min(逻辑核数, 8)）。当前不暴露前端配置。
    pub copy_concurrency: Option<NonZeroUsize>,
```

在 `src-tauri/src/migrator.rs` 顶部 import 块（约 `:1-11`）补 `use std::num::NonZeroUsize;`。

补三个 struct literal（每处在最后一个字段后、`}` 前加一行）：

- `src-tauri/src/migrator.rs:791` 附近 `plan_for` 的 `MigratePlan { ... }`，在 `enable_vss: false,` 之后加 `copy_concurrency: None,`。
- `src-tauri/src/commands.rs:357` 附近 `MigratePlan { ... }`，在 `enable_vss,` 之后加 `copy_concurrency: None,`。
- `src-tauri/tests/e2e_migration.rs:37` 附近 `MigratePlan { ... }`，在 `enable_vss: false,` 之后加 `copy_concurrency: None,`。

- [ ] **步骤 10：migrator 阶段 a'——用 `CopyOutcome` 替换旧复制块**

把 `src-tauri/src/migrator.rs:222-252` 的整段 `if let Err(e) = ops.copy_tree(...) { ... }` 替换为下面这段。关键变化：`copy_tree` 返回 `outcome`；`bail` 包裹保留；取消/失败回滚路径不变。

```rust
    let outcome = match ops.copy_tree(
        resolver.read(),
        &plan.tmp,
        &|progress| {
            on_progress(transfer_event(
                &plan.task_id,
                stage::COPYING,
                (0, 60),
                progress,
                "正在统计待复制内容",
                "正在复制到迁移仓库",
            ))
        },
        &|| cancel.load(std::sync::atomic::Ordering::Relaxed),
        resolve_concurrency(plan.copy_concurrency),
    ) {
        Ok(o) => o,
        Err(e) => {
            let _ = ops.remove_tree(&plan.tmp);
            if matches!(e, AppError::Cancelled) {
                bail(
                    journal.cancel(&plan.task_id),
                    source_changed,
                    "migrate_rolled_back",
                )?;
            } else {
                bail(
                    journal.fail(&plan.task_id, "复制失败"),
                    source_changed,
                    "migrate_rolled_back",
                )?;
            }
            return Err(fail(e, source_changed, "migrate_rolled_back"));
        }
    };
```

并在 `migrator.rs` 顶部 import 块补 `use crate::file_ops::resolve_concurrency;`。

- [ ] **步骤 11：migrator 阶段 b'——用 outcome 两份 manifest diff 替换双 manifest**

把 `src-tauri/src/migrator.rs:267-301` 的整段（从 `// 阶段 b：首次校验` 注释到 `mark_stage "manifest_ok"` 结束）替换为：

```rust
    // 阶段 b'：复核——零额外目录遍历，直接 diff 复制的两份 outcome manifest。
    emit(stage::VERIFYING, 60, "校验 manifest");
    if !ops
        .diff_manifests(&outcome.copied_manifest, &outcome.dst_manifest)
        .is_empty()
    {
        // 保留 tmp 供排查
        bail(
            journal.fail(&plan.task_id, "manifest 不一致"),
            source_changed,
            "migrate_rolled_back",
        )?;
        return Err(fail(
            AppError::Migrate("manifest 不一致，已保留 tmp 待人工确认".into()),
            source_changed,
            "migrate_rolled_back",
        ));
    }
    bail(
        journal.mark_stage(&plan.task_id, "copied"),
        source_changed,
        "migrate_rolled_back",
    )?;
    bail(
        journal.mark_stage(&plan.task_id, "manifest_ok"),
        source_changed,
        "migrate_rolled_back",
    )?;
```

- [ ] **步骤 12：migrator 阶段 c 增量块——仅补参数（c' 重组留给任务3）**

任务1 只改签名适配，不改 c 的语义。把 `src-tauri/src/migrator.rs:334-348` 的 `ops.copy_tree(&plan.old_path, &plan.tmp, &|progress| {...}, &|| cancel.load(...))` 调用补第五参数 `1`（即原四参调用尾加 `, 1`）。该块是 `if let Err(e) = ops.copy_tree(...) { ... }`，**仅给 `copy_tree` 实参表末尾加 `1`**，失败处理逻辑不动。

具体定位：`src-tauri/src/migrator.rs:347` 行 `&|| cancel.load(std::sync::atomic::Ordering::Relaxed),` 之后、该 `copy_tree(...)` 闭合 `)` 之前，加 `,\n            1,`。

- [ ] **步骤 13：restore 的 `copy_tree` 调用补参数**

把 `src-tauri/src/migrator.rs:589-603` 的 `ops.copy_tree(resolver.read(), &restore_tmp, &|progress| {...}, &|| cancel.load(...))` 调用末尾补 `, resolve_concurrency(None)`（restore 不读 plan.copy_concurrency，用默认并发加速）。该块同样仅加实参，失败处理不动。

- [ ] **步骤 14：适配 `file_ops.rs` 测试调用点并编译检查**

`file_ops.rs` 测试模块有 5 处 `copy_tree` 调用缺第五参数 `concurrency`，签名变更后会编译失败。给每处调用末尾加 `, 1`（串行、确定性）：

- `src-tauri/src/file_ops.rs:484`：`ops().copy_tree(&src, &dst, &|_| {}, &|| false, 1).unwrap();`
- `src-tauri/src/file_ops.rs:500-506`：`.copy_tree(&src, &dst, &|progress|..., &|| false, 1)`（在 `&|| false` 后加 `, 1`，原调用跨多行，仅加末尾参数）
- `src-tauri/src/file_ops.rs:533`：`ops().copy_tree(&src, &dst, &|_| {}, &|| true, 1)`
- `src-tauri/src/file_ops.rs:551`：`ops().copy_tree(&src, &dst, &|_| {}, &|| false, 1).unwrap();`
- `src-tauri/src/file_ops.rs:565`：`ops().copy_tree(&src, &dst, &|_| {}, &|| false, 1).unwrap();`

然后运行：`cargo check --manifest-path src-tauri/Cargo.toml`
预期：编译通过。常见报错与对策：
- `MockOps::copy_tree` 签名不匹配 -> 回步骤 8 核对 `Sync` 与 `concurrency`。
- `cannot find type 'NonZeroUsize'` -> 回步骤 9 核对 migrator.rs 顶部 import。
- `copy_tree` 调用实参数量不符 -> 步骤 12/13/14 核对第五参数。

- [ ] **步骤 16：运行现有 lib 测试验证回归**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- migrator`
预期：`migrator` 模块现有测试全 PASS。重点验证：
- `migrate_success_creates_junction_records_and_logs` 用 `RealFileOps`（`migrator.rs:802`），`plan_for` 写 `a.txt`=5 字节；新串行 `copy_tree` 扫描得 `total_bytes=5`/`total_files=1`，复制阶段发 Copying 事件含 `total_bytes==Some(5)`/`total_files==Some(1)`，最终 transfer percent=60（`<=60`），末事件 percent=100。✓


运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- file_ops::tests`
预期：`resolve_concurrency` 两测试 PASS；`copy_tree_*` 与 `manifest_then_diff_*` 等用 `RealFileOps` 的测试全 PASS（串行实现行为等价）。

运行：`cargo test --manifest-path src-tauri/Cargo.toml --test e2e_migration`
预期：`full_pipeline_migrate_then_restore_preserves_data` PASS。

- [ ] **步骤 17：Commit**

```powershell
git add src-tauri/src/file_ops.rs src-tauri/src/migrator.rs src-tauri/src/commands.rs src-tauri/tests/e2e_migration.rs
git commit -m "refactor(migrate): CopyOutcome + 单遍复制两阶段串行实现 + 签名适配

- file_ops: 新增 CopyOutcome/CopyTask/resolve_concurrency/copy_file_counted；
  copy_tree 返回 CopyOutcome，should_cancel 加 Sync，新增 concurrency 参数；
  删除 measure_tree 与 copy_file_with_control（被两阶段流程取代）。
- migrator: MigratePlan 增 copy_concurrency 字段；阶段 a/b 重组为 a'/b'
  （outcome 双 manifest diff，零额外遍历）；c 与 restore 调用补 concurrency 参数。
- MockOps/commands/e2e: 构造点补 copy_concurrency: None。"
```

## 任务 2：并发执行器（`thread::scope` + 任务队列 + 进度/错误/取消）

**目标：** 把任务1 的串行阶段②改为 N worker 并发：`std::thread::scope` 内 N 个 worker 从 `Arc<Mutex<VecDeque<CopyTask>>>` 取任务，进度用 `AtomicU64`/`AtomicUsize` 累加并由主线程 100ms 汇报器（带 `min(actual,total)` clamp）发出，错误用 `Mutex<Option<AppError>>` 首错 + `AtomicBool stop`，取消由主线程与 worker 共同检查。`copy_tree` 签名不变，`concurrency` 参数生效。

**文件：** 修改 `src-tauri/src/file_ops.rs`（仅 `RealFileOps::copy_tree` 阶段② + 顶部 import）；新增并发确定性测试。

- [ ] **步骤 1：编写并发确定性失败测试**

在 `src-tauri/src/file_ops.rs` 的 `#[cfg(test)] mod tests` 内新增（验证并发结果与串行一致）：

```rust
    #[test]
    fn copy_tree_concurrent_matches_serial_for_many_small_files() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        for i in 0..200 {
            std::fs::write(src.join(format!("f{i}.txt")), vec![i as u8; 64]).unwrap();
        }
        for i in 0..50 {
            std::fs::write(src.join("a/b/g{i}.txt"), vec![9u8; 128]).unwrap();
        }
        // 多 worker
        let dst_multi = root.path().join("dst_multi");
        let outcome_multi = ops()
            .copy_tree(&src, &dst_multi, &|_| {}, &|| false, 8)
            .unwrap();
        // 单 worker
        let dst_one = root.path().join("dst_one");
        let outcome_one = ops()
            .copy_tree(&src, &dst_one, &|_| {}, &|| false, 1)
            .unwrap();
        // 两份 outcome manifest 各自内部一致
        assert!(
            ops().diff_manifests(&outcome_multi.copied_manifest, &outcome_multi.dst_manifest).is_empty(),
            "多 worker 下 copied 与 dst manifest 必须一致"
        );
        // 并发与串行的 copied manifest 一致（条目集合相同）
        assert_eq!(outcome_multi.total_files, outcome_one.total_files);
        assert_eq!(outcome_multi.total_bytes, outcome_one.total_bytes);
        // 内容抽检
        assert_eq!(std::fs::read(dst_multi.join("f0.txt")).unwrap(), vec![0u8; 64]);
        assert_eq!(std::fs::read(dst_multi.join("a/b/g0.txt")).unwrap(), vec![9u8; 128]);
    }
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- copy_tree_concurrent_matches_serial_for_many_small_files`
预期：PASS（任务1 的串行实现 `concurrency` 被忽略，8 与 1 结果相同）。此测试是**回归护栏**——任务2 改并发后仍必须通过。若任务1 已通过，本步骤确认护栏就位。

- [ ] **步骤 3：在 `file_ops.rs` 顶部补并发原语 import**

把 `file_ops.rs:1-5` 的 import 块替换为：

```rust
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
```

- [ ] **步骤 4：重写阶段②为并发实现**

把任务1 步骤 6 中 `RealFileOps::copy_tree` 的**阶段②**部分（从注释 `// 阶段②（任务1：串行；任务2 改并发）...` 到 `Ok(CopyOutcome { ... })` 之前，即整个阶段②的 `for task in &tasks { ... }` 串行循环 + `if total_files == 0 { ... }` 块）替换为下面的并发版本。阶段①（扫描建树）保持不变。

```rust
        // 阶段②：并发复制。目录已在阶段①串行建好，worker 只往已存在目录填文件，无 create 竞态。
        let actual_bytes = AtomicU64::new(0);
        let actual_files = AtomicUsize::new(0);
        let copied_files: Arc<Mutex<Vec<ManifestEntry>>> = Arc::new(Mutex::new(Vec::with_capacity(tasks.len())));
        let dst_files: Arc<Mutex<Vec<ManifestEntry>>> = Arc::new(Mutex::new(Vec::with_capacity(tasks.len())));
        let first_error: Arc<Mutex<Option<AppError>>> = Arc::new(Mutex::new(None));
        let stop = AtomicBool::new(false);
        let queue: Arc<Mutex<VecDeque<CopyTask>>> = Arc::new(Mutex::new(tasks.into_iter().collect()));

        let n = concurrency.max(1);
        let active = AtomicUsize::new(n);

        std::thread::scope(|s| {
            for _ in 0..n {
                let queue = Arc::clone(&queue);
                let copied_files = Arc::clone(&copied_files);
                let dst_files = Arc::clone(&dst_files);
                let first_error = Arc::clone(&first_error);
                let ops = self;
                s.spawn(move || loop {
                    if stop.load(Ordering::Relaxed) || should_cancel() {
                        active.fetch_sub(1, Ordering::Relaxed);
                        break;
                    }
                    let task = queue.lock().unwrap().pop_front();
                    let Some(task) = task else {
                        active.fetch_sub(1, Ordering::Relaxed);
                        break;
                    };
                    // 打开前重新检查：已不存在或已不再是普通文件（含变 reparse/目录）则跳过，
                    // 交由 c' 对账，不算复制错误。
                    let still_plain = match std::fs::symlink_metadata(&task.src) {
                        Ok(m) => m.is_file() && !ops.is_reparse_point(&task.src),
                        Err(_) => false,
                    };
                    if !still_plain {
                        continue;
                    }
                    match copy_file_counted(&task.src, &task.dst, should_cancel) {
                        Ok(actual) => {
                            actual_bytes.fetch_add(actual, Ordering::Relaxed);
                            actual_files.fetch_add(1, Ordering::Relaxed);
                            copied_files.lock().unwrap().push(ManifestEntry {
                                rel_path: task.rel_path.clone(),
                                is_dir: false,
                                size: actual,
                                mtime: 0,
                                attrs: 0,
                            });
                            match std::fs::symlink_metadata(&task.dst) {
                                Ok(m) => dst_files.lock().unwrap().push(ManifestEntry {
                                    rel_path: task.rel_path,
                                    is_dir: false,
                                    size: m.len(),
                                    mtime: 0,
                                    attrs: 0,
                                }),
                                Err(e) => {
                                    let mut err = first_error.lock().unwrap();
                                    if err.is_none() {
                                        *err = Some(AppError::from(e));
                                    }
                                    stop.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(e) => {
                            let mut err = first_error.lock().unwrap();
                            if err.is_none() {
                                *err = Some(e);
                            }
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                });
            }
            // 主线程进度汇报器：每 100ms 读原子值回调 on_progress，completed clamp 到 total。
            while active.load(Ordering::Relaxed) > 0 {
                if should_cancel() {
                    stop.store(true, Ordering::Relaxed);
                }
                let done_bytes = actual_bytes.load(Ordering::Relaxed).min(total_bytes);
                let done_files = (actual_files.load(Ordering::Relaxed) as u64).min(total_files);
                on_progress(&CopyProgress {
                    phase: CopyPhase::Copying,
                    completed_bytes: done_bytes,
                    total_bytes: Some(total_bytes),
                    completed_files: done_files,
                    total_files: Some(total_files),
                    current_path: None,
                });
                std::thread::sleep(Duration::from_millis(100));
                // 若队列已空且无 worker 活跃，主线程也退出（避免空转）。
                if queue.lock().unwrap().is_empty()
                    && active.load(Ordering::Relaxed) == 0
                {
                    break;
                }
            }
            // scope 返回前 join 所有 worker；worker 退出时通过 active 计数衰减。
            // （scope 自动 join，此处无需显式 join。）
        });
```

> **ctive 退出语义：** worker 闭包是 loop { ... }，仅通过上面两处 reak 退出，每处退出前都已 ctive.fetch_sub(1)。主线程汇报循环 while active > 0 因此能在所有 worker 退出后收敛。

- [ ] **步骤 5：在阶段②之后、`Ok(CopyOutcome)` 之前，处理结果与最终进度**

紧接步骤 4 的 `std::thread::scope(...)` 之后，补上结果归并与最终进度/错误返回（替换任务1 原阶段②末尾的 `Ok(CopyOutcome{...})`）：

```rust
        // 最终一次进度（确保小文件快速完成时也至少发出一个 Copying 终态事件）。
        let done_bytes = actual_bytes.load(Ordering::Relaxed).min(total_bytes);
        let done_files = (actual_files.load(Ordering::Relaxed) as u64).min(total_files);
        on_progress(&CopyProgress {
            phase: CopyPhase::Copying,
            completed_bytes: done_bytes,
            total_bytes: Some(total_bytes),
            completed_files: done_files,
            total_files: Some(total_files),
            current_path: None,
        });

        // 错误/取消优先于成功。
        if let Some(e) = first_error.lock().unwrap().take() {
            return Err(e);
        }
        if should_cancel() {
            return Err(AppError::Cancelled);
        }

        copied_entries.extend(copied_files.lock().unwrap().drain(..));
        dst_entries.extend(dst_files.lock().unwrap().drain(..));

        Ok(CopyOutcome {
            copied_manifest: Manifest {
                root: src.to_string_lossy().into(),
                entries: copied_entries,
            },
            dst_manifest: Manifest {
                root: dst.to_string_lossy().into(),
                entries: dst_entries,
            },
            total_bytes,
            total_files,
        })
```

> **`copied_entries`/`dst_entries` 可变性：** 阶段①里它们是 `let mut`；阶段②并发后只在最后 `extend`，仍需 `mut`。任务1 步骤 6 已声明为 `let mut copied_entries` / `let mut dst_entries`，无需改动。

- [ ] **步骤 6：编译检查**

运行：`cargo check --manifest-path src-tauri/Cargo.toml`
预期：编译通过。常见问题：`active` 未使用警告（步骤 4 已用）、`should_cancel` 借用进 `thread::scope`（它是 `&(dyn Fn()->bool+Sync)`，`'scope` 内可用，OK）、`CopyTask` 未实现 `Send`（其字段 `PathBuf`/`String` 均 `Send`，OK）。

- [ ] **步骤 7：运行并发与回归测试**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- copy_tree_concurrent_matches_serial_for_many_small_files`
预期：PASS（并发结果与串行一致）。

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- file_ops::tests`
预期：全 PASS。`copy_tree_reports_real_transfer_totals` 在多核下可能收到多次 Copying 中间事件，但最后一个 Copying 事件 `completed_bytes==3072`/`completed_files==2` 不变。

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- migrator`
预期：全 PASS（migrator 未变）。

- [ ] **步骤 8：Commit**

```powershell
git add src-tauri/src/file_ops.rs
git commit -m "feat(migrate): copy_tree 文件级并发（thread::scope + 任务队列）

阶段②改为 N worker 并发：共享 VecDeque 任务队列，AtomicU64/AtomicUsize
累加进度并由主线程 100ms 汇报器带 clamp 发出，首错 Mutex+AtomicBool stop
传播，worker 打开前重新校验文件类型（源变化交 c' 对账）。"
```

## 任务 3：migrator c' 重组（源变化条件补传）+ MockOps 改造 + 测试更新

**目标：** 把任务1 保留的 c' 旧"无条件全量增量"逻辑重组为规格 §5 的 c'：改名源后扫一次 `old_path`，用 `diff(copied_manifest, old_manifest)` 判断源是否变化——空则跳过补传（常态零数据读取），非空则清空 tmp 全量补传并做 patch diff + final `old_path` 复核。改造 MockOps 以注入"源变化"语义，新增 c' 降级测试，并更新两个受影响的现有测试。

**文件：**
- 修改 `src-tauri/src/migrator.rs`：`migrate_with_snapshot` 的 c' else 分支（生产代码）；`MockOps`（struct/success_path/copy_tree/manifest/diff_manifests，测试代码）；`migrate_source_changed_tracking` 与 `non_vss_migrate_reads_original_and_writes_original`（测试断言）。

- [ ] **步骤 1：改造 MockOps 以支持 c' 源变化注入**

MockOps 当前的 `copy_tree` 返回空 manifest、`manifest` 返回空、`diff_manifests` 用纯 `manifest_ok` 旋钮——无法让 b' 通过的同时让 c' 判定"源变了"。改造为：`copy_tree` 返回哨兵 manifest（copied 与 dst 相同，b' 真实 diff 为空）；`manifest(old_path)` 用调用计数区分 c' 增量前/补传后，按 `source_changed_during_copy`/`source_keeps_changing` 返回空或哨兵制造 diff；`diff_manifests` 在 `manifest_ok=true` 时委托真实比较（与 `RealFileOps` 等价），`false` 时强制非空。

**1a.** 在 `MockOps` struct（`migrator.rs:849` 附近）的 `recycle_ok: bool,` 之后新增两个字段，并在 `rename_calls` 之后新增计数字段：

```rust
        /// 复制期间源是否变化：true 时 c' 判定需增量补传。
        source_changed_during_copy: bool,
        /// 补传期间源是否继续变化：true 时 final 复核不一致 -> 回滚改名。
        source_keeps_changing: bool,
```

并在 struct 末尾 `rename_calls: RefCell<Vec<(std::path::PathBuf, std::path::PathBuf)>>,` 之后加：

```rust
        /// manifest(old_path) 调用计数：区分 c' 增量前(第1次)与 final(第2次)。
        manifest_old_count: RefCell<usize>,
```

**1b.** 在 `MockOps::success_path`（`migrator.rs:871` 附近）的 `recycle_ok: true,` 之后、`create_junction_calls:` 之前加：

```rust
                source_changed_during_copy: false,
                source_keeps_changing: false,
```

并在 `rename_calls: RefCell::new(vec![]),` 之后加：

```rust
                manifest_old_count: RefCell::new(0),
```

**1c.** 在 `impl MockOps` 之前（或 tests 模块内合适位置）新增哨兵 helper：

```rust
    /// 与 MockOps::copy_tree 返回的 copied_manifest 内容一致的哨兵条目，
    /// 用于让 b'(copied vs dst) 与 c' 源未变(copied vs old) 的真实 diff 为空。
    fn mock_sentinel_manifest() -> Manifest {
        Manifest {
            root: String::new(),
            entries: vec![crate::file_ops::ManifestEntry {
                rel_path: "mocked.txt".into(),
                is_dir: false,
                size: 1,
                mtime: 0,
                attrs: 0,
            }],
        }
    }
```

**1d.** 把任务1 步骤 8 写入的 `MockOps::copy_tree`（返回空 manifest 那版）整体替换为：

```rust
        fn copy_tree(
            &self,
            s: &Path,
            _d: &Path,
            _p: &dyn Fn(&crate::file_ops::CopyProgress),
            _cancel: &(dyn Fn() -> bool + Sync),
            _concurrency: usize,
        ) -> AppResult<crate::file_ops::CopyOutcome> {
            self.copy_src_calls.borrow_mut().push(s.to_path_buf());
            let is_incremental_stage = s
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.contains("dayu-old-"))
                .unwrap_or(false);
            let ok = if is_incremental_stage {
                self.incremental_copy_ok
            } else {
                self.copy_ok
            };
            if ok {
                Ok(crate::file_ops::CopyOutcome {
                    copied_manifest: mock_sentinel_manifest(),
                    dst_manifest: mock_sentinel_manifest(),
                    total_bytes: 0,
                    total_files: 0,
                })
            } else {
                Err(crate::error::AppError::Migrate(
                    if is_incremental_stage { "incremental copy fail" } else { "copy fail" }.into(),
                ))
            }
        }
```

**1e.** 把 `MockOps::manifest`（`migrator.rs:919` 附近）替换为按计数分流版本：

```rust
        fn manifest(&self, s: &Path) -> AppResult<Manifest> {
            self.manifest_calls.borrow_mut().push(s.to_path_buf());
            let is_old = s
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.contains("dayu-old-"))
                .unwrap_or(false);
            if is_old {
                let mut count = self.manifest_old_count.borrow_mut();
                *count += 1;
                // 第 1 次 = c' 增量前；第 2 次 = 补传后 final 复核。
                let changed = if *count == 1 {
                    self.source_changed_during_copy
                } else {
                    self.source_keeps_changing
                };
                if changed {
                    Ok(Manifest { root: String::new(), entries: vec![] })
                } else {
                    Ok(mock_sentinel_manifest())
                }
            } else {
                Ok(mock_sentinel_manifest())
            }
        }
```

**1f.** 把 `MockOps::diff_manifests`（`migrator.rs:926` 附近）替换为"`manifest_ok=false` 强制非空，否则委托真实比较"：

```rust
        fn diff_manifests(&self, a: &Manifest, b: &Manifest) -> Vec<String> {
            if !self.manifest_ok {
                return vec!["f.txt".into()];
            }
            // 委托真实比较（与 RealFileOps::diff_manifests 等价：仅比 rel_path/is_dir/size）。
            use std::collections::{HashMap, HashSet};
            let map_a: HashMap<&str, &crate::file_ops::ManifestEntry> =
                a.entries.iter().map(|e| (e.rel_path.as_str(), e)).collect();
            let map_b: HashMap<&str, &crate::file_ops::ManifestEntry> =
                b.entries.iter().map(|e| (e.rel_path.as_str(), e)).collect();
            let mut keys: HashSet<&str> = map_a.keys().copied().collect();
            keys.extend(map_b.keys().copied());
            let mut diffs = Vec::new();
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
```

> **回归核对：** 改造后，`manifest_ok=false` 仍让 b'（`diff(copied, dst)`）失败；`manifest_ok=true` 时 b' 的哨兵自比为空、c' 源未变时 `copied` 与 `old` 均哨兵自比为空——所有现有 MockOps 测试在新 mock 下语义不变（步骤 5 会跑回归确认）。

- [ ] **步骤 2：编写 c' 降级路径的失败测试**

在 migrator tests 模块新增（用改造后的 MockOps 注入源变化）。这些测试在 migrator c' 仍是旧逻辑时**应失败**（旧逻辑无条件全量增量，不区分源是否变化，也不会触发"补传期间变化"回滚）：

```rust
    #[test]
    fn migrate_c_prime_skips_sync_when_source_unchanged() {
        // 源没变：c' 应跳过补传，不调用增量 copy_tree(old_path)。
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "cprime-skip");
        let ops = MockOps::success_path(); // source_changed_during_copy = false
        let cancel = AtomicBool::new(false);
        migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel).unwrap();
        let copies = ops.copy_src_calls.borrow();
        assert!(
            !copies.iter().any(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("dayu-old-"))
            }),
            "源未变时 c' 不应调用增量 copy_tree(old_path)，实参: {copies:?}"
        );
    }

    #[test]
    fn migrate_c_prime_incremental_fail_rolls_back_rename() {
        // 源变了 + 增量 copy_tree 失败 -> 回滚改名，source_changed=true，migrate_partial。
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "cprime-incfail");
        let ops = {
            let mut o = MockOps::success_path();
            o.source_changed_during_copy = true;
            o.incremental_copy_ok = false;
            o
        };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        let (_e, outcome) = res.expect_err("源变+增量失败应回滚");
        assert!(outcome.source_changed, "已改名，source_changed=true");
        assert_eq!(outcome.reason, "migrate_partial");
    }

    #[test]
    fn migrate_c_prime_source_keeps_changing_rolls_back_rename() {
        // 源变了、增量成功、但补传期间源再变 -> final 复核不一致 -> 回滚改名。
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "cprime-keepchanging");
        let ops = {
            let mut o = MockOps::success_path();
            o.source_changed_during_copy = true;
            o.source_keeps_changing = true; // 补传后 final old_manifest 为空 -> diff 非空
            o
        };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        let (_e, outcome) = res.expect_err("补传期间源再变应回滚");
        assert!(outcome.source_changed, "已改名，source_changed=true");
        assert_eq!(outcome.reason, "migrate_partial");
    }

    #[test]
    fn migrate_c_prime_source_changed_then_stable_succeeds() {
        // 源变了、增量补传成功、补传后源稳定 -> 最终建链成功。
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "cprime-stable");
        let ops = {
            let mut o = MockOps::success_path();
            o.source_changed_during_copy = true;
            o.source_keeps_changing = false; // final old_manifest = 哨兵 -> diff 空
            o
        };
        let cancel = AtomicBool::new(false);
        let (m, outcome) = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel)
            .expect("源变后稳定补传应成功");
        assert!(outcome.source_changed);
        assert_eq!(outcome.reason, "migrated");
        assert_eq!(m.status, MigrationStatus::Active);
    }
```

- [ ] **步骤 3：运行 c' 测试验证失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- migrate_c_prime`
预期：FAIL。`migrate_c_prime_skips_sync_when_source_unchanged` 在旧逻辑下仍会调用 `copy_tree(old_path)`（断言失败）；其余三个在旧逻辑下要么不触发预期分支、要么 panic 于 `expect_err`/`expect`。这确认测试就位且当前实现未重组。

- [ ] **步骤 4：重组 migrator c' 的 else 分支**

把 `src-tauri/src/migrator.rs` 中 c' 的 `else { ... }` 分支整体替换。该分支当前（任务1 后）是任务1 步骤 12 保留的旧逻辑：从 `emit(stage::SYNCING, 80, "准备同步复制期间的变化");` 开始，到该 `else` 块结束的 `bail(journal.mark_stage(&plan.task_id, "incremental_synced"), source_changed, "migrate_partial")?;` 为止。替换为：

```rust
        emit(stage::SYNCING, 80, "检查复制期间的变化");
        let old_manifest = bail(
            ops.manifest(&plan.old_path),
            source_changed,
            "migrate_partial",
        )?;
        if ops
            .diff_manifests(&outcome.copied_manifest, &old_manifest)
            .is_empty()
        {
            // 源没变 -> 跳过补传（常态，省掉旧的全量重读）。
        } else {
            // 源变了（罕见）-> 先清空旧 tmp，再在空目录中全量补传。
            // 不能原地覆盖：old_path 中已删除的条目必须从 tmp 消失。
            if let Err(e) = ops.remove_tree(&plan.tmp) {
                let _ = ops.rename(&plan.old_path, &plan.src);
                bail(
                    journal.fail(&plan.task_id, "清空临时目录失败"),
                    source_changed,
                    "migrate_partial",
                )?;
                return Err(fail(e, source_changed, "migrate_partial"));
            }
            let patch = match ops.copy_tree(
                &plan.old_path,
                &plan.tmp,
                &|progress| {
                    on_progress(transfer_event(
                        &plan.task_id,
                        stage::SYNCING,
                        (80, 90),
                        progress,
                        "正在检查复制期间的变化",
                        "正在同步复制期间的变化",
                    ))
                },
                &|| cancel.load(std::sync::atomic::Ordering::Relaxed),
                resolve_concurrency(plan.copy_concurrency),
            ) {
                Ok(p) => p,
                Err(e) => {
                    // 回滚：改回原名
                    let _ = ops.rename(&plan.old_path, &plan.src);
                    if matches!(e, AppError::Cancelled) {
                        let _ = ops.remove_tree(&plan.tmp);
                        bail(
                            journal.cancel(&plan.task_id),
                            source_changed,
                            "migrate_partial",
                        )?;
                    } else {
                        bail(
                            journal.fail(&plan.task_id, "增量同步失败"),
                            source_changed,
                            "migrate_partial",
                        )?;
                    }
                    return Err(fail(e, source_changed, "migrate_partial"));
                }
            };
            if !ops
                .diff_manifests(&patch.copied_manifest, &patch.dst_manifest)
                .is_empty()
            {
                let _ = ops.rename(&plan.old_path, &plan.src);
                bail(
                    journal.fail(&plan.task_id, "二次校验不一致"),
                    source_changed,
                    "migrate_partial",
                )?;
                return Err(fail(
                    AppError::Migrate("增量后 manifest 不一致".into()),
                    source_changed,
                    "migrate_partial",
                ));
            }
            // old_path 在改名后理论上不再被常规路径写入；仍复核一次，
            // 保持现有"补传期间发生变化则回滚"的安全语义。
            let final_old_manifest = bail(
                ops.manifest(&plan.old_path),
                source_changed,
                "migrate_partial",
            )?;
            if !ops
                .diff_manifests(&patch.copied_manifest, &final_old_manifest)
                .is_empty()
            {
                let _ = ops.rename(&plan.old_path, &plan.src);
                bail(
                    journal.fail(&plan.task_id, "补传期间源发生变化"),
                    source_changed,
                    "migrate_partial",
                )?;
                return Err(fail(
                    AppError::Migrate("补传期间源发生变化".into()),
                    source_changed,
                    "migrate_partial",
                ));
            }
        }
        bail(
            journal.mark_stage(&plan.task_id, "incremental_synced"),
            source_changed,
            "migrate_partial",
        )?;
```

> **`outcome` 作用域：** `outcome` 由阶段 a'（任务1 步骤 10）的 `let outcome = match {...}` 绑定，其作用域覆盖整个 `migrate_with_snapshot` 函数体直到末尾，c' 可直接引用 `outcome.copied_manifest`。无需额外传递。

> **journal 阶段名不变：** 仍是 `incremental_synced`，与现有 `recover_pending_decisions` 兼容；`source_changed` 跟踪规则不变（改名前 false、改名后 true；补传失败=true 回滚改名）。VSS 分支（`if resolver.vss_enabled()`）完全不动。

- [ ] **步骤 5：运行 c' 测试验证通过**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- migrate_c_prime`
预期：四个测试全 PASS。

- [ ] **步骤 6：更新受影响的现有测试**

**6a.** `migrate_source_changed_tracking`（`migrator.rs:1241` 附近的"增量同步失败"分支）：旧逻辑下 `incremental_copy_ok=false` 单独即可触发增量失败；c' 重组后必须先让 c' 进入增量分支。在该分支的 mock 构造里补 `source_changed_during_copy`。把：

```rust
        let ops = {
            let mut o = MockOps::success_path();
            o.incremental_copy_ok = false;
            o
        };
```

改为：

```rust
        let ops = {
            let mut o = MockOps::success_path();
            o.source_changed_during_copy = true;
            o.incremental_copy_ok = false;
            o
        };
```

（仅此一处"增量同步失败"分支改动；该测试其余分支不动。）

**6b.** `non_vss_migrate_reads_original_and_writes_original`（`migrator.rs:1501` 附近）：c' 重组后源未变时**不**调用 `copy_tree(old_path)`，改为调用 `manifest(old_path)`。把断言：

```rust
        // old_path->src 的回滚不存在（成功路径），但 old_path->tmp 的增量 copy 首参是 old_path。
        assert!(
            copy_calls.iter().any(|p| *p == old_path),
            "增量同步应读 old_path 原路径，实参: {copy_calls:?}"
        );
```

改为：

```rust
        // c' 源未变：不调用增量 copy_tree(old_path)，而是扫描 old_path 记 manifest。
        assert!(
            !copy_calls.iter().any(|p| *p == old_path),
            "源未变时 c' 不应调用 copy_tree(old_path)，实参: {copy_calls:?}"
        );
        let manifest_calls = ops.manifest_calls.borrow();
        assert!(
            manifest_calls.iter().any(|p| *p == old_path),
            "c' 应扫描 old_path 记 manifest，实参: {manifest_calls:?}"
        );
```

- [ ] **步骤 7：全量编译与回归测试**

运行：`cargo check --manifest-path src-tauri/Cargo.toml`
预期：编译通过。

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- migrator`
预期：全 PASS。重点：
- `migrate_source_changed_tracking`：增量失败分支现在 `source_changed_during_copy=true` 触发 c' 增量 -> `incremental_copy_ok=false` -> 回滚改名、`source_changed=true`、`migrate_partial`。✓ 其余分支不变。
- `non_vss_migrate_reads_original_and_writes_original`：c' 源未变跳过 `copy_tree(old_path)`、改调 `manifest(old_path)`。✓
- `migrate_aborts_when_manifest_mismatch_keeps_tmp`（`manifest_ok=false`）：b' `diff(copied, dst)` 强制返回 `["f.txt"]` -> 失败、`source_changed=false`。✓
- `migrate_success_creates_junction_records_and_logs`（RealFileOps）：c' 真实源未变 -> `diff(copied, old)` 空 -> 跳过补传 -> 建链成功。✓

运行：`cargo test --manifest-path src-tauri/Cargo.toml --test e2e_migration`
预期：`full_pipeline_migrate_then_restore_preserves_data` PASS（RealFileOps 真实 c' 跳过补传）。

- [ ] **步骤 8：Commit**

```powershell
git add src-tauri/src/migrator.rs
git commit -m "feat(migrate): c' 条件补传（源未变跳过增量，源变清空 tmp 重建）

- migrator c' 重组：改名后扫 old_path 一次，diff(copied_manifest, old_manifest)
  为空则跳过补传（常态零数据读取）；非空则 remove_tree(tmp) 后全量补传，
  并做 patch diff + final old_path 复核，变化则回滚改名。
- MockOps 改造：copy_tree 返回哨兵 manifest；manifest(old_path) 按调用计数
  与 source_changed_during_copy/source_keeps_changing 分流；diff_manifests
  在 manifest_ok=true 时委托真实比较。
- 更新 migrate_source_changed_tracking 增量分支与 non_vss_migrate_reads 断言。"
```

## 任务 4：新增测试（规格 §8 测试 2/3/5/6）

**目标：** 用 `RealFileOps` 补充并发正确性、枚举/复制竞态、取消/错误传播、进度边界的端到端测试。规格 §8 测试 1（MockOps 适配）与测试 4（c' 降级）已在任务 3 覆盖。

**文件：** 修改 `src-tauri/src/file_ops.rs` 的 `#[cfg(test)] mod tests`（新增 5 个测试）。

- [ ] **步骤 1：测试 A——空目录保留 + reparse 占位 + outcome 自洽**

在 file_ops tests 模块新增。验证多层目录、空目录被保留，内部 junction 仅占位不递归，且 `outcome` 两份 manifest 真实 diff 为空：

```rust
    #[test]
    fn copy_tree_preserves_empty_dirs_and_outcome_manifests_agree() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(src.join("empty/sub")).unwrap();
        std::fs::write(src.join("keep.txt"), b"x").unwrap();
        let dst = root.path().join("dst");
        let outcome = ops()
            .copy_tree(&src, &dst, &|_| {}, &|| false, 4)
            .unwrap();
        // 空目录被保留
        assert!(dst.join("empty/sub").is_dir(), "空目录应被复制");
        assert_eq!(std::fs::read(dst.join("keep.txt")).unwrap(), b"x");
        // 两份 outcome manifest 真实 diff 为空（复制完整性 b'）
        assert!(
            ops().diff_manifests(&outcome.copied_manifest, &outcome.dst_manifest).is_empty(),
            "copied 与 dst manifest 必须一致"
        );
        // copied manifest 应含 keep.txt（文件）与目录条目
        let rels: Vec<&str> = outcome.copied_manifest.entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(rels.contains(&"keep.txt"));
        assert!(rels.iter().any(|r| r.starts_with("empty")), "应含 empty 目录条目");
    }
```

- [ ] **步骤 2：测试 B——枚举/复制竞态（扫描后删除文件不误报）**

验证规格 §4.2 组件 1 的语义：扫描清单不是一致性基准，扫描后被删的文件由 worker 打开前重新检查跳过、交 c' 对账，不作为复制错误。用 `on_progress` 在阶段①末尾的 Preparing 事件里删除一个文件：

```rust
    #[test]
    fn copy_tree_skips_files_removed_after_scan_without_error() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("keep.txt"), b"keep").unwrap();
        let gone_path = src.join("gone.txt");
        std::fs::write(&gone_path, b"gone").unwrap();
        let dst = root.path().join("dst");
        // 阶段①扫描末尾会发一次最终 Preparing 事件；在其回调里删除 gone.txt，
        // 使阶段② worker 打开它前 symlink_metadata 检查命中"已删除"-> 跳过。
        let gone = gone_path.clone();
        let outcome = ops()
            .copy_tree(
                &src,
                &dst,
                &|p| {
                    if p.phase == CopyPhase::Preparing {
                        let _ = std::fs::remove_file(&gone);
                    }
                },
                &|| false,
                1,
            )
            .unwrap();
        // 不报错；keep.txt 正常复制
        assert_eq!(std::fs::read(dst.join("keep.txt")).unwrap(), b"keep");
        assert!(!dst.join("gone.txt").exists(), "被删文件不应出现在 dst");
        // outcome 自洽：copied 与 dst 一致（均不含 gone）
        assert!(
            ops().diff_manifests(&outcome.copied_manifest, &outcome.dst_manifest).is_empty(),
            "源变化跳过的条目不应造成 copied/dst 不一致"
        );
    }
```

- [ ] **步骤 3：测试 C——复制中途取消返回 Cancelled**

验证规格 §4.2 组件 4 的取消传播。用一个大文件 + 计数型 `should_cancel`，确保取消落在阶段②复制过程中：

```rust
    #[test]
    fn copy_tree_mid_copy_cancellation_returns_cancelled() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // 10MB 单文件：阶段①扫描 should_cancel 调用极少，阈值落在阶段②块复制中。
        std::fs::write(src.join("big.bin"), vec![0u8; 10 * 1024 * 1024]).unwrap();
        let dst = root.path().join("dst");
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = counter.clone();
        let cancel = move || {
            // 前 5 次（扫描 + 取任务 + 前几块）false，之后 true -> 块复制中途取消。
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 5
        };
        let result = ops().copy_tree(&src, &dst, &|_| {}, &cancel, 1);
        assert!(matches!(result, Err(AppError::Cancelled)), "复制中途取消应返回 Cancelled");
    }
```

- [ ] **步骤 4：测试 D——首错传播（dst 碰撞致 IO 错误）**

验证规格 §4.2 组件 4 的首错 `Mutex<Option<AppError>>` 传播。确定性构造：预先把 `dst/x.txt` 建成**目录**，worker 复制 `src/x.txt` 时 `File::create(dst/x.txt)` 因目标已是目录而失败：

```rust
    #[test]
    fn copy_tree_propagates_first_io_error() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("x.txt"), b"hello").unwrap();
        let dst = root.path().join("dst");
        std::fs::create_dir_all(dst.join("x.txt")).unwrap(); // 目标同名已是目录 -> create 失败
        let result = ops().copy_tree(&src, &dst, &|_| {}, &|| false, 1);
        assert!(result.is_err(), "dst 碰撞应作为 IO 错误传播");
        // 不应是 Cancelled；应是 Io 错误（File::create 对目录失败）
        assert!(!matches!(result, Err(AppError::Cancelled)));
    }
```

- [ ] **步骤 5：测试 E——进度 clamp（completed 永不超过 total）**

验证规格 §4.2 组件 3 的 `min(actual, total)` clamp，且进度单调（copying 阶段 completed 不回退）：

```rust
    #[test]
    fn copy_tree_progress_never_exceeds_total() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), vec![1u8; 1024]).unwrap();
        std::fs::write(src.join("b.bin"), vec![2u8; 2048]).unwrap();
        let dst = root.path().join("dst");
        let events = std::cell::RefCell::new(Vec::new());
        ops()
            .copy_tree(
                &src,
                &dst,
                &|p| events.borrow_mut().push(p.clone()),
                &|| false,
                4,
            )
            .unwrap();
        let events = events.into_inner();
        let mut last_completed = 0u64;
        for e in &events {
            if e.phase == CopyPhase::Copying {
                if let Some(total) = e.total_bytes {
                    assert!(e.completed_bytes <= total, "completed_bytes 不得超过 total_bytes");
                }
                if let Some(total) = e.total_files {
                    assert!(e.completed_files <= total, "completed_files 不得超过 total_files");
                }
                // 单调（主线程汇报器按原子累加值发出，clamp 后不减）
                assert!(e.completed_bytes >= last_completed, "进度不得回退");
                last_completed = e.completed_bytes;
            }
        }
    }
```

- [ ] **步骤 6：运行任务 4 全部测试**

运行：`cargo test --manifest-path src-tauri/Cargo.toml --lib -- copy_tree_preserves_empty_dirs_and_outcome_manifests_agree copy_tree_skips_files_removed_after_scan_without_error copy_tree_mid_copy_cancellation_returns_cancelled copy_tree_propagates_first_io_error copy_tree_progress_never_exceeds_total`
预期：5 个测试全 PASS。

> **测试 D 平台说明：** `File::create` 对一个已存在目录路径在 Windows/Linux 均返回 IO 错误，故该测试跨平台确定性。若某 CI 环境下 `create_dir_all(dst.join("x.txt"))` 行为异常，可改为预先写入 `dst/x.txt` 为文件后再 `copy_tree`（同样触发 create 截断/锁冲突），但当前实现优先用目录碰撞。

- [ ] **步骤 7：全量测试 + Commit**

运行：`cargo test --manifest-path src-tauri/Cargo.toml`
预期：全部测试 PASS（lib + integration）。

```powershell
git add src-tauri/src/file_ops.rs
git commit -m "test(migrate): 新增并发/竞态/取消/错误/进度边界测试

覆盖规格 §8 测试 2/3/5/6：空目录保留与 outcome 自洽、扫描后删文件的
枚举竞态跳过、复制中途取消、dst 碰撞首错传播、进度 clamp 与单调性。"
```

---

## 自检（writing-plans 编写者自查）

**1. 规格覆盖度**——逐条对照规格章节：

| 规格 章节/需求 | 实现任务 |
|---|---|
| §4.1 流程重组 a'/b'/c' | 任务1（a'/b'）、任务3（c'） |
| §4.2 组件1 TreeCopier 两阶段 | 任务1（串行两阶段）、任务2（阶段②并发） |
| §4.2 组件2 并发执行器（scope+队列+NonZeroUsize） | 任务2 + 任务1 `resolve_concurrency` |
| §4.2 组件3 进度聚合（Atomic+clamp+主线程汇报器） | 任务2 步骤 4/5 |
| §4.2 组件4 错误/取消传播（首错+stop+cancel） | 任务2 步骤 4/5、任务4 测试 C/D |
| §4.3 CopyOutcome + should_cancel Sync | 任务1 步骤 3/4/8 |
| §4.3 copy_concurrency 字段 + 构造点 None | 任务1 步骤 9 |
| §5 migrator 集成骨架 | 任务1（a'/b'）、任务3（c'） |
| §5.1 c' 降级（清 tmp 重建 + final 复核） | 任务3 步骤 4 |
| §6 契约保持（journal 阶段名/source_changed/回滚/VSS/回收站/前端/校验强度） | 任务3 步骤 4 注释逐条保留；阶段 d 全程不动 |
| §7 错误处理（各阶段 source_changed/reason） | 任务1 步骤 10/11、任务3 步骤 4 |
| §8 测试 1 MockOps 适配 | 任务1 步骤 8 + 任务3 步骤 1 |
| §8 测试 2 并发正确性 | 任务2 步骤 1 + 任务4 测试 A |
| §8 测试 3 枚举/复制竞态 | 任务4 测试 B |
| §8 测试 4 c' 降级（删除项/补传期间变化） | 任务3 步骤 2 |
| §8 测试 5 取消/错误传播 | 任务4 测试 C/D |
| §8 测试 6 进度/参数边界 | 任务1 步骤 1（resolve_concurrency）+ 任务4 测试 E |
| §10 实现顺序 1-5 | 任务1→2→3→4 严格对应 |

**遗漏检查：** 规格 §8 测试 4 提到"覆盖补传期间 old_path 再变化时回滚改名"——任务3 步骤 2 的 `migrate_c_prime_source_keeps_changing_rolls_back_rename` 覆盖。规格 §8 测试 2 的"含 reparse point"——任务4 测试 A 验证 outcome 自洽，reparse 占位不递归已由现有 `copy_tree_does_not_descend_into_reparse_point`（任务1 保留）覆盖。无遗漏。

**2. 占位符扫描：** 全计划无"TODO/待定/类似任务N/添加适当错误处理"等占位。每个代码步骤含完整可编译代码块；每个"运行"步骤含精确命令与预期。已修正：任务1 步骤 6 reparse 占位分支（原含错误 `String::new()`）、任务2 步骤 4 `cancel_requested` 多余变量与"手动加 fetch_sub"指令、任务1 步骤 15 的 MockOps 误判分析、任务1 漏列的 file_ops 测试调用点适配。

**3. 类型一致性：**
- `CopyOutcome { copied_manifest: Manifest, dst_manifest: Manifest, total_bytes: u64, total_files: u64 }`——任务1 定义，任务2/3/4 引用字段名一致。
- `copy_tree` 签名 `(src, dst, on_progress: &dyn Fn(&CopyProgress), should_cancel: &(dyn Fn()->bool+Sync), concurrency: usize) -> AppResult<CopyOutcome>`——任务1 定义，任务2/3/4 调用一致。
- `resolve_concurrency(Option<NonZeroUsize>) -> usize`——任务1 定义，migrator a'/c' 用 `resolve_concurrency(plan.copy_concurrency)`、restore 用 `resolve_concurrency(None)`，一致。
- `MigratePlan.copy_concurrency: Option<NonZeroUsize>`——任务1 定义，commands.rs/e2e/plan_for 补 `None`，一致。
- `MockOps` 字段 `source_changed_during_copy`/`source_keeps_changing`/`manifest_old_count`——任务3 步骤 1 定义，步骤 2 测试与步骤 6a 引用一致。
- journal 阶段名 `copied`/`manifest_ok`/`source_renamed`/`incremental_synced`/`junction_created` 全程未改名。
- `AppError::Cancelled`/`Migrate`/`Io` 变体引用与 `error.rs` 一致。

**4. 已知设计决策（非占位）：**
- `copy_tree` 增加 `concurrency: usize` 参数是对规格 §4.3 接口清单的补全（规格 §4.2 要求并发度可配但未说明传递路径），已在"设计补全说明"章节注明。
- restore 不重组流程，仅借 `copy_tree` 内部并发获得复制加速（`resolve_concurrency(None)`），符合规格"聚焦迁移板块"范围。
- MockOps `diff_manifests` 在 `manifest_ok=true` 时委托真实比较，是对规格 §4.3"diff_manifests 不变"（指 trait 签名与 RealFileOps 实现）的测试侧适配，生产代码 `RealFileOps::diff_manifests` 与 `file_ops.rs` 自由逻辑均未改动。

<!-- END -->
