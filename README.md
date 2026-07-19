# dayu-disk-manager
## 1. 项目定位与核心价值

**dayu-disk-manager** 是一款 Windows 桌面磁盘治理工具，核心功能是通过 **junction（目录联接）** 把 C 盘的大体积目录迁移到其他盘，并在原位建立链接，从而释放 C 盘空间。项目名取"大禹治水"之意——疏导归流，而非东搬西塞。

<img width="1920" height="1032" alt="image" src="https://github.com/user-attachments/assets/9c0a9c7b-c357-4641-9be8-a263d3e190c0" />

**定位：** 面向懂一点电脑的用户。有图形界面但保留专业控制力，介于傻瓜式"C 盘搬家"和极客命令行工具之间。预设常见场景 + 支持自定义目录，默认安全（可预览、可还原）。

**三大核心价值：**

1. **智能识别** — 扫描 C 盘自动发现并标注可迁移的大目录（微信、Steam、开发工具缓存等），扫描结果带"一键迁移"。
2. **安全迁移** — 复制→校验→原子切换→建链→删原（走回收站）状态机 + 事务恢复 + 完整操作日志。
3. **统一治理** — 所有迁移点归集到统一仓库 `D:\Migrated`（可配置），结构清晰、历史可查。

**技术栈：** Tauri 2 + Rust + Vue 3。创建 junction 本身不需要 Windows 符号链接开发者权限；但迁移受 NTFS ACL、目录占用、源/目标卷能力影响。首版默认处理当前用户可写的数据目录，受保护目录需要明确提示权限不足或引导用户以管理员身份重试。

**技术策略约束：** 只处理目录，只用 junction，不支持单文件链接（单文件迁移对释放 C 盘空间无实际意义）。

## 2. 核心功能模块

软件由 5 个功能模块组成：

### 2.1 扫描分析（Scan）
- 扫描 C 盘目录树，计算各目录占用大小。
- 自动识别预设场景（通讯办公 / 游戏平台 / 开发工具）的安装位置，标注"这是什么、占多大、建议迁移"。
- 未命中预设的大目录（按体积阈值，默认 >500MB）列出，提供"自定义迁移"入口。
- 扫描结果可排序、可筛选，是发现"该迁什么"的入口。
- 扫描时跳过 reparse point，避免 junction/symlink 循环和重复计数；遇到 AccessDenied 不中断，只记录为"无法扫描"。
- 支持扫描取消、限速和结果缓存；扫描中设置变更不影响当前任务。

### 2.2 迁移（Migrate）
- 执行核心状态机：复制到统一仓库临时目录 → manifest 校验 → 源目录短暂改名 → 增量同步 → 建 junction → 原目录移入回收站。
- 迁移前预检：目标盘剩余空间是否足够、源目录是否被进程占用、是否为系统关键路径（拒绝迁移）、源/目标权限是否足够、目标卷是否支持所需 NTFS 语义。
- 操作前展示"将要做什么"的清单，用户确认后执行。
- 支持取消（中途取消时安全回滚到迁移前状态）。
- 迁移进度可视化（进度条 + 当前阶段）。
- 每个阶段开始/完成都写入持久化任务日志，应用崩溃或系统断电后能继续恢复或安全清理。

### 2.3 软链接管理（Links）
- 列出所有由本工具创建的 junction（及系统已有的 junction，标注来源）。
- 每条链接可操作：还原（把数据搬回原位并恢复普通目录）、断开链接（删除 junction、保留迁移后数据，但会使原路径不可用，需二次确认）、打开原位置。
- 检测失效链接（目标被手动删除的），提供清理。

### 2.4 操作历史（History）
- 完整记录每次迁移、还原、删除操作（时间、源路径、目标路径、结果）。
- 可按操作类型/时间筛选查看。
- 双向可追溯：从历史能跳到对应软链接，从软链接能看迁移历史。

### 2.5 设置（Settings）
- 统一迁移仓库路径（默认 `D:\Migrated`，可改到其他盘）。
- 扫描偏好（大目录阈值、要排除的路径）。
- 数据存储位置查看、导出操作日志。

## 3. 架构与模块边界

按 Tauri 标准分层：

```
┌─────────────────────────────────────────────────┐
│  Frontend (Vue 3 + TypeScript)                  │
│  ├─ ScanView      扫描结果展示与交互              │
│  ├─ MigrateView   迁移预检清单与进度              │
│  ├─ LinksView     软链接列表与管理操作            │
│  ├─ HistoryView   操作历史                       │
│  └─ SettingsView  设置                          │
├──── Tauri IPC (invoke / event) ────────────────┤
│  Backend (Rust)                                 │
│  ├─ scanner        扫描 + 预设场景识别            │
│  ├─ migrator       迁移状态机 + 回滚/恢复          │
│  ├─ junction      junction 创建/删除/校验         │
│  ├─ safety        预检（空间/占用/路径黑名单）     │
│  ├─ journal       任务状态与崩溃恢复日志           │
│  ├─ history       操作日志读写                   │
│  ├─ store         迁移记录与配置（本地数据）        │
│  └─ win32         Win32 API 封装                 │
└─────────────────────────────────────────────────┘
```

**各单元职责与接口：**

| 单元 | 职责 | 对外接口（给其他单元） | 依赖 |
|------|------|----------------------|------|
| `scanner` | 遍历目录算体积、匹配预设场景 | `scan(root) -> Vec<ScanItem>` | `store`（读预设场景配置） |
| `safety` | 迁移前预检 | `precheck(src, dst) -> Report` | `win32`（查盘空间/占用进程） |
| `migrator` | 执行迁移/还原状态机 + 进度事件 | `migrate(src, dst, on_progress)`、`restore(id, on_progress)` | `safety`、`junction`、`journal`、`history`、`store` |
| `junction` | junction 的创建/删除/读取/校验 | `create`、`remove`、`resolve`、`verify` | `win32` |
| `history` | 操作日志追加与查询 | `append(op)`、`list(filter)` | `store` |
| `journal` | 记录运行中任务阶段，启动时恢复/清理半成品 | `begin`、`mark_stage`、`complete`、`recover_pending` | `store` |
| `store` | 配置与迁移记录的持久化 | 读写 JSON | 无 |
| `win32` | Win32 API 封装（盘空间、junction API、文件占用） | 薄封装函数 | 无 |

**关键边界原则：**
- `migrator` 是状态机的核心，但它**只编排**——实际建链委托 `junction`，预检委托 `safety`，记录委托 `history`。这样迁移/还原编排逻辑可单独测试，不耦合 Win32 细节。
- `journal` 是恢复边界：每个不可逆或半不可逆动作（复制完成、源目录改名、junction 创建、原目录进回收站）前后都必须先落盘。应用启动时先恢复未完成任务，再允许新迁移。
- `win32` 是唯一的平台边界，其他单元不直接碰系统 API。未来若考虑跨平台，只改这一层。
- `scanner` 和 `migrator` 通过 Tauri event 向前端推进度，前端不轮询。

## 4. 核心数据流 — 一次完整迁移

以"迁移微信文件"为例，走通主流程，验证各模块协作：

```
1. 扫描阶段
   用户点"扫描 C 盘"
   → scanner.scan("C:\")
   → 遍历 + 匹配预设场景，识别到 C:\Users\xxx\Documents\WeChat Files
   → 前端展示：「微信文件，32.4 GB，建议迁移」+ [迁移] 按钮

2. 预检阶段
   用户点[迁移]
   → store 读配置：仓库 = D:\Migrated
   → migrator 分配 taskId/id，目标路径 = D:\Migrated\wechat\{uuid}\data，临时路径 = D:\Migrated\wechat\{uuid}\data.tmp
   → migrator 调 safety.precheck(src, dst)
     - 查 D 盘剩余空间 ≥ 源目录大小 + 安全余量？✓
     - D 盘是本地 NTFS 卷，目标路径可写，且仓库不在源目录内部？✓
     - 源目录被进程占用？先做通用句柄检测，再叠加 WeChat.exe 等预设进程提示
     - 源目录不是 reparse point，且路径不在系统关键目录黑名单内？✓
     - 目标最终目录/临时目录不存在冲突？✓
   → 前端展示预检清单，用户[确认]

3. 迁移阶段（可恢复状态机）
   阶段0 建立任务锁与恢复日志：
     - journal.begin({taskId, src, dst, tmp, stage: "created"})
     - 同一源路径/目标路径只能有一个运行中任务
   阶段a 复制：copy C:\...\WeChat Files → D:\Migrated\wechat\{uuid}\data.tmp
     - 不跟随源目录内部 reparse point，保留时间戳、属性、ACL、备用数据流等 NTFS 元数据
     - 进度事件 on_progress("copying", 45%)
     - 若失败/取消 → 清理 tmp，journal 标记 failed/canceled，源目录不动
   阶段b 校验：生成并对比 manifest
     - manifest 至少包含相对路径、类型、字节数、mtime、attributes；默认不做全量 hash
     - 不一致 → 中止，保留 tmp 供排查，标记"待人工确认"
   阶段c 建链：
     - journal.mark_stage("renaming_source")
     - 将源目录改名 → C:\...\WeChat Files.dayu-old-{taskId}（暂存，非直接删）
     - 从 .dayu-old-{taskId} 到 tmp 做一次增量同步，捕捉复制期间发生的变化
     - 再次校验 manifest；通过后将 tmp 原子改名为最终目标 data
     - junction.create("C:\...\WeChat Files", "D:\Migrated\wechat\{uuid}\data")
     - 校验 junction 解析正常
   阶段d 删原（走回收站）：
     - 先写 store 迁移映射：{src, dst, oldPath, status: "active", ...}
     - 再将 .dayu-old-{taskId} 目录移入回收站（SHFileOperation/IFileOperation + allow undo）
     - 若回收站不可用或失败，保留 oldPath 并标记 old_pending_delete，提示用户手动处理

4. 记录阶段
   → history.append({op: "migrate", src, dst, time, result: "ok"})
   → journal.complete(taskId)
   → 前端跳转到 LinksView，新链接高亮

5. 回滚（用户事后在 LinksView 点[还原]）
   → junction.verify 先确认链接仍指向有效目标
   → 复制 D:\Migrated\wechat\{uuid}\data → C:\...\WeChat Files.restore-tmp-{taskId}
   → manifest 校验通过后进入短切换窗口
   → 删除 junction → 将 restore-tmp 原子改名为 C:\...\WeChat Files
   → 若切换失败，优先重建 junction 指回目标，避免原路径断开
   → history.append({op: "restore", ...})
   → 清理 D:\Migrated\wechat\{uuid}\data（移回收站，失败则标记 target_pending_delete）
```

**几个设计要点：**

- **`.dayu-old-{taskId}` 暂存 + 回收站而非直接删原**：建链成功前原目录绝不删；建链成功且映射记录落盘后，原目录才走回收站。回收站不可用时保留 oldPath，不把"已回收"当作强承诺。
- **校验用 manifest 对比**，不用默认逐字节 hash（太慢，30G 目录会算很久）。manifest 至少覆盖相对路径、类型、字节数、mtime、attributes；后续可对小文件或抽样文件增加 hash。
- **复制语义必须明确**：递归复制不跟随源目录内部 reparse point，避免循环和重复计数；生产实现需尽量保留 NTFS 时间戳、属性、ACL、备用数据流、压缩/稀疏标记，并支持长路径。
- **恢复优先于新任务**：启动时先检查 journal 里未完成任务，按阶段恢复/清理。存在 pending 任务时，禁止对同一路径发起新迁移。
- **还原同样是状态机**：先复制到源盘临时目录并校验，再删除 junction 和原子改名。切换失败时优先重建 junction，避免应用入口路径消失。
- **进度用 Tauri event 推送**，前端被动接收，不轮询。
- **进程占用检测**：迁移前若源目录被占用，提示用户先关应用；不强杀进程（危险且不专业）。

## 5. 错误处理与边界情况

磁盘工具最容易翻车的是异常场景。明确处理策略：

**迁移阶段的失败/中断处理：**

| 阶段 | 异常 | 处理 |
|------|------|------|
| 复制中 | 磁盘满 / 权限 / 中断 | 清理 tmp；源目录未改名，状态恢复迁移前；journal 标记 failed |
| 复制中 | 用户点取消 | 清理 tmp；源目录未改名，记录"用户取消" |
| 首次校验 | manifest 不一致 | 保留 tmp（不删），标记"待人工确认"，提示用户排查 |
| 源目录改名 | 改名失败 | 通常因目录被占用。中止迁移，tmp 保留，提示用户关闭占用进程后重试；不强制改名 |
| 增量同步/二次校验 | 同步失败或 manifest 不一致 | 尚未建 junction 时，将 .dayu-old-{taskId} 改回原名；tmp 保留供排查 |
| 建链 | junction 创建失败 | 删除可能存在的半成品 junction；将 .dayu-old-{taskId} 改回原名；target/tmp 保留，标记失败 |
| 记录映射 | store 写入失败 | junction 已建好但不删除 oldPath；journal 保留 pending_record，启动恢复时优先补写或提示修复 |
| 删原 | 移回收站失败 | junction 已建好、映射已落盘，仅 oldPath 未清理；标记 old_pending_delete，提示用户可手动删 |
| 还原复制 | 复制/校验失败 | 删除 restore-tmp；保留原 junction 和目标数据，记录失败 |
| 还原切换 | 删除 junction 后改名失败 | 优先重建 junction 指回目标；restore-tmp 保留供排查 |
| 应用崩溃/断电 | 任意阶段中断 | 启动时读取 journal，按阶段恢复：未改名则清 tmp；已改名未建链则优先改回；已建链未记录则补写/提示 |

**关键原则：失败时永远优先保数据，宁可留垃圾也不删数据。** 每个阶段的失败都有明确状态，前端可看到"卡在哪一步"，并提供"清理残留"入口。

**其他边界情况：**

- **源目录被占用**：预检阶段做通用占用检测，并结合预设进程给出"请先关闭 XXX"。提供"我已关闭，重试"按钮；不强杀进程。
- **目标盘空间不足**：预检直接拦截。空间判断需要加安全余量，因为复制期间源目录可能增长，回收站也可能需要额外空间。
- **系统关键路径**：硬编码黑名单（`C:\Windows`、`C:\Program Files` 下的系统组件、`System32`、`ProgramData\Microsoft` 等），扫描可展示但迁移拒绝；首版一键迁移优先采用白名单目录。
- **重复迁移**：扫描时若发现源目录已经是 junction，或 `migrations.json` 已有 active/pending 记录，标注"已迁移/处理中"，不重复操作。
- **目标已存在**：最终目标路径采用 `{preset-or-custom}\{uuid}\data`，正常不会冲突。若发现目标/临时目录已存在，只允许"采用已有迁移记录"、"换新 ID"或"人工清理"，不提供覆盖。
- **junction 失效**：LinksView 检测到链接指向的目标不存在，标注"失效"。清理前必须确认该链接不是用户手工维护的重要链接。
- **仓库路径限制**：仓库不能选 C 盘本身，不能是网络路径，不能位于任一待迁源目录内部；目标卷需为本地 NTFS 且可写。
- **权限**：受保护目录迁移可能需要管理员权限。首版默认不提权；遇到权限不足时明确提示失败原因，并支持用户选择"以管理员身份重启后重试"作为后续能力。
- **任务并发**：同一时刻允许多个扫描任务最多一个、迁移/还原任务最多一个；实现成熟后再开放队列。运行中任务锁定源路径、目标路径和设置快照。
- **长路径与特殊文件名**：Win32 层统一处理 `\\?\` 长路径前缀、大小写差异、尾随点/空格等 Windows 路径边界，前端只展示规范化后的路径。

## 6. 数据持久化与配置

**1. 存储位置**

所有数据放在 `%LOCALAPPDATA%\dayu-disk-manager\`（即 `C:\Users\xxx\AppData\Local\dayu-disk-manager\`）。迁移映射绑定本机盘符、卷信息和 reparse point 状态，不放 Roaming，避免被漫游/同步到其他机器：

```
%LOCALAPPDATA%\dayu-disk-manager\
├─ config.json              配置（仓库路径、扫描阈值、黑名单等）
├─ migrations.json          迁移映射记录（src ↔ dst ↔ oldPath ↔ 状态）
├─ operation_journal.jsonl  运行中任务恢复日志（追加写，每行一条 JSON）
└─ history.jsonl            操作历史（追加写，每行一条 JSON）
```

**2. 数据结构**

`config.json`：
```json
{
  "schemaVersion": 1,
  "repository": "D:\\Migrated",
  "scan": {
    "minSizeMB": 500,
    "excludePaths": ["C:\\Windows", "C:\\Program Files\\WindowsApps"]
  },
  "presets": [ ]
}
```

`migrations.json`（迁移映射，回滚的依据）：
```json
[
  {
    "id": "uuid",
    "schemaVersion": 1,
    "source": "C:\\Users\\xxx\\Documents\\WeChat Files",
    "target": "D:\\Migrated\\wechat\\uuid\\data",
    "oldPath": "C:\\Users\\xxx\\Documents\\WeChat Files.dayu-old-taskId",
    "preset": "wechat",
    "createdAt": "2026-07-18T10:30:00Z",
    "status": "active",
    "sourceVolumeSerial": "xxxx-xxxx",
    "targetVolumeSerial": "yyyy-yyyy",
    "recycleBinRef": "",
    "pendingCleanup": null
  }
]
```

`operation_journal.jsonl`（恢复日志，每行一条阶段变更）：
```json
{"taskId":"task-id","op":"migrate","migrationId":"uuid","stage":"source_renamed","src":"...","dst":"...","tmp":"...","oldPath":"...","time":"..."}
```

`history.jsonl`（操作流水，每行一条）：
```json
{"op":"migrate","id":"uuid","src":"...","dst":"...","result":"ok","time":"...","durationSec":120}
```

**3. 设计要点：**

- **migrations.json / operation_journal.jsonl / history.jsonl 分工**：migrations 是"当前状态"（哪些链接还活着，回滚查它）；operation_journal 是"未完成任务恢复依据"；history 是"流水账"（审计查它）。
- **预设场景可扩展**：`presets` 写在 config 里，内置默认值首次启动注入。未来加新场景（如 Epic 游戏）只改预设，不动代码。
- **目标路径布局**：仓库下按 `{preset-or-custom}\{uuid}\data` 保存数据，避免同名目录冲突。展示给用户时显示原始目录名和应用名，不直接暴露 UUID 作为主要信息。
- **数据安全**：migrations.json 是回滚的命根子，写入使用"写临时文件 → flush → 原子 rename"；写入前保留 `.bak`，写失败回滚。绝不允许该文件损坏后用户无法还原。
- **恢复优先**：启动时先读 operation_journal，若存在未完成任务，先尝试自动修复；无法判断时进入"待人工确认"状态，并禁止对相关路径继续迁移。
- **配置校验**：仓库路径启动时校验——不能是 C 盘、不能是网络路径、不能位于待迁源目录内部、所在盘需为本地 NTFS 且可写。

## 7.开发环境启动

### 前置依赖

- **Node.js** ≥ 20 与 **pnpm**（包管理器）
- **Rust** 工具链（`rustup` 安装 stable；迁移、junction 等能力依赖 Windows target）
- 系统级：Tauri 2 在 Windows 需 **WebView2 Runtime** 与 MSVC 构建工具（Visual Studio Build Tools / C++ 桌面开发工作负载）

### 安装依赖

```bash
pnpm install
```

### 开发运行（推荐：Tauri 桌面壳）

启动 Tauri dev，会自动拉起 Vite dev server（`http://localhost:1420`）并打开桌面窗口：

```bash
pnpm tauri dev
```

> 直接 `pnpm dev` 只起前端，Tauri IPC 命令不可用，仅用于纯 UI 调样。

### 仅前端（UI 调试）

```bash
pnpm dev
```

### 类型检查 / 构建

```bash
pnpm build        # vue-tsc 类型检查 + Vite 生产构建
pnpm tauri build  # 产出 MSI / NSIS 安装包（src-tauri/tauri.conf.json 中 targets）
```

### 测试

```bash
pnpm test         # 运行 vitest 前端单测
```
