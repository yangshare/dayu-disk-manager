# T10 实现报告：文件系统操作后整树失效

状态：**DONE_WITH_CONCERNS**
提交：`10687b6 feat(commands): 文件系统变化后原子失效扫描树`
基线：`395d100`（T9 完成 HEAD）

## 1. 实现概要

- **models.rs**：新增 `OperationOutcome { source_changed, reason }`（后端内部）和 `ScanInvalidatedEvent { reason, auto_rescan }`（前端监听）两种类型。
- **migrator.rs**：
  - `migrate` 返回 `Result<(Migration, OperationOutcome), (AppError, OperationOutcome)>`，新增 `source_changed` 内部状态在改名源后置 true，并在所有失败路径用 `fail(e, source_changed, reason)` helper 返回。
  - `restore` 返回 `Result<OperationOutcome, (AppError, OperationOutcome)>`，删 junction 后置 source_changed=true；删 junction 后改名失败时仍报告 true（裁定：中间过程 src 不再是 junction）。
  - `break_link` 返回 `Result<OperationOutcome, (AppError, OperationOutcome)>`，remove_junction 失败时 source_changed=false。
  - 既有回滚逻辑不变（删 tmp、改回 src、删 junction 重建等）。
- **commands.rs**：
  - 新增 `invalidate_scan_tree(app, state, outcome)`：write lock 内 take，锁外 emit，emit 失败不影响已清空的 store。
  - 抽 `invalidate_scan_tree_impl(outcome, take_fn, emit_fn)` 纯函数（`#[cfg(test)]`）用于测试。
  - 抽 `apply_outcome(outcome, on_invalidate_changed)` 纯函数覆盖命令接线逻辑。
  - 抽 `propagate_migrator_error(app, state, err)` 把 migrator 失败转为 AppError，source_changed=true 时同步失效。
  - `start_migrate`/`start_restore`/`break_link_cmd` 三个命令全部接线：成功路径按 outcome.source_changed 决定失效；失败路径用 propagate_migrator_error。

## 2. 关键决策

- **返回结构（§2.6）**：migrate 选 `Result<(Migration, OperationOutcome), (AppError, OperationOutcome)>`——成功带 Migration 供前端展示，所有失败路径带 outcome 标明 source_changed。restore/break_link 用同一结构保持调用对称。
- **§2.2 source_changed 表格落地**：migrate 内部跟踪 `mut source_changed` 变量，改名源 `mark_stage("source_renamed")` 后置 true；增量失败、建 junction 失败、记录失败、回收站降级、成功均按表精确返回。
- **§2.3 restore 删-重建 junction 的 source_changed 裁定**：取 **true**（源-重建过程中 src 曾不是 junction，扫描树基于删 junction 前视图已不一致），并在代码注释和报告 §5 说明。
- **可注入测试方案**：抽三个纯函数（`invalidate_scan_tree_impl`、`apply_outcome`、`propagate_migrator_error`），命令接线的 source_changed 分支用 `apply_outcome` 覆盖；失效辅助本身用 `invalidate_scan_tree_impl` 注入 mock take_fn/emit_fn。

## 3. 测试结果

### commands::tests（30 通过，0 失败；既有 20 + T10 新增 10）
1. `invalidate_clears_published_mft_store_and_auto_rescans`：发布 MFT store + 失效，断言 current_scan=None、auto_rescan=true。
2. `invalidate_filesystem_store_no_auto_rescan`：filesystem store → auto_rescan=false。
3. `invalidate_when_no_store_no_auto_rescan`：无 store → 不 panic、auto_rescan=false。
4. `invalidate_clears_before_emit_failure`：emit 返 Err 时 current_scan 仍被 take 清空。
5. `start_migrate_success_invalidates_tree`：apply_outcome + changed("migrated") → 触发失效。
6. `start_migrate_rolled_back_failure_does_not_invalidate`：unchanged("migrate_rolled_back") → 不触发失效。
7. `start_migrate_partial_failure_source_changed_invalidates`：changed("migrate_partial") → 触发失效。
8. `start_restore_success_invalidates`：changed("restored") → 触发失效。
9. `break_link_success_invalidates`：changed("broken_link") → 触发失效。
10. `break_link_failure_does_not_invalidate`：unchanged("break_link_rolled_back") → 不触发失效。

### migrator::tests（9 通过，0 失败；既有 6 + T10 新增 2 + 适配性断言 1）
11. `migrate_source_changed_tracking`：6 个子用例（成功、复制失败、改名源失败、增量同步失败、建 junction 失败、回收站降级）逐一断言 source_changed 与 reason 精确符合 §2.2 表。
12. `migrate_rolled_back_failure_no_junction_left`：复制失败后断言 src 未改名、无 junction、无 tmp/target 残留、无迁移记录（既有回滚不回归）。
- 既有 `restore_success_recovers_dir_and_removes_link` / `restore_aborts_when_junction_invalid` / `restore_switch_fail_rebuilds_junction` 适配新返回类型并新增 outcome.source_changed / reason 断言。

### 全量（304 通过，0 失败）
- lib unittests：270（含 commands 30、migrator 9、scanner、mft、journal、junction、store、safety、history 等）
- crash_recovery：5
- e2e_migration：2（ripple-effect 适配）
- mft_fixture：26
- smoke：1

### Build + Clippy
- `cargo build` 成功
- `cargo clippy --lib --tests`：commands.rs/migrator.rs/models.rs 三个 T10 文件 **0 warning**；剩余 warning 全部在边界外（mft.rs、scanner.rs、journal.rs、junction.rs、mft_fixture.rs、e2e_migration.rs 中既有/T9 之前的 warning）。

## 4. 修改文件

| 文件 | 行数变更 | 性质 |
|---|---|---|
| src-tauri/src/commands.rs | +321 | 新增失效辅助 + 三命令接线 + 10 项测试 |
| src-tauri/src/migrator.rs | +387/-100 | migrate/restore/break_link 返回类型 + source_changed 跟踪 + 测试 |
| src-tauri/src/models.rs | +39 | 新增 OperationOutcome + ScanInvalidatedEvent |
| src-tauri/tests/e2e_migration.rs | +6 | ripple-effect 适配（migrate 新返回签名；非 T10 边界内，但不可避免） |

简报边界外的修改：仅 `tests/e2e_migration.rs` 因 migrate 返回类型变化而需解构 outcome（最小修改，不引入新断言）。

## 5. 自审疑虑

1. **e2e_migration.rs 修改越界**：严格说不在 T10 三文件边界内，但 migrator::migrate 返回类型变更是 breaking change，调用方必须更新。该文件只加 `_outcome` 解构与注释，不引入新断言，保持原有测试意图。已在 §4 标注。
2. **restore 删-重建 junction 的 source_changed 裁定**：简报 §2.3 给出"倾向 true"，实现者选 true。理由：扫描树基于删 junction 前的 src 视图已不一致；重建 junction 后形态虽恢复，但中间过程 src 曾不是 junction，对前端而言视图已 stale。若后续需要"删-重建后视作未变"，可改回 false 并报告说明。
3. **commands::tests 中 apply_outcome + outcome 的纯函数测试** 是命令接线语义的代理测试，未覆盖真正的 start_migrate 整体路径（含 spawn_blocking、emit "dayu://progress" 等）。这部分靠 review 保证接线正确（三个命令都按 propagate_migrator_error 模式处理）。如未来加 Migrator trait 可抽出真正端到端 mock 测试。
4. **migrate_source_changed_tracking 用 MockOps 注入失败**：MockOps 通过路径名（src 含 "dayu-old-" 视为增量阶段）区分复制阶段和增量阶段，因真实环境无现成字段传入。这一 mock 模式适用于测试，不进入生产代码。
5. **break_link 的 source_changed**：remove_junction 失败时 false；store.remove_migration 失败时 true（junction 已删）。这是最干净的设计，但与"操作语义上失败"略不对称；如需要更严格，可改为"任一阶段失败都按 source_changed=true"，但代价是 remove_junction 失败时仍失效扫描树（实际上 junction 仍在 → 扫描树可能仍有效，失效会导致前端误 rescan）。当前设计保守。