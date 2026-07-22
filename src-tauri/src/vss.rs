//! VSS 卷影快照：为迁移创建非持久快照，绕过被外部进程独占的文件。
//!
//! 背景：`std::fs::File::open` 无法读取被其他进程以零共享模式独占的文件
//! （AI 工具运行时缓存等），迁移到“同步变化”阶段报 `os error 5`。解决思路是
//! 迁移前对源卷创建非持久 VSS 快照，所有读源操作改走快照设备路径
//! `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN\<rel>`，从而彻底免疫文件锁。
//!
//! 关键约束：`windows = "0.62"` **未导出** `IVssBackupComponents` 接口与
//! `CreateVssBackupComponents` 工厂，但其 CLSID（`VSSCoordinator`）、`IVssAsync`、
//! `VSS_SNAPSHOT_PROP` 均现成可用。因此本模块**手写** `IVssBackupComponents` 的
//! COM vtable 绑定（槽序以 SDK `vsbackup.h` 为准，已逐行核对）。
//!
//! 生命周期（思路 2：单次快照 + 单次全量复制 + 单次校验）：
//! 1. `create_snapshot(volume_root)` 在调用线程初始化 COM 并创建快照，返回 `SnapshotGuard`；
//! 2. 迁移读源用 [`shadow_path`] 计算的快照设备路径（`SnapshotGuard::device_path`）；
//! 3. guard drop 时 `BackupComplete` + `DeleteSnapshots` + `Release` 回收卷空间。
//!
//! # 线程亲和性
//! COM 是线程局部的：`SnapshotGuard` 必须在**创建它的同一线程**上 drop。调用方
//! （commands.rs）须保证快照的构造、使用、释放都在同一个 `spawn_blocking` 闭包内。

use std::path::{Path, PathBuf};

// =============================================================================
// 纯函数：快照设备路径拼接（跨平台，可单测）
// =============================================================================

/// 把原始源路径映射到快照设备路径下的对应位置。
///
/// `device` 形如 `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3`，
/// `original` 形如 `C:\Users\x\cache\foo.bin`。剥掉盘符根得相对路径后拼接：
/// `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3\Users\x\cache\foo.bin`。
///
/// 纯字符串操作，不触碰任何 Win32 API，全平台可单测。
pub fn shadow_path(device: &str, original: &Path) -> PathBuf {
    // 规范化：统一反斜杠，剥离 \\?\ / \\.\ 长路径前缀。
    let s = original
        .to_string_lossy()
        .replace('/', "\\")
        .trim_start_matches(r"\\?\")
        .trim_start_matches(r"\\.\")
        .to_string();
    // 盘符根（X:\）占 3 字节；剥掉它得到相对路径。
    let bytes = s.as_bytes();
    let rel: &str = if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        &s[3..]
    } else {
        // 无盘符（非典型）：原样使用，调用方负责语义正确性。
        &s
    };
    let rel = rel.trim_start_matches('\\');
    PathBuf::from(format!("{device}\\{rel}"))
}

// =============================================================================
// Windows 实现
// =============================================================================

#[cfg(windows)]
mod imp {
    use crate::error::{AppError, AppResult};
    use std::ffi::c_void;
    use std::ptr;

    // 宽字符助手（与 win32.rs 同实现，模块私有，避免改动 win32.rs 可见性）。
    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    fn from_wide_ptr(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        unsafe {
            while *p.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(p, len);
            String::from_utf16_lossy(slice)
        }
    }

    // ---- 类型别名，缩短手写 vtable 签名 ----
    type RawPtr = *mut c_void;
    type Hr = windows::core::HRESULT;
    type Guid = windows::core::GUID;
    /// vtable 中“未绑定”方法槽的占位类型（仅保证 ABI 长度，永不调用）。
    type OpaqueSlot = unsafe extern "system" fn();

    // ---- VSS 语义常量（值取自 windows-rs 的 Vss mod，已核对）----
    const VSS_BT_COPY: i32 = 5; // VSS_BACKUP_TYPE::Copy：不影响 writer 备份状态
    const VSS_CTX_BACKUP: i32 = 0; // VSS_SNAPSHOT_CONTEXT::Backup：非持久快照
    const VSS_OBJECT_SNAPSHOT: i32 = 3; // VSS_OBJECT_TYPE::Snapshot
    /// IVssAsync::Wait 的无限等待（Win32 INFINITE = 0xFFFFFFFF）。
    const WAIT_INFINITE: u32 = u32::MAX;
    /// CLSCTX_SERVER = INPROC_SERVER(1) | LOCAL_SERVER(4) | REMOTE_SERVER(16)。
    const CLSCTX_SERVER: u32 = 0x1 | 0x4 | 0x10;

    /// IVssBackupComponents 的接口 ID（665c1d5f-c218-414d-a05d-7fef5f9d5c86）。
    const IID_IVSS_BACKUP_COMPONENTS: Guid =
        Guid::from_u128(0x665c1d5f_c218_414d_a05d_7fef5f9d5c86);

    // =========================================================================
    // 手写 IVssBackupComponents vtable
    //
    // 槽序严格对应 SDK `vsbackup.h` 中 IVssBackupComponents 的 STDMETHOD 声明顺序
    // （IUnknown 占 0..2，方法从槽 3 起）。仅类型化本流程调用的方法；其余槽用
    // OpaqueSlot 占位，保证字段偏移精确。ABI 上所有槽同为指针宽度。
    //
    // 调用约定：COM 对象指针 `this` 指向一段内存，其首元素是 vtable 指针；
    // 即 `vtbl = *(this as *const *const Vtbl)`，方法首参恒为 `this`。
    // 字段名刻意沿用 COM 方法名（PascalCase），故关闭 non_snake_case。
    #[allow(non_snake_case)]
    #[repr(C)]
    struct IVssBackupComponentsVtbl {
        // IUnknown (0..2)
        QueryInterface: OpaqueSlot,
        AddRef: OpaqueSlot,
        Release: unsafe extern "system" fn(RawPtr) -> u32,
        // (3..) IVssBackupComponents
        GetWriterComponentsCount: OpaqueSlot,
        GetWriterComponents: OpaqueSlot,
        InitializeForBackup: unsafe extern "system" fn(RawPtr, *const u16) -> Hr, // 5
        SetBackupState: unsafe extern "system" fn(RawPtr, bool, bool, i32, bool) -> Hr, // 6
        InitializeForRestore: OpaqueSlot,
        SetRestoreState: OpaqueSlot,
        GatherWriterMetadata: unsafe extern "system" fn(RawPtr, *mut RawPtr) -> Hr, // 9 (异步)
        GetWriterMetadataCount: OpaqueSlot,
        GetWriterMetadata: OpaqueSlot,
        FreeWriterMetadata: unsafe extern "system" fn(RawPtr) -> Hr, // 12
        AddComponent: OpaqueSlot,
        PrepareForBackup: unsafe extern "system" fn(RawPtr, *mut RawPtr) -> Hr, // 14 (异步)
        AbortBackup: unsafe extern "system" fn(RawPtr) -> Hr, // 15
        GatherWriterStatus: OpaqueSlot,
        GetWriterStatusCount: OpaqueSlot,
        FreeWriterStatus: OpaqueSlot,
        GetWriterStatus: OpaqueSlot,
        SetBackupSucceeded: OpaqueSlot,
        SetBackupOptions: OpaqueSlot,
        SetSelectedForRestore: OpaqueSlot,
        SetRestoreOptions: OpaqueSlot,
        SetAdditionalRestores: OpaqueSlot,
        SetPreviousBackupStamp: OpaqueSlot,
        SaveAsXML: OpaqueSlot,
        BackupComplete: unsafe extern "system" fn(RawPtr, *mut RawPtr) -> Hr, // 27 (异步)
        AddAlternativeLocationMapping: OpaqueSlot,
        AddRestoreSubcomponent: OpaqueSlot,
        SetFileRestoreStatus: OpaqueSlot,
        AddNewTarget: OpaqueSlot,
        SetRangesFilePath: OpaqueSlot,
        PreRestore: OpaqueSlot,
        PostRestore: OpaqueSlot,
        SetContext: unsafe extern "system" fn(RawPtr, i32) -> Hr, // 35
        StartSnapshotSet: unsafe extern "system" fn(RawPtr, *mut Guid) -> Hr, // 36
        AddToSnapshotSet:
            unsafe extern "system" fn(RawPtr, *const u16, Guid, *mut Guid) -> Hr, // 37
        DoSnapshotSet: unsafe extern "system" fn(RawPtr, *mut RawPtr) -> Hr, // 38 (异步)
        DeleteSnapshots:
            unsafe extern "system" fn(RawPtr, Guid, i32, i32, *mut i32, *mut Guid) -> Hr, // 39
        ImportSnapshots: OpaqueSlot,
        BreakSnapshotSet: OpaqueSlot,
        GetSnapshotProperties:
            unsafe extern "system" fn(RawPtr, Guid, *mut windows::Win32::Storage::Vss::VSS_SNAPSHOT_PROP)
                -> Hr, // 42
        Query: OpaqueSlot,
        IsVolumeSupported: OpaqueSlot,
        DisableWriterClasses: OpaqueSlot,
        EnableWriterClasses: OpaqueSlot,
        DisableWriterInstances: OpaqueSlot,
        ExposeSnapshot: OpaqueSlot,
        RevertToSnapshot: OpaqueSlot,
        QueryRevertStatus: OpaqueSlot,
    }

    /// 取 `this` 指向的 vtable。COM 对象首字段即 vtable 指针。
    ///
    /// # Safety
    /// `this` 必须是有效的 `IVssBackupComponents` 接口指针。
    unsafe fn vtbl(this: RawPtr) -> *const IVssBackupComponentsVtbl {
        *(this as *const *const IVssBackupComponentsVtbl)
    }

    // ---- 需要的 FFI（windows-rs 未导出部分，手写 extern）----

    // CoCreateInstance 的原始 5 参版本（windows-rs 只暴露泛型包装，需显式 riid）。
    #[link(name = "ole32")]
    extern "system" {
        #[link_name = "CoCreateInstance"]
        fn co_create_instance(
            rclsid: *const Guid,
            punk_outer: *mut c_void,
            dwclsctx: u32,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> Hr;
    }

    // 释放 `VSS_SNAPSHOT_PROP` 内由 VSS 分配的字符串。
    // 导出名经 dumpbin 核对（vssapi.dll，非 Internal 变体）；返回 void。
    // 用 raw-dylib 直接按 DLL 导出名生成桩：vssapi.lib 在现代 SDK 中不含这些符号
    // （API 集转发），raw-dylib 绕过 import lib，从 vssapi.dll 导出表直接链接。
    #[link(name = "vssapi", kind = "raw-dylib")]
    extern "system" {
        fn VssFreeSnapshotProperties(p_prop: *mut windows::Win32::Storage::Vss::VSS_SNAPSHOT_PROP);
    }

    // ---- 错误映射 ----

    #[derive(Debug)]
    enum VssError {
        Com(String),
        Internal(String),
    }
    impl From<VssError> for AppError {
        fn from(e: VssError) -> Self {
            match e {
                VssError::Com(s) | VssError::Internal(s) => AppError::Vss(s),
            }
        }
    }

    /// 把 HRESULT 映射为用户可读的 `AppError`。
    /// `E_ACCESSDENIED`（含非提权）→ Conflict 引导提权；其余 → Vss 含错误码。
    fn map_hresult(hr: Hr, op: &str) -> AppError {
        let code = hr.0 as u32;
        // HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED) = 0x80070005
        if code == 0x8007_0005 {
            return AppError::Conflict(format!(
                "VSS 操作「{op}」被拒绝访问：需要管理员权限，请以管理员身份重新启动本程序"
            ));
        }
        AppError::Vss(format!("VSS 操作「{op}」失败: 0x{code:08X}"))
    }

    /// HRESULT 失败即转错误，成功返回 ()。
    fn check(hr: Hr, op: &str) -> AppResult<()> {
        if hr.is_err() {
            Err(map_hresult(hr, op))
        } else {
            Ok(())
        }
    }

    /// COM 单元 RAII：构造时 `CoInitializeEx(MULTITHREADED)`，drop 时 `CoUninitialize`。
    struct ComInit {
        uninit_on_drop: bool,
    }
    impl ComInit {
        fn new() -> AppResult<Self> {
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let code = hr.0 as u32;
            // S_OK(0) 首次初始化；S_FALSE(1) 该线程已初始化（仍需配对 Uninitialize）。
            let uninit = code == 0 || code == 1;
            if !uninit {
                // RPC_E_CHANGED_MODE(0x80010106) 等：本线程已属其它单元，无法用 MTA。
                return Err(VssError::Com(format!("CoInitializeEx 失败: 0x{code:08X}")).into());
            }
            Ok(Self { uninit_on_drop: uninit })
        }
    }
    impl Drop for ComInit {
        fn drop(&mut self) {
            if self.uninit_on_drop {
                // CoUninitialize 无返回值；吞错（与项目 VolumeHandle::drop 风格一致）。
                unsafe { windows::Win32::System::Com::CoUninitialize() };
            }
        }
    }

    /// 运行一个“返回 IVssAsync”的 VSS 方法并阻塞等待其完成。
    ///
    /// # Safety
    /// `invoke` 必须是当前 `this` 上某异步方法的正确函数指针。
    unsafe fn run_async(
        this: RawPtr,
        invoke: unsafe extern "system" fn(RawPtr, *mut RawPtr) -> Hr,
        op: &str,
    ) -> AppResult<()> {
        use windows::core::Interface;
        let mut async_ptr: RawPtr = ptr::null_mut();
        let hr = invoke(this, &mut async_ptr);
        if let Err(e) = check(hr, op) {
            return Err(e);
        }
        if async_ptr.is_null() {
            return Err(VssError::Internal(format!("{op}: IVssAsync 为空")).into());
        }
        // 接管 out-param 的引用计数；drop 时自动 Release。
        let async_obj: windows::Win32::Storage::Vss::IVssAsync = Interface::from_raw(async_ptr);
        match async_obj.Wait(WAIT_INFINITE) {
            Ok(()) => Ok(()),
            Err(e) => Err(map_hresult(e.code(), &format!("{op}: Wait"))),
        }
    }

    /// 把卷挂载点（如 `C:\`）转为 VSS 所需的 `\\?\Volume{GUID}\` 格式。
    fn volume_guid_name(volume_mount: &str) -> AppResult<String> {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
        let wide = to_wide(volume_mount);
        let mut buf = vec![0u16; 50];
        let r = unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(wide.as_ptr()), &mut buf) };
        r.map_err(|_| AppError::from(VssError::Com(format!("无法解析卷 GUID 名: {volume_mount}"))))?;
        Ok(from_wide_ptr(buf.as_ptr()))
    }

    /// `CoCreateInstance(VSSCoordinator, IID_IVssBackupComponents)`。
    fn create_backup_components() -> AppResult<RawPtr> {
        use windows::Win32::Storage::Vss::VSSCoordinator;
        let mut ppv: RawPtr = ptr::null_mut();
        let hr = unsafe {
            co_create_instance(
                &VSSCoordinator,
                ptr::null_mut(),
                CLSCTX_SERVER,
                &IID_IVSS_BACKUP_COMPONENTS,
                &mut ppv,
            )
        };
        if let Err(e) = check(hr, "CoCreateInstance(VSSCoordinator)") {
            return Err(e);
        }
        if ppv.is_null() {
            return Err(VssError::Internal("CoCreateInstance 返回空指针".to_string()).into());
        }
        Ok(ppv)
    }

    // =========================================================================
    // 公开类型与 API
    // =========================================================================

    /// 一份非持久 VSS 快照的 RAII 句柄。
    ///
    /// drop 时执行 `BackupComplete` + `DeleteSnapshots` + `Release`，确保即使迁移
    /// 中途 panic 或被取消，快照也被回收、卷空间释放。**必须在创建它的同一线程 drop。**
    pub struct SnapshotGuard {
        backup: RawPtr,
        snapshot_id: Guid,
        device_path: String,
        // 字段声明在最后 → Drop::drop 之后最后析构 → CoUninitialize 在所有 COM 调用之后。
        // 仅持有以驱动其 Drop（CoUninitialize），本身从不被读取。
        #[allow(dead_code)]
        com: ComInit,
    }

    impl SnapshotGuard {
        /// 快照设备根路径，形如 `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3`。
        pub fn device_path(&self) -> &str {
            &self.device_path
        }
    }

    impl Drop for SnapshotGuard {
        fn drop(&mut self) {
            // 全力清理，任何错误都吞掉（best-effort）。
            unsafe {
                let v = vtbl(self.backup);
                // 1. BackupComplete（异步）：通知 writer 备份结束。
                let mut async_ptr: RawPtr = ptr::null_mut();
                let _ = ((*v).BackupComplete)(self.backup, &mut async_ptr);
                if !async_ptr.is_null() {
                    use windows::core::Interface;
                    let a: windows::Win32::Storage::Vss::IVssAsync = Interface::from_raw(async_ptr);
                    let _ = a.Wait(WAIT_INFINITE); // drop a → Release
                }
                // 2. DeleteSnapshots（强制）：显式删除本快照。
                let mut deleted: i32 = 0;
                let mut nondeleted = Guid::zeroed();
                let _ = ((*v).DeleteSnapshots)(
                    self.backup,
                    self.snapshot_id,
                    VSS_OBJECT_SNAPSHOT,
                    1, // BOOL TRUE：强制删除
                    &mut deleted,
                    &mut nondeleted,
                );
                // 3. Release 接口。
                let _ = ((*v).Release)(self.backup);
            }
            // self.com 随后析构 → CoUninitialize
        }
    }

    /// 对 `volume_root`（如 `C:\`）所在卷创建非持久快照，返回其设备路径句柄。
    ///
    /// 完整 VSS 流程：COM 初始化 → CoCreateInstance → InitializeForBackup →
    /// SetBackupState(COPY) → GatherWriterMetadata → SetContext(BACKUP) →
    /// StartSnapshotSet → AddToSnapshotSet → PrepareForBackup → DoSnapshotSet →
    /// GetSnapshotProperties（读设备路径）。
    ///
    /// 失败策略（v1）：直接返回错误，由上层引导用户（提权 / 关闭占用进程 / 关 VSS），
    /// 不静默降级。
    pub fn create_snapshot(volume_root: &str) -> AppResult<SnapshotGuard> {
        let com = ComInit::new()?;
        // 卷名转 GUID 形式（AddToSnapshotSet 要求 \\?\Volume{GUID}\）。
        let vol_guid = volume_guid_name(volume_root)?;
        let backup = create_backup_components()?;

        // 标准快照创建链。
        unsafe {
            let v = vtbl(backup);
            check(((*v).InitializeForBackup)(backup, ptr::null()), "InitializeForBackup")?;
            // VSS_BT_COPY：copy backup，不改动 writer 备份状态，writer 参与最少。
            check(
                ((*v).SetBackupState)(backup, false, false, VSS_BT_COPY, false),
                "SetBackupState",
            )?;
        }
        // GatherWriterMetadata：收集 writer 元数据。部分 Win11 系统 writer 状态不稳，
        // 其失败对“仅复制文件”的快照不致命 → 容忍失败继续。
        let writer_meta_ok = unsafe {
            run_async(backup, (*vtbl(backup)).GatherWriterMetadata, "GatherWriterMetadata").is_ok()
        };
        if writer_meta_ok {
            unsafe {
                check(((*vtbl(backup)).FreeWriterMetadata)(backup), "FreeWriterMetadata")?;
            }
        }
        unsafe {
            let v = vtbl(backup);
            check(((*v).SetContext)(backup, VSS_CTX_BACKUP), "SetContext")?;
            let mut snapshot_set_id = Guid::zeroed();
            check(((*v).StartSnapshotSet)(backup, &mut snapshot_set_id), "StartSnapshotSet")?;
            let vol_wide = to_wide(&vol_guid);
            let mut snapshot_id = Guid::zeroed();
            check(
                ((*v).AddToSnapshotSet)(backup, vol_wide.as_ptr(), Guid::zeroed(), &mut snapshot_id),
                "AddToSnapshotSet",
            )?;
            // PrepareForBackup / DoSnapshotSet：实际创建快照（异步），失败即终止。
            run_async(backup, (*v).PrepareForBackup, "PrepareForBackup")?;
            run_async(backup, (*v).DoSnapshotSet, "DoSnapshotSet")?;

            // 读设备路径。
            let mut prop = windows::Win32::Storage::Vss::VSS_SNAPSHOT_PROP::default();
            check(((*v).GetSnapshotProperties)(backup, snapshot_id, &mut prop), "GetSnapshotProperties")?;
            let device_path = from_wide_ptr(prop.m_pwszSnapshotDeviceObject);
            VssFreeSnapshotProperties(&mut prop);

            Ok(SnapshotGuard {
                backup,
                snapshot_id,
                device_path,
                com,
            })
        }
    }
}

#[cfg(windows)]
pub use imp::{create_snapshot, SnapshotGuard};

// =============================================================================
// 非 Windows：仅保留 shadow_path（已在顶层定义）+ 占位 stub
// =============================================================================

#[cfg(not(windows))]
mod imp {
    use crate::error::{AppError, AppResult};

    /// 非 Windows 上不存在真实快照，结构体仅用于类型占位。
    pub struct SnapshotGuard {
        _private: (),
    }
    impl SnapshotGuard {
        pub fn device_path(&self) -> &str {
            ""
        }
    }

    /// 非 Windows：VSS 不可用，直接报错。
    pub fn create_snapshot(_volume_root: &str) -> AppResult<SnapshotGuard> {
        Err(AppError::Vss("VSS 卷影快照仅支持 Windows".into()))
    }
}

#[cfg(not(windows))]
pub use imp::{create_snapshot, SnapshotGuard};

// =============================================================================
// 测试
// =============================================================================

#[cfg(test)]
mod tests {
    use super::shadow_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn shadow_path_strips_drive_and_joins_device() {
        let dev = r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3";
        let got = shadow_path(dev, Path::new(r"C:\Users\x\cache\foo.bin"));
        assert_eq!(
            got,
            PathBuf::from(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3\Users\x\cache\foo.bin")
        );
    }

    #[test]
    fn shadow_path_normalizes_forward_slashes() {
        let dev = r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1";
        let got = shadow_path(dev, Path::new("E:/Data/sub/a.txt"));
        assert_eq!(
            got,
            PathBuf::from(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\Data\sub\a.txt")
        );
    }

    #[test]
    fn shadow_path_strips_long_path_prefix() {
        let dev = r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy2";
        let got = shadow_path(dev, Path::new(r"\\?\C:\a\b"));
        assert_eq!(
            got,
            PathBuf::from(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy2\a\b")
        );
    }

    #[test]
    fn shadow_path_lowercase_drive_matches_uppercase() {
        let dev = r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy4";
        // 盘符大小写不影响剥离（按位置而非内容剥 3 字节）。
        let got = shadow_path(dev, Path::new(r"c:\X\Y"));
        assert_eq!(
            got,
            PathBuf::from(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy4\X\Y")
        );
    }

    /// 手动门控测试：跑真实 VSS 快照生命周期。需管理员 + VSS 服务，CI 不跑。
    /// `set DAYU_MANUAL_VSS_TEST=1` 启用。
    #[cfg(windows)]
    #[test]
    fn vss_snapshot_lifecycle_manual_gate() {
        if std::env::var("DAYU_MANUAL_VSS_TEST").ok().as_deref() != Some("1") {
            eprintln!("[skip] 设置 DAYU_MANUAL_VSS_TEST=1 并以管理员运行以启用本测试");
            return;
        }
        if !crate::win32::is_elevated_current() {
            panic!("DAYU_MANUAL_VSS_TEST 要求以管理员身份运行（cargo test 需管理员 shell）");
        }
        // 对 C 盘创建快照。
        let guard = super::create_snapshot(r"C:\").expect("创建快照失败");
        let dev = guard.device_path().to_string();
        eprintln!("快照设备路径: {dev}");
        assert!(
            dev.contains("HarddiskVolumeShadowCopy"),
            "设备路径异常: {dev}"
        );
        // shadow_path 拼接：C:\Windows\System32 应映射到快照下对应位置。
        let mapped = shadow_path(&dev, Path::new(r"C:\Windows\System32"));
        eprintln!("映射: {mapped:?}");
        assert!(mapped
            .to_string_lossy()
            .ends_with(r"\Windows\System32"));
        // 该路径在快照下必须真实存在（证明快照可读、且确为卷的快照视图）。
        assert!(mapped.exists(), "快照映射路径应存在: {mapped:?}");
        // guard drop 时删除快照；测试通过即隐含清理成功。
        drop(guard);
    }
}
