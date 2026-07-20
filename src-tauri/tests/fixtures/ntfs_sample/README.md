# NTFS 样本 fixture（T3）

本目录是 T3 MFT 记录解析器的真实 fixture：从一个专用小 NTFS 卷导出的在用 MFT 记录原始字节。解析器对其做 USA fixup 与属性解析，验证对真实 NTFS 字节的正确性（而非仅靠合成字节）。

## 卷与导出

- **卷：** 专用 256MB 固定 VHD（`E:\mft_fixture.vhd`），挂载为 Z:，标签 `MFTFIX`，全新 NTFS 格式化后只放入下表所列测试文件。
- **文件系统：** NTFS 3.1
- **导出工具：** `src-tauri/examples/mft_export.rs`（控制者本机构建并以管理员权限运行）。该 example 用 T0 的 `read_mft_record`（`FSCTL_GET_NTFS_FILE_RECORD`）从 `slot_count - 1` 向下枚举，把每条在用记录的**原始字节（USA 未修复）**写入 `record_<N>.bin`，卷几何参数写入 `volume_meta.txt`。
- **导出日期：** 2026-07-20
- **导出命令：** `mft_export.exe Z <本目录>/raw`

## 卷几何（`raw/volume_meta.txt`）

```
drive=Z:
bytes_per_sector=512
bytes_per_cluster=4096
bytes_per_file_record_segment=1024
mft_valid_data_length=262144
ntfs_version=3.1
slot_count=256
```

T3 解析器以 `bytes_per_sector=512`、`bytes_per_file_record_segment=1024` 为准做 USA fixup。

## 记录文件命名

`raw/record_<N>.bin`，N 为记录号（6 位零填充）。每文件 1024 字节（= `bytes_per_file_record_segment`），内容为该记录的原始字节（含 USA，未修复）。

导出记录总数：**56 条**。

## 测试文件场景（在 Z:\mft_src 下构造）

| 文件/目录 | 类型 | 覆盖的解析场景 |
|---|---|---|
| `alpha.txt` | 普通文件，resident `$DATA`（22B） | resident value；多个 `$FILE_NAME`（Win32 + 8.3 DOS）；硬链接源 |
| `sub1\hardlink_to_alpha.txt` | alpha 的硬链接 | 同一 record 多个 `$FILE_NAME` 入口（不同 parent） |
| `big.bin` | non-resident `$DATA`（1MB+5B） | non-resident `$DATA` 属性头 logical/real size；**40 个命名数据流（ADS）`stream1..stream40`**，每个 100B，触发记录属性数超 1024 → 引出 `$ATTRIBUTE_LIST` 与 extension record |
| `sub1\deep\leaf.txt` | 嵌套目录中的 resident 文件 | parent chain；namespace |
| `junction_target` | junction（mount point reparse） | `$REPARSE_POINT`，tag = `IO_REPARSE_TAG_MOUNT_POINT`；目录 + ReparsePoint |
| `dir_symlink` | 目录符号链接 | `$REPARSE_POINT`，tag = `IO_REPARSE_TAG_SYMLINK` |
| `long filename with spaces & special.txt` | 含空格/特殊字符的长名 | namespace 1 (Win32) 长名 + namespace 2 (DOS) 8.3 短名 |
| `file_with_special.txt` | 普通文件 | 多余的 `$FILE_NAME` 入口 |
| `rootfile.txt` | 根目录文件 | parent = 根记录 5 |

系统元记录（0..15）天然存在：记录 0 = `$MFT`、记录 5 = 根目录 `Z:\`、记录 6 = `$Bitmap` 等。

## 损坏记录

NTFS 上无法安全地主动损坏单条记录而不破坏整卷。**损坏记录的健壮性由 T3 的 proptest 用合成字节覆盖**（任意截断点、非法属性长度、错误 USA offset/count、非法 UTF-16 长度），真实 fixture 只覆盖"合法但多样"的场景。

## 使用约束

- fixture 必须**入库**并在普通 `cargo test` 执行（T3 简报 3.1 要求，不得 `#[ignore]`）。
- T3 集成测试（`tests/mft_fixture.rs`）从 `raw/` 读取字节，逐条做 USA fixup + 解析，断言解析出的**具体名称、父引用、sequence、stream 大小、reparse tag**（不只是 `is_ok()`）。
- 关键记录号映射（record 5 = 根、各文件 record 号）由 T3 实现者通过解析结果自行确立并在测试中钉死——本 README 不预填记录号，因 NTFS 分配顺序不保证固定。
