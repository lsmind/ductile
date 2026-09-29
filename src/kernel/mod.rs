//! Ductile v0.24 语言内核 — 模块根。
//!
//! 平行生长策略（v0.24.1 迁移节）：内核模块独立于存量 L0-L4，
//! 不动现有 parser/executor；sidecar 期内两套前端并存，
//! 每条管线固定 language_version，禁同管线混用。

pub mod types;
pub mod golden;
pub mod ast;
pub mod check;
pub mod hash;
pub mod evalfns;
