# dayu-disk-manager
一款一站式磁盘空间治理工具，如同大禹治水，梳理磁盘冗余文件、分区、存储碎片，规范化管理本地 / 服务器磁盘。

## 工程结构

Tauri 2 桌面应用，前端 Vue 3 + TypeScript + Vite，后端 Rust，两端通过 Tauri IPC 通信。

```
dayu-disk-manager/
├── src/                       # 前端（Vue 3 + TS）
│   ├── main.ts                # 应用入口，挂载 Pinia / Router
│   ├── App.vue                # 根组件（无边框窗口外壳）
│   ├── router/                # 路由表，按页面分模块
│   ├── views/                 # 页面级组件
│   │   ├── ScanView.vue       # 磁盘扫描与体积排行
│   │   ├── MigrateView.vue    # 迁移执行与进度
│   │   ├── HistoryView.vue    # 历史记录与恢复
│   │   ├── LinksView.vue      # 失效 / 有效 junction 链接管理
│   │   └── SettingsView.vue   # 设置
│   ├── components/             # 通用组件（ProgressStage / SizeCell）
│   ├── stores/                # Pinia 状态：scan / migrate / links
│   ├── ipc/                   # Tauri 命令封装、事件订阅、类型定义
│   └── styles.css             # 全局样式
├── src-tauri/                 # 后端（Rust）
│   ├── src/
│   │   ├── lib.rs / main.rs   # Tauri 应用装配与命令注册入口
│   │   ├── commands.rs        # 暴露给前端的 #[tauri::command] 接口层
│   │   ├── scanner.rs         # 目录体积扫描
│   │   ├── migrator.rs        # 迁移主流程（复制 → 改名 → junction）
│   │   ├── junction.rs        # Windows junction（目录符号链接）创建/解析
│   │   ├── file_ops.rs        # 文件复制 / 删除 / 改名原语
│   │   ├── process_probe.rs   # 进程占用探测（重启管理器 / 快照）
│   │   ├── safety.rs          # 迁移前安全检查
│   │   ├── journal.rs         # 迁移日志（崩溃恢复依据）
│   │   ├── history.rs         # 历史记录持久化
│   │   ├── store.rs           # 应用状态存储
│   │   ├── app_state.rs       # 全局运行时状态
│   │   ├── win32.rs           # Windows API 封装
│   │   ├── models.rs          # 数据模型 / 序列化结构
│   │   └── error.rs           # 统一错误类型
│   ├── Cargo.toml             # Rust 依赖
│   └── tauri.conf.json        # Tauri 构建 / 窗口 / 打包配置
├── docs/                      # 文档
├── package.json               # 前端脚本与依赖（pnpm）
├── vite.config.ts             # Vite 配置（dev 端口 1420）
└── tsconfig.json              # TS 配置
```

## 开发环境启动

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
