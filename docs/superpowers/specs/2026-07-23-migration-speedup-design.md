# 迁移板块提速设计

- **日期**：2026-07-23
- **范围**：`src-tauri/src/migrator.rs`、`src-tauri/src/file_ops.rs`（核心）；`commands.rs`、`MigratePlan` 构造点和相关 mock（仅补齐参数，不改变命令/前端行为）
- **目标**：在保持现有正确性契约的前提下，显著加快迁移（复制 + 校验）速度，主攻「海量小文件」场景。

## 1. 背景与问题诊断

迁移一次（非 VSS）的完整 I/O 路径，当前实现把目录重复遍历 6 次、文件数据全量读取 2 次：

| 阶段 | 现状操作 | 浪费 |
|---|---|---|
| a 复制 | `measure_tree`（扫 1）+ `copy_tree`（扫 1 + 读数据 1） | 目录扫 2 遍，数据读 1 遍 |
| b 首校验 | `manifest(src)`（扫 1）+ `manifest(tmp)`（扫 1）+ `diff` | 目录又扫 2 遍 |
| c 增量同步 | `copy_tree(old→tmp 全量覆盖)`（扫 1 + 读数据 2）+ 双 `manifest`（扫 2）+ `diff` | 🔴 目录扫 4 遍，数据第 2 次全量读 |

最大浪费源：阶段 c 的「增量同步」实为全量再复制——`copy_tree` 不做差异判断，直接把改名后的源（`old_path`）整体重读重写到 `tmp` 覆盖。源在复制期间基本不变 → 数据量翻倍读取。

次要浪费：复制底层 `copy_file_with_control` 为 `read`/`write` + 1MB buffer 串行单线程，海量小文件场景下瓶颈是 syscall 数量（每文件一次 open/read/write/close + 多次 stat）与逐文件 IO 调度延迟，无法被带宽掩盖。

## 2. 目标与非目标

### 目标

- 常态目录遍历次数 6 → 2（均为 stat-only；源发生变化而触发全量补传时允许额外遍历）。
- 文件数据全量读取次数 2 → 1。
- 引入文件级并发，掩盖小文件的 open/close/元数据延迟。
- 保持全部现有正确性契约（journal 阶段、`source_changed` 跟踪、回滚路径、VSS 分支）。
- 兼容最近提交（`ed52567`）的回收站降级改动。

### 非目标（YAGNI）

- 不引入 rsync 式定向补丁组件（`patch_tree`）。源变化是罕见情况，降级全量补传即可。
- 不做目标盘类型自动探测（`IOCTL_STORAGE_QUERY_PROPERTY` 旋转速率）。并发度走默认 + 可配置。
- 不引入内容 hash 校验（用户明确「最大提速优先」，hash 增 CPU 开销，与目标冲突）。
- 不改动阶段 d（建链 / 记录 / 清理）及其回收站降级逻辑。
- 不改动前端进度契约（`phase` 仍为 Preparing/Copying，`TransferProgress` 结构不变）。

## 3. 方案选型

经对比三方案（A 算法层重排 / B A+并发 / C A+边复制边算 hash），用户选定 **B：A + 文件级并发，一步到位**。

并发原语：用 **`std::thread::scope` + 共享任务队列，零新依赖**（项目当前无 rayon/crossbeam，且本任务为 IO 密集而非 CPU 密集，rayon 的 work-stealing 非最优）。

## 4. 架构设计

### 4.1 流程重组

**旧流程（非 VSS）**：
```
a 复制(measure+copy) → b 首校验(manifest×2+diff) → c 改名源 → 增量全量复制+二次校验 → d 建链/记录/清理
```

**新流程（非 VSS）**：
```
a' 单遍复制(扫描建树 + 并发复制 + 记 copied_manifest/dst_manifest)
b' 复核 diff(copied_manifest, dst_manifest)          ← 零额外遍历
c' 改名源 → 扫 old_path 记 old_manifest(stat 1遍)
   → diff(copied_manifest, old_manifest)：空则跳过；非空重建 tmp 后全量补传
d 建链/记录/清理（不变）
```

三个语义保证：

1. **复制完整性**（b'）：`copied_manifest` 是 worker 对实际读入、实际创建的条目记录；`dst_manifest` 是每个条目写入/创建后从目标路径取得的元数据。两者 diff 验证实际传输的输入与落盘结果，零额外目录遍历。
2. **复制期间源变化**（c'）：`copied_manifest`（实际已复制版本）vs `old_manifest`（改名后最新）比较。源没变 → diff 空 → 跳过补传；变了 → 在新的空 tmp 中降级全量补传。
3. **VSS 分支不变**：`resolver.vss_enabled()` 时仍跳过 c' 补传。

### 4.2 核心组件

#### 组件 1：`TreeCopier`（合并遍历复制器）

替换 `measure_tree` + `copy_tree` + 双 `manifest`，两阶段内部流程：

```
阶段① 扫描建树（单线程，stat-only，不读数据）
  - 深度优先遍历 src，跳过非 src 自身的 reparse point（语义同现状）
  - 目录：create_dir_all 对应 dst（幂等安全）+ 记入待确认的目录任务；文件：stat 只用于进度总量并推入复制任务队列
  - 扫描清单只决定任务和进度，**不得**作为 b'/c' 的一致性基准；文件可能在枚举后被创建、删除或改写
  - 产出：total_bytes/total_files + 任务队列 [(src,dst,rel_path)]

阶段② 并发复制（线程池，读数据）
  - N 个 worker 从队列取任务：打开并读取 src → 写入 dst → 按实际读入字节记 `copied_manifest`，再 `symlink_metadata(dst)` 记 `dst_manifest`
  - 目录和 reparse 占位在阶段①创建后也分别从 src/dst 记入两份 manifest；文件任务在打开前重新 `symlink_metadata`，已不存在或已不再是普通文件时跳过并交由 c' 对账，其他 I/O 错误仍失败
  - 产出：`copied_manifest`（实际输入）+ `dst_manifest`（实际目标）+ 实际复制统计
```

`copied_manifest` 只在条目实际创建/读取时定型，`dst_manifest` 在对应目标条目创建/写入后定型。因此 b' 的差异代表传输不一致；源在枚举后才新增、删除或改写的条目不会被误判为 b' 失败，而会由改名后的 `old_manifest` 在 c' 检出并补传。

#### 组件 2：并发执行器

- `std::thread::scope` 内 spawn N 个 worker，共享任务队列；scope 结束自动 join。
- **任务队列**：`Arc<Mutex<VecDeque<CopyTask>>>` 共享，worker 循环 `lock().pop()` 取任务（std 原生多消费者方案，无需 channel clone）。
- **并发度**：默认 `min(max(逻辑核数, 1), 8)`；通过 `MigratePlan` 新增可选字段 `copy_concurrency: Option<NonZeroUsize>` 覆盖（`None` 走默认），从类型层拒绝 0。`commands.rs` 和现有测试构造点显式传 `None`；当前不暴露前端配置。
- **目录竞态规避**：目录在阶段①串行建好，阶段②只填文件到已存在目录，无 create 竞态。

#### 组件 3：进度聚合

- `AtomicU64 actual_completed_bytes` + `AtomicUsize actual_completed_files`，worker 按块累加。
- 主线程跑进度汇报器：每 100ms 读原子值回调 `on_progress`，复制结束停止。避免多 worker 抢 `last_emit` 锁。
- `total_bytes/total_files` 来自阶段①。若源在扫描后增长，实际计数可超过预估总量；对前端发出的 `completed_*` 必须分别 clamp 为 `min(actual, total)`，从而保持 `completed <= total`。`phase` 仍分 Preparing/Copying，前端契约不变。

#### 组件 4：错误 / 取消传播

- **取消**：`should_cancel`（AtomicBool）由外部持有。`FileOps::copy_tree` 的参数改为 `&(dyn Fn() -> bool + Sync)`，worker 可安全地在取任务前及块复制中检查；取消时停止取新任务，主线程清理 tmp 并走 `journal.cancel`。`on_progress` 仍只在扫描/主线程汇报器中调用，无需跨线程调用外部回调。
- **错误**：`Mutex<Option<AppError>> first_error`。任一 worker 失败即写入首个错误并设停止标志，其他 worker 收尾。主线程 join 后返回 `first_error`，migrator 按复制失败回滚（`source_changed=false`，清理 tmp，`journal.fail`）——与现有复制失败回滚路径完全一致。

### 4.3 FileOps trait 接口变更

新增结构（`file_ops.rs`）：

```rust
pub struct CopyOutcome {
    /// 实际创建目录或成功读入的源条目，不是扫描时的任务清单。
    pub copied_manifest: Manifest,
    /// 每个对应目标条目创建/写入后取得的实际元数据。
    pub dst_manifest: Manifest,
    pub total_bytes: u64,
    pub total_files: u64,
}
```

`copy_tree` 返回值 `AppResult<()>` → `AppResult<CopyOutcome>`。

- `manifest` 方法保留（c' 扫 `old_path` 仍用）。
- `diff_manifests` 不变。
- `should_cancel` 参数从 `&dyn Fn() -> bool` 改为 `&(dyn Fn() -> bool + Sync)`；调用方闭包与 `MockOps` 签名一并更新。
- `MockOps`（测试 mock）的 `copy_tree` 返回值跟着改；`MigratePlan` 的全部 struct literal 补 `copy_concurrency: None`。

## 5. migrator 集成

新 `migrate_with_snapshot`（非 VSS）流程骨架：

```rust
// 阶段 a'：单遍复制 + 双 manifest
emit(COPYING, 0, "准备复制到临时目录");
let outcome = bail(ops.copy_tree(resolver.read(), &plan.tmp, on_progress, cancel),
                   false, "migrate_rolled_back")?;   // CopyOutcome

// 阶段 b'：复核（零额外目录遍历）
emit(VERIFYING, 60, "校验 manifest");
if !ops.diff_manifests(&outcome.copied_manifest, &outcome.dst_manifest).is_empty() {
    bail(journal.fail(task, "manifest 不一致"), false, "migrate_rolled_back")?;  // 保留 tmp
    return Err(fail(Migrate("manifest 不一致"), false, "migrate_rolled_back"));
}
journal.mark_stage(task, "copied")?;
journal.mark_stage(task, "manifest_ok")?;

// 阶段 c'：改名源
emit(RENAMING_SOURCE, 70, "改名源目录");
ops.rename(&plan.src, &plan.old_path)?;
source_changed = true;
journal.mark_stage(task, "source_renamed")?;

// 增量补传（VSS 仍跳过）
if resolver.vss_enabled() {
    emit(SYNCING, 80, "快照一致视图，跳过增量同步");
    journal.mark_stage(task, "incremental_synced")?;
} else {
    emit(SYNCING, 80, "检查复制期间的变化");
    let old_manifest = ops.manifest(&plan.old_path)?;   // 仅 stat，1 遍
    if ops.diff_manifests(&outcome.copied_manifest, &old_manifest).is_empty() {
        // 源没变 -> 跳过补传（常态，省掉旧的全量重读）
    } else {
        // 源变了（罕见）-> 先清空旧 tmp，再在空目录中全量补传。
        // 不能原地覆盖：old_path 中已删除的条目必须从 tmp 消失。
        // remove_tree/copy_tree 的失败和取消复用现有增量失败处理：
        // 尝试 old_path -> src 回滚、按 source_changed=true 写 journal。
        ops.remove_tree(&plan.tmp)?;
        let patch = ops.copy_tree(&plan.old_path, &plan.tmp, on_progress, cancel)?;
        if !ops.diff_manifests(&patch.copied_manifest, &patch.dst_manifest).is_empty() {
            let _ = ops.rename(&plan.old_path, &plan.src);  // 回滚改名
            bail(journal.fail(task, "二次校验不一致"), true, "migrate_partial")?;
            return Err(fail(Migrate("增量后 manifest 不一致"), true, "migrate_partial"));
        }
        // old_path 在改名后理论上不再被常规路径写入；仍复核一次，
        // 保持现有“补传期间发生变化则回滚”的安全语义。
        let final_old_manifest = ops.manifest(&plan.old_path)?;
        if !ops.diff_manifests(&patch.copied_manifest, &final_old_manifest).is_empty() {
            let _ = ops.rename(&plan.old_path, &plan.src);
            bail(journal.fail(task, "补传期间源发生变化"), true, "migrate_partial")?;
            return Err(fail(Migrate("补传期间源发生变化"), true, "migrate_partial"));
        }
    }
    journal.mark_stage(task, "incremental_synced")?;
}

// 阶段 d：建链/记录/清理 —— 完全不动（含 guard_recycle_bin / pending_cleanup / OldPendingDelete）
```

### 5.1 c' 降级策略（YAGNI）

不引入 `patch_tree`。源变了是罕见情况，先删除旧 tmp，再复用 `copy_tree` 在同一路径新建完整树；这样旧 tmp 中已被源删除的条目不会残留。补传后比较 `patch.copied_manifest`/`patch.dst_manifest`，并重新扫描 `old_path`，若补传期间仍发生变化则回滚改名。常态（源没变）走 diff 空分支，零数据读取；降级路径允许额外一次 `old_path` stat 遍历。trait 只改 `copy_tree` 返回 `CopyOutcome` 和取消回调的 `Sync` 约束，不加新方法。

## 6. 契约保持

| 契约 | 保持方式 |
|---|---|
| journal 阶段名 | `copied`/`manifest_ok`/`source_renamed`/`incremental_synced`/`junction_created`/... 全部保留相同名称与顺序，`recover_pending_decisions` 兼容 |
| `source_changed` 跟踪 | 规则不变：改名前 false、改名后 true；复制失败=false、增量失败=true |
| 回滚路径 | 复制失败→清 tmp+journal.fail（false）；补传前清 tmp 或补传失败→回滚改名+journal.fail（true）；建链失败→删 junction+target 回 tmp+old 回原名 |
| VSS 分支 | `vss_enabled()` 仍跳过 c' 补传 |
| 回收站降级（提交 `ed52567`） | 阶段 d 完全不动：`guard_recycle_bin`、`pending_cleanup`、`OldPendingDelete` 降级、前端标签 |
| 前端进度契约 | `TransferProgress` 结构与 `phase` 值不变 |
| 校验强度 | 不变：仍为 size + is_dir 比对（同现状），提速不降低校验基线 |

## 7. 错误处理

- **复制失败**（阶段 a'）：`source_changed=false`，清理 tmp，`journal.fail`/`journal.cancel`，返回 `migrate_rolled_back`。并发下任一 worker 失败等同复制失败；枚举后已不存在或已变为非普通文件的任务是源变化，不作为复制错误。
- **复核不一致**（阶段 b'）：保留 tmp 供排查，`source_changed=false`，`migrate_rolled_back`。
- **改名源失败**（阶段 c' 入口）：`source_changed=false`（未改名），`migrate_rolled_back`。
- **补传前清 tmp、增量补传失败或不一致**（阶段 c' 降级路径）：`source_changed=true`（已改名），回滚改名，`migrate_partial`。
- **建链/记录失败**：现有路径不变。
- **回收站失败/panic**：现有 `guard_recycle_bin` + `OldPendingDelete` 降级不变。

## 8. 测试策略

1. **MockOps / 调用方适配**：`copy_tree` 返回 `CopyOutcome`（构造 `copied_manifest` 和 `dst_manifest`），取消回调满足 `Sync`，全部 `MigratePlan` 构造点补 `copy_concurrency: None`。现有注入点（`manifest_ok`/`copy_ok`/`incremental_copy_ok`）语义平移。
2. **并发正确性（RealFileOps）**：fixtures 造 1000+ 小文件 + 多层目录 + 含 reparse point，验证两份 outcome manifest 为空 diff、无文件丢失/无空目录残留，以及 `copy_concurrency=1` 与多 worker 的结果相同。
3. **枚举/复制竞态**：在扫描后、worker 打开前分别新增、删除、扩容文件及文件/目录类型切换；验证 b' 不误报、c' 能按实际 `copied_manifest` 判断是否补传，最终目标与 `old_path` 一致。
4. **c' 降级**：让初次复制后源删除文件或目录，并在 c' 触发补传；验证旧 tmp 被清空、删除项不会残留。另覆盖补传期间 old_path 再变化时回滚改名。
5. **取消和错误传播**：并发复制中途置 `cancel`，验证 tmp 清理 + `AppError::Cancelled` + `journal.cancel`；注入单文件不可读错误，验证首个错误传播 + `source_changed=false`。
6. **进度与参数边界**：增长文件下 UI 事件仍满足 `completed_* <= total_*` 且单调；验证 `None` 默认值、`NonZeroUsize::new(1)` 和高并发值，编译期/构造期拒绝 0。

## 9. 预期收益与风险

- **预期提速**：1.5–2×（消除冗余遍历 + 全量增量复制）叠加并发 3–5×（SSD）/ 2–3×（HDD）。
- **主要风险**：并发正确性（目录竞态、源变化、进度时序、错误传播）——通过「先串行建目录再并发填文件」「实际读入/实际落盘两份 manifest」「源变化时重建 tmp」「主线程进度汇报器」「首个错误 + 停止标志」化解，并以竞态测试覆盖。
- **改动面**：`file_ops.rs`（`TreeCopier` + `CopyOutcome` + 并发执行器 + `Sync` 回调约束）、`migrator.rs`（阶段重组）、`commands.rs` 和测试中的 plan 构造、测试 mock 适配。阶段 d 与回收站降级不动。

## 10. 实现顺序提示（供 writing-plans 细化）

1. `CopyOutcome`（实际读入/实际落盘 manifest）+ `copy_tree`/取消回调签名变更 + 串行实现；更新 plan 构造点并设 `copy_concurrency: None`。
2. 并发执行器（`thread::scope` + 任务队列 + 进度 clamp/错误/取消），并以 `NonZeroUsize` 约束并发度。
3. `migrator` 阶段重组（a'/b'/c'），实现源变化时清空 tmp 后重建及补传后二次 old_path 复核。
4. MockOps 适配 + 现有测试回归。
5. 并发正确性 / 枚举竞态 / 取消 / 错误传播 / 删除项 c' 降级 / 进度 新增测试。
