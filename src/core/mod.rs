//! core — 层间共享词汇表（AST 类型 + DSL_RESULT 协议）。
//! 认知栈各层都可引用（类比：所有程序层共享同一机器码编码）。
//! 规则：core 不依赖任何 L* 层——只做被依赖方。
pub mod ast;
pub mod dslresult;
pub mod replay_eval;
pub mod script_card;
pub use ast::*;
pub use dslresult::*;
pub use replay_eval::*;
pub use script_card::*;
