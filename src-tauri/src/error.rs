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

    #[error("VSS 错误: {0}")]
    Vss(String),

    #[error("stale_scan")]
    StaleScan,
}

// Tauri 命令返回的 Result<T, AppError> 必须可序列化给前端
impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
