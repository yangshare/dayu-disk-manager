// 编译期冒烟测试：验证计划 T1 要求的 Rust 依赖能解析、版本无冲突、
// 所需 feature 已启用。若依赖版本冲突或 feature 缺失，cargo test 会编译
// 失败在此处暴露。
#[test]
fn dependencies_resolve() {
    let _ = serde_json::json!({"ok": true});
    let _ = uuid::Uuid::new_v4();
    let _ = chrono::Utc::now();
}
