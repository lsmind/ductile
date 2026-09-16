//! script_card — 脚本契约卡（纯数据 + parse，零执行逻辑）。
//! v0.18.5 分层重组下沉：L0(db 存储) 与 L2(script 执行) 共享的数据契约。
//! 真正的解释器映射与 CSE 判定留在 L2_orchestration/script.rs。
/// 一张脚本契约卡。LLM 通过 `ductile script show <name>` 读这张卡即可调用，
/// 无需阅读脚本本体。
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptCard {
    pub name: String,
    pub path: String,
    pub lang: String,
    pub desc: String,
    /// 原始 params 声明（逗号分隔的 `name(type, required|default=X)`）
    pub params: String,
    /// 原始 output 声明（逗号分隔的 `field(type)`）
    pub output: String,
    pub pure: bool,
    pub idempotent: bool,
    pub concurrency: Concurrency,
    /// 原始 effects 声明（逗号分隔：none/fs/net/process/system）
    pub effects: String,
    pub timeout_secs: u64,
    pub retries: usize,
    /// v0.18.11 MCSM/FOPT 认知坐标：F(f)-O(o)-P(p)-T(t)，各维 1-4
    /// （建表/冲突/抽象/实践）。空串 = 未标注。解析见 script::parse_mcsm。
    pub mcsm: String,
    /// v0.19.x FOPT 强制实例级注解：mcsm 声明时必须伴随四维具体指称
    /// （F/O/P/T 各一行 `# mcsm_note_f: 文件系统`），说明该维度在当前环境下
    /// **具体是什么**（不是通用维度名"场域"——那等于没说）。空串 = mcsm 未声明。
    /// 解析校验见 script::parse_contract。
    pub mcsm_note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    Safe,
    Exclusive,
    Serial,
}

impl Concurrency {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "safe" => Ok(Concurrency::Safe),
            "exclusive" => Ok(Concurrency::Exclusive),
            "serial" => Ok(Concurrency::Serial),
            other => Err(format!(
                "unknown concurrency '{}' (known: safe/exclusive/serial)",
                other
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Concurrency::Safe => "safe",
            Concurrency::Exclusive => "exclusive",
            Concurrency::Serial => "serial",
        }
    }
}
