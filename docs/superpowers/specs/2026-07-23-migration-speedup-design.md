# 迁移板块提速设计

- **日期**：2026-07-23
- **范围**：`src-tauri/src/migrator.rs`、`src-tauri/src/file_ops.rs`（核心）；`commands.rs`（调用方不受影响）
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

- 目录遍历次数 6 → 2（均为 stat-only）。
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
a' 单遍复制(扫描建树+记 src_manifest + 并发复制+记 dst_manifest)
b' 复核 diff(src_manifest, dst_manifest)            ← 零额外遍历
c' 改名源 → 扫 old_path 记 old_manifest(stat 1遍)
   → diff(src_manifest, old_manifest)：空则跳过；非空降级全量补传
d 建链/记录/清理（不变）
```

三个语义保证：

1. **复制完整性**（b'）：复制时记录的 `src_manifest`（扫描快照）与 `dst_manifest`（实际写入）diff——捕捉复制本身是否忠实。语义同旧 b，零额外遍历。
2. **复制期间源变化**（c'）：`src_manifest`（复制前快照）vs `old_manifest`（改名后最新）。源没变 → diff 空 → 跳过补传；变了 → 降级全量补传。
3. **VSS 分支不变**：`resolver.vss_enabled()` 时仍跳过 c' 补传。

### 4.2 核心组件

#### 组件 1：`TreeCopier`（合并遍历复制器）

替换 `measure_tree` + `copy_tree` + 双 `manifest`，两阶段内部流程：

```
阶段① 扫描建树（单线程，stat-only，不读数据）
  - 深度优先遍历 src，跳过非 src 自身的 reparse point（语义同现状）
  - 目录：create_dir_all 对应 dst（幂等安全）+ 记 src_manifest 目录条目
  - 文件：stat 拿 size/mtime/attrs → 记 src_manifest 文件条目 + 推入复制任务队列
  - 产出：src_manifest（完整）+ total_bytes/total_files + 任务队列 [(src,dst,size)]

阶段② 并发复制（线程池，读数据）
  - N 个 worker 从队列取任务：read src → write dst → 记 dst_manifest 条目（size=实际写入）
  - 产出：dst_manifest（完整）+ 实际复制统计
```

`src_manifest` 在扫描时定型（复制前快照），`dst_manifest` 在写入后定型。两者 diff 即能捕捉复制期间源变化（源被追加 → `src_manifest` 记旧 size、dst 写入新 size → diff 命中），此即 c' 补传依据，无需额外遍历。

#### 组件 2：并发执行器

- `std::thread::scope` 内 spawn N 个 worker，共享任务队列；scope 结束自动 join。
- **任务队列**：`Arc<Mutex<VecDeque<CopyTask>>>` 共享，worker 循环 `lock().pop()` 取任务（std 原生多消费者方案，无需 channel clone）。
- **并发度**：默认 `min(逻辑核数, 8)`；通过 `MigratePlan` 新增可选字段 `copy_concurrency: Option<usize>` 覆盖（`None` 走默认），HDD 抖动场景可调低。
- **目录竞态规避**：目录在阶段①串行建好，阶段②只填文件到已存在目录，无 create 竞态。

#### 组件 3：进度聚合

- `AtomicU64 completed_bytes` + `AtomicUsize completed_files`，worker 按块累加。
- 主线程跑进度汇报器：每 100ms 读原子值回调 `on_progress`，复制结束停止。避免多 worker 抢 `last_emit` 锁。
- `total_bytes/total_files` 来自阶段①，`phase` 仍分 Preparing/Copying，前端契约不变。

#### 组件 4：错误 / 取消传播

- **取消**：`should_cancel`（AtomicBool）由外部持有。worker 取任务前检查；取消时停止取新任务，主线程清理 tmp 并走 `journal.cancel`。
- **错误**：`Mutex<Option<AppError>> first_error`。任一 worker 失败即写入首个错误并设停止标志，其他 worker 收尾。主线程 join 后返回 `first_error`，migrator 按复制失败回滚（`source_changed=false`，清理 tmp，`journal.fail`）——与现有复制失败回滚路径完全一致。

### 4.3 FileOps trait 接口变更

新增结构（`file_ops.rs`）：

```rust
pub struct CopyOutcome {
    pub src_manifest: Manifest,
    pub dst_manifest: Manifest,
    pub total_bytes: u64,
    pub total_files: u64,
}
```

`copy_tree` 返回值 `AppResult<()>` → `AppResult<CopyOutcome>`。

- `manifest` 方法保留（c' 扫 `old_path` 仍用）。
- `diff_manifests` 不变。
- `MockOps`（测试 mock）的 `copy_tree` 返回值跟着改。

## 5. migrator 集成

新 `migrate_with_snapshot`（非 VSS）流程骨架：

```rust
// 阶段 a'：单遍复制 + 双 manifest
emit(COPYING, 0, "准备复制到临时目录");
let outcome = bail(ops.copy_tree(resolver.read(), &plan.tmp, on_progress, cancel),
                   false, "migrate_rolled_back")?;   // CopyOutcome

// 阶段 b'：复核（零额外遍历）
emit(VERIFYING, 60, "校验 manifest");
if !ops.diff_manifests(&outcome.src_manifest, &outcome.dst_manifest).is_empty() {
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
    if ops.diff_manifests(&outcome.src_manifest, &old_manifest).is_empty() {
        // 源没变 -> 跳过补传（常态，省掉旧的全量重读）
    } else {
        // 源变了（罕见）-> 降级全量补传，复用现有 copy_tree，保证正确性
        let patch = ops.copy_tree(&plan.old_path, &plan.tmp, on_progress, cancel)?;
        if !ops.diff_manifests(&old_manifest, &patch.dst_manifest).is_empty() {
            let _ = ops.rename(&plan.old_path, &plan.src);  // 回滚改名
            bail(journal.fail(task, "二次校验不一致"), true, "migrate_partial")?;
            return Err(fail(Migrate("增量后 manifest 不一致"), true, "migrate_partial"));
        }
    }
    journal.mark_stage(task, "incremental_synced")?;
}

// 阶段 d：建链/记录/清理 —— 完全不动（含 guard_recycle_bin / pending_cleanup / OldPendingDelete）
```

### 5.1 c' 降级策略（YAGNI）

不引入 `patch_tree`。源变了是罕见情况，降级为旧的全量补传（复用 `copy_tree`）保证正确性，比写一套定向补丁简单、少错。常态（源没变）走 diff 空分支，零数据读取。trait 因此只改 `copy_tree` 返回 `CopyOutcome`，不加新方法。

## 6. 契约保持

| 契约 | 保持方式 |
|---|---|
| journal 阶段名 | `copied`/`manifest_ok`/`source_renamed`/`incremental_synced`/`junction_created`/... 全部保留相同名称与顺序，`recover_pending_decisions` 兼容 |
| `source_changed` 跟踪 | 规则不变：改名前 false、改名后 true；复制失败=false、增量失败=true |
| 回滚路径 | 复制失败→清 tmp+journal.fail（false）；增量失败→回滚改名+journal.fail（true）；建链失败→删 junction+target 回 tmp+old 回原名 |
| VSS 分支 | `vss_enabled()` 仍跳过 c' 补传 |
| 回收站降级（提交 `ed52567`） | 阶段 d 完全不动：`guard_recycle_bin`、`pending_cleanup`、`OldPendingDelete` 降级、前端标签 |
| 前端进度契约 | `TransferProgress` 结构与 `phase` 值不变 |
| 校验强度 | 不变：仍为 size + is_dir 比对（同现状），提速不降低校验基线 |

## 7. 错误处理

- **复制失败**（阶段 a'）：`source_changed=false`，清理 tmp，`journal.fail`/`journal.cancel`，返回 `migrate_rolled_back`。并发下任一 worker 失败等同复制失败。
- **复核不一致**（阶段 b'）：保留 tmp 供排查，`source_changed=false`，`migrate_rolled_back`。
- **改名源失败**（阶段 c' 入口）：`source_changed=false`（未改名），`migrate_rolled_back`。
- **增量补传失败/不一致**（阶段 c' 降级路径）：`source_changed=true`（已改名），回滚改名，`migrate_partial`。
- **建链/记录失败**：现有路径不变。
- **回收站失败/panic**：现有 `guard_recycle_bin` + `OldPendingDelete` 降级不变。

## 8. 测试策略

1. **MockOps 适配**：`copy_tree` 返回 `CopyOutcome`（构造两份 manifest）。现有注入点（`manifest_ok`/`copy_ok`/`incremental_copy_ok`）语义平移。现有 migrator 测试（`migrate_source_changed_tracking`、`migrate_rolled_back_failure_no_junction_left`、各命根子失败注入测试）契约不变、断言不改，仅改 mock 返回值装配。
2. **并发正确性（RealFileOps）**：fixtures 造 1000+ 小文件 + 多层目录 + 含 reparse point，验证复制完整、`diff_manifests` 为空、无文件丢失/无空目录残留。
3. **取消**：并发复制中途置 `cancel`，验证 tmp 清理 + `AppError::Cancelled` + `journal.cancel`。
4. **错误传播**：注入单文件复制失败（mock 或不可读文件），验证首个错误传播 + tmp 清理 + `source_changed=false`。
5. **c' 降级**：mock 让 `src_manifest` 与 `old_manifest` diff 非空，验证走全量补传路径并最终一致。
6. **进度**：并发下 `completed_bytes` 单调递增、不超 `total_bytes`、phase 正确。

## 9. 预期收益与风险

- **预期提速**：1.5–2×（消除冗余遍历 + 全量增量复制）叠加并发 3–5×（SSD）/ 2–3×（HDD）。
- **主要风险**：并发正确性（目录竞态、进度时序、错误传播）——通过「先串行建目录再并发填文件」「主线程进度汇报器」「首个错误 + 停止标志」三机制化解，并以并发正确性测试覆盖。
- **改动面**：`file_ops.rs`（`TreeCopier` + `CopyOutcome` + 并发执行器）、`migrator.rs`（阶段重组）、测试 mock 适配。阶段 d 与回收站降级不动。

## 10. 实现顺序提示（供 writing-plans 细化）

1. `CopyOutcome` 结构 + `copy_tree` 签名变更 + 串行实现（先不并发，保证语义正确）。
2. 并发执行器（`thread::scope` + 任务队列 + 进度/错误/取消）。
3. `migrator` 阶段重组（a'/b'/c'）。
4. MockOps 适配 + 现有测试回归。
5. 并发正确性 / 取消 / 错误传播 / c' 降级 / 进度 新增测试。
