//! Ductile v0.24 语言内核 — 类型系统（规范 §1，行动③ D1-2 冻结）
//!
//! 规范来源：docs/v0.24_language_kernel_spec.md
//! 设计：外援 space-bunny-alpha（2026-09-29 终审 v0.24.1）
//!
//! 冻结项：Value<T> 全集 / Schema 文法 / Outcome 三态 / SkipReason 四因 /
//!         ErrCode E1xx-E5xx / 合法属性表 / 规范化比较器（逐字段一致判据）
//!
//! 本模块零依赖 std 之外的东西（serde 仅在 feature 开启时），可被
//! 编译器三遍、执行器、chaos harness、审计账本共同 import——单一事实源。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;

// ── §1 值域 ──────────────────────────────────────────────

/// v0.24 值全集。不提供 null——可选通过"省略"表达（规范 §1）。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    /// 进程内强类型视图；不得跨进程序列化（fd3/审计中禁止出现）。
    Ref(RefPath),
}

impl Value {
    /// 规范类型名（错误信息 expected/got 用，禁自由散文）。
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "str",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::List(_) => "list",
            Value::Map(_) => "map",
            Value::Ref(_) => "ref",
        }
    }

    /// IEEE-754 有限性守卫：NaN/Inf 不允许进入值域（规范 §1）。
    pub fn finite_float(f: f64) -> Result<Value, ErrCode> {
        if f.is_finite() {
            Ok(Value::Float(f))
        } else {
            Err(ErrCode::E201) // TypeMismatch：非有限数
        }
    }
}

/// `@id.field...` 强类型引用路径。编译期解析（E202/E203 在 type-check 阶段报）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RefPath {
    pub id: String,
    pub fields: Vec<String>,
}

impl fmt::Display for RefPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.id)?;
        for seg in &self.fields {
            write!(f, ".{}", seg)?;
        }
        Ok(())
    }
}

// ── §1 Schema（contract 可执行文法，EBNF 见规范补丁①）──

/// 封闭 schema：字段必填、名称唯一。`F` 的运行时表示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schema {
    Str,
    Int,
    Float,
    Bool,
    List(Box<Schema>),
    Map(Box<Schema>),
    /// 嵌套记录：字段名 → 类型（BTreeMap 保证 JCS 键序）。
    Record(BTreeMap<String, Schema>),
}

impl Schema {
    pub fn type_name(&self) -> String {
        match self {
            Schema::Str => "str".into(),
            Schema::Int => "int".into(),
            Schema::Float => "float".into(),
            Schema::Bool => "bool".into(),
            Schema::List(t) => format!("list<{}>", t.type_name()),
            Schema::Map(t) => format!("map<{}>", t.type_name()),
            Schema::Record(fields) => {
                let inner: Vec<String> =
                    fields.iter().map(|(k, v)| format!("{}:{}", k, v.type_name())).collect();
                format!("{{{}}}", inner.join(","))
            }
        }
    }

    /// 值对 schema 的编译期/运行期双向校验（结构检查；谓词在 Expr 层）。
    pub fn check(&self, v: &Value) -> Result<(), TypeMismatch> {
        match (self, v) {
            (Schema::Str, Value::Str(_)) => Ok(()),
            (Schema::Int, Value::Int(_)) => Ok(()),
            (Schema::Float, Value::Float(_)) => Ok(()),
            (Schema::Bool, Value::Bool(_)) => Ok(()),
            (Schema::List(t), Value::List(items)) => {
                for item in items {
                    t.check(item)?;
                }
                Ok(())
            }
            (Schema::Map(t), Value::Map(m)) => {
                for (_k, val) in m {
                    t.check(val)?;
                }
                Ok(())
            }
            (Schema::Record(fields), Value::Map(m)) => {
                // 字段必填、封闭：缺字段=结构失败；多余字段=结构失败
                if fields.len() != m.len() {
                    // 找出差异给 expected/got
                    for k in fields.keys() {
                        if !m.contains_key(k) {
                            return Err(TypeMismatch {
                                expected: self.type_name(),
                                got: v.type_name().to_string(),
                                detail: format!("missing field '{}'", k),
                            });
                        }
                    }
                    for k in m.keys() {
                        if !fields.contains_key(k) {
                            return Err(TypeMismatch {
                                expected: self.type_name(),
                                got: v.type_name().to_string(),
                                detail: format!("unknown field '{}'", k),
                            });
                        }
                    }
                }
                for (k, t) in fields {
                    let val = m
                        .get(k)
                        .ok_or_else(|| TypeMismatch {
                            expected: self.type_name(),
                            got: v.type_name().to_string(),
                            detail: format!("missing field '{}'", k),
                        })?;
                    t.check(val).map_err(|e| {
                        TypeMismatch {
                            expected: t.type_name(),
                            got: e.got,
                            detail: format!("field '{}': {}", k, e.detail),
                        }
                    })?;
                }
                Ok(())
            }
            (s, v) => Err(TypeMismatch {
                expected: s.type_name(),
                got: v.type_name().to_string(),
                detail: String::new(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeMismatch {
    pub expected: String,
    pub got: String,
    pub detail: String,
}

// ── §1 ErrCode（规范 §5，封闭枚举，禁 Other/Unknown）──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrCode {
    // E1xx 语法
    E101, // Syntax
    E102, // Escape/Nesting
    E103, // Deprecated
    E104, // RawInterpolation
    // E2xx 类型
    E201, // TypeMismatch
    E202, // Unresolved
    E203, // UnknownField
    E204, // DependencyCycle
    E205, // Scope
    E206, // PlanMismatch
    E207, // PredicateType
    E208, // AuthCompile：能力契约编译期授权失败（治理超图；失败不隔离，拒绝进入 registry）
    // E3xx 执行
    E301, // Spawn
    E302, // Exit
    E303, // Timeout（注：硬化方案沙箱超时用 E405，进程级超时仍 E303）
    E304, // Codec
    E305, // Backend（暂态）
    E306, // Protocol
    E307, // Schema
    E308, // Limit
    E309, // Cancel
    E310, // NotFound
    E311, // Conflict
    E312, // Args
    E313, // Reducer
    E314, // Auth
    E315, // Registry
    E316, // DependencyFailed
    E317, // PayloadUnavailable
    E318, // IO
    // E4xx 沙箱
    E401, // Permission
    E402, // RawAudit
    E403, // Secret
    E404, // JudgeIndependent / 沙箱启动失败（见硬化方案 §2 映射）
    E405, // JudgePolicy / 沙箱超时
    E406, // 资源超限（内存/PID/CPU/磁盘/输出）
    E407, // 网络策略拒绝
    E408, // 路径策略拒绝
    E409, // Worker 异常退出
    // E42x 治理（sonet 第四轮：每码唯一检测者+时点；优先级 E404>E421>E423>E424>E425>E422>E420）
    E420, // EffectScopeRuntime：运行期 preflight/提交越权（仅兜底，专属码优先）
    E421, // EffectMismatch：registry/沙箱 mediator 发现实际 effect 与声明不符
    E422, // QuotaViolation：配额预留/结算越界（预算维度=wall/cpu/as/fd/outbox）
    E423, // CredentialInvalid：签名信封验证失败（根/issuer/时效/撤销/受众）
    E424, // BeforeHashMismatch：head-CAS 失配（陈旧事件/并发写冲突）
    E425, // ArtifactMismatch：candidate/decision/envelope 摘要不一致（含 fd3 语义伪造）
    // E5xx 契约
    E501, // OutputContract
    E502, // Invariant
    E503, // NoEligible
    E504, // DeliveryMissing
}

impl ErrCode {
    /// 稳定错误码字符串——诊断与审计的唯一标识。
    pub fn code(&self) -> &'static str {
        match self {
            Self::E101 => "E101",
            Self::E102 => "E102",
            Self::E103 => "E103",
            Self::E104 => "E104",
            Self::E201 => "E201",
            Self::E202 => "E202",
            Self::E203 => "E203",
            Self::E204 => "E204",
            Self::E205 => "E205",
            Self::E206 => "E206",
            Self::E207 => "E207",
            Self::E208 => "E208",
            Self::E301 => "E301",
            Self::E302 => "E302",
            Self::E303 => "E303",
            Self::E304 => "E304",
            Self::E305 => "E305",
            Self::E306 => "E306",
            Self::E307 => "E307",
            Self::E308 => "E308",
            Self::E309 => "E309",
            Self::E310 => "E310",
            Self::E311 => "E311",
            Self::E312 => "E312",
            Self::E313 => "E313",
            Self::E314 => "E314",
            Self::E315 => "E315",
            Self::E316 => "E316",
            Self::E317 => "E317",
            Self::E318 => "E318",
            Self::E401 => "E401",
            Self::E402 => "E402",
            Self::E403 => "E403",
            Self::E404 => "E404",
            Self::E405 => "E405",
            Self::E406 => "E406",
            Self::E407 => "E407",
            Self::E408 => "E408",
            Self::E409 => "E409",
            Self::E420 => "E420",
            Self::E421 => "E421",
            Self::E422 => "E422",
            Self::E423 => "E423",
            Self::E424 => "E424",
            Self::E425 => "E425",
            Self::E501 => "E501",
            Self::E502 => "E502",
            Self::E503 => "E503",
            Self::E504 => "E504",
        }
    }

    /// 稳定名称（与规范 §5 中央表一致；此表是唯一来源）。
    pub fn name(&self) -> &'static str {
        match self {
            Self::E101 => "Syntax",
            Self::E102 => "EscapeNesting",
            Self::E103 => "Deprecated",
            Self::E104 => "RawInterpolation",
            Self::E201 => "TypeMismatch",
            Self::E202 => "Unresolved",
            Self::E203 => "UnknownField",
            Self::E204 => "DependencyCycle",
            Self::E205 => "Scope",
            Self::E206 => "PlanMismatch",
            Self::E207 => "PredicateType",
            Self::E208 => "AuthCompile",
            Self::E301 => "Spawn",
            Self::E302 => "Exit",
            Self::E303 => "Timeout",
            Self::E304 => "Codec",
            Self::E305 => "Backend",
            Self::E306 => "Protocol",
            Self::E307 => "Schema",
            Self::E308 => "Limit",
            Self::E309 => "Cancel",
            Self::E310 => "NotFound",
            Self::E311 => "Conflict",
            Self::E312 => "Args",
            Self::E313 => "Reducer",
            Self::E314 => "Auth",
            Self::E315 => "Registry",
            Self::E316 => "DependencyFailed",
            Self::E317 => "PayloadUnavailable",
            Self::E318 => "IO",
            Self::E401 => "Permission",
            Self::E402 => "RawAudit",
            Self::E403 => "Secret",
            Self::E404 => "JudgeIndependent",
            Self::E405 => "JudgePolicy",
            Self::E406 => "ResourceQuota",
            Self::E407 => "NetPolicy",
            Self::E408 => "PathPolicy",
            Self::E409 => "WorkerCrash",
            Self::E420 => "EffectScopeRuntime",
            Self::E421 => "EffectMismatch",
            Self::E422 => "QuotaViolation",
            Self::E423 => "CredentialInvalid",
            Self::E424 => "BeforeHashMismatch",
            Self::E425 => "ArtifactMismatch",
            Self::E501 => "OutputContract",
            Self::E502 => "Invariant",
            Self::E503 => "NoEligible",
            Self::E504 => "DeliveryMissing",
        }
    }
}

impl fmt::Display for ErrCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.code(), self.name())
    }
}

// ── §1 Error 载荷 ────────────────────────────────────────

/// 运行期错误载荷：code + detail（无自由文本控制流；detail 仅展示）。
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub code: ErrCode,
    pub detail: String,
}

// ── §1 SkipReason（四因，Ineligible 不设第四态——终审判定）──

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `.when` 谓词为 false（含 detail 供审计）。
    Ineligible(String),
    /// 必要上游被跳过。
    UpstreamSkipped,
    /// plan/pick 未选中本分支；对父状态不可见（补丁②）。
    NotSelected,
    /// foreach 空源。
    EmptyInput,
}

impl SkipReason {
    pub fn name(&self) -> &'static str {
        match self {
            SkipReason::Ineligible(_) => "Ineligible",
            SkipReason::UpstreamSkipped => "UpstreamSkipped",
            SkipReason::NotSelected => "NotSelected",
            SkipReason::EmptyInput => "EmptyInput",
        }
    }
}

// ── §1 Outcome 三态（全引擎唯一结果协议）──────────────────

pub type Meta = BTreeMap<String, Value>;

/// Success 的 fields 类型由 Schema 封闭；Failed/Skipped 的 fields
/// 恒为空 map（Payload<F> = F | EmptyMap，补丁①）。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Success {
        fields: BTreeMap<String, Value>,
        stdout: String,
        meta: Meta,
    },
    Failed {
        error: Error,
        stdout: String,
        meta: Meta,
    },
    Skipped {
        reason: SkipReason,
        stdout: String,
        meta: Meta,
    },
}

impl Outcome {
    // —— 合法属性表（补丁①：@x.status/ok/stdout/meta/fields/error/reason）——

    pub fn status(&self) -> &'static str {
        match self {
            Outcome::Success { .. } => "Success",
            Outcome::Failed { .. } => "Failed",
            Outcome::Skipped { .. } => "Skipped",
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self, Outcome::Success { .. })
    }

    pub fn stdout(&self) -> &str {
        match self {
            Outcome::Success { stdout, .. }
            | Outcome::Failed { stdout, .. }
            | Outcome::Skipped { stdout, .. } => stdout,
        }
    }

    pub fn meta(&self) -> &Meta {
        match self {
            Outcome::Success { meta, .. }
            | Outcome::Failed { meta, .. }
            | Outcome::Skipped { meta, .. } => meta,
        }
    }

    /// `@x.fields`：Success → 字段 map；非 Success → 空 map（合法，非错误）。
    pub fn fields(&self) -> &BTreeMap<String, Value> {
        static EMPTY: std::sync::OnceLock<BTreeMap<String, Value>> = std::sync::OnceLock::new();
        match self {
            Outcome::Success { fields, .. } => fields,
            _ => EMPTY.get_or_init(BTreeMap::new),
        }
    }

    /// `@x.error`：仅 Failed 合法；其余 E317 PayloadUnavailable（补丁①）。
    pub fn error(&self) -> Result<&Error, ErrCode> {
        match self {
            Outcome::Failed { error, .. } => Ok(error),
            _ => Err(ErrCode::E317),
        }
    }

    /// `@x.reason`：仅 Skipped 合法；其余 E317。
    pub fn reason(&self) -> Result<&SkipReason, ErrCode> {
        match self {
            Outcome::Skipped { reason, .. } => Ok(reason),
            _ => Err(ErrCode::E317),
        }
    }

    /// `@x.fields.k`：仅 Success 且 k∈F；否则 E317（不返回伪造默认值）。
    pub fn field(&self, key: &str) -> Result<&Value, ErrCode> {
        match self {
            Outcome::Success { fields, .. } => {
                fields.get(key).ok_or(ErrCode::E317)
            }
            _ => Err(ErrCode::E317),
        }
    }

    // —— 构造便捷 ——
    pub fn success(fields: BTreeMap<String, Value>, stdout: impl Into<String>) -> Self {
        Outcome::Success { fields, stdout: stdout.into(), meta: Meta::new() }
    }

    pub fn failed(code: ErrCode, detail: impl Into<String>) -> Self {
        Outcome::Failed {
            error: Error { code, detail: detail.into() },
            stdout: String::new(),
            meta: Meta::new(),
        }
    }

    pub fn skipped(reason: SkipReason) -> Self {
        Outcome::Skipped { reason, stdout: String::new(), meta: Meta::new() }
    }

    /// 契约失败把结果改写为 Failed(OutputContract/Invariant)（规范 §1）。
    pub fn contract_fail(mut self, invariant: bool) -> Self {
        let stdout = self.stdout().to_string();
        let meta = self.meta().clone();
        self = Outcome::Failed {
            error: Error {
                code: if invariant { ErrCode::E502 } else { ErrCode::E501 },
                detail: String::new(),
            },
            stdout,
            meta,
        };
        self
    }
}

// ── 规范化比较器（行动③ D1-2：恢复判据"逐字段一致"）──────

/// evidence 字段 vs result 字段的分类（行动③ §1：run_id/时间戳是 evidence，
/// 不参与一致性判定）。
pub const EVIDENCE_META_KEYS: &[&str] = &["run_id", "ts", "started_at", "finished_at", "attempt"];

/// 规范化投影：取 Outcome 的 result 视图（status + fields），剥离 evidence。
pub fn result_projection(out: &Outcome) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("status".to_string(), Value::Str(out.status().to_string()));
    if let Outcome::Success { fields, .. } = out {
        for (k, v) in fields {
            m.insert(format!("fields.{}", k), v.clone());
        }
    }
    m
}

/// 逐字段一致判据（chaos harness 判据 4）。
/// 浮点比较：bit-exact（规范：禁止隐式转换；恢复一致性要求 exact）。
pub fn results_match(base: &Outcome, replay: &Outcome) -> Result<(), FieldDiff> {
    let a = result_projection(base);
    let b = result_projection(replay);
    let mut diffs = Vec::new();
    for k in a.keys().chain(b.keys()).collect::<std::collections::BTreeSet<_>>() {
        match (a.get(k), b.get(k)) {
            (Some(x), Some(y)) if x == y => {}
            (x, y) => diffs.push(FieldDiff {
                field: k.clone(),
                base: x.cloned().map(|v| format!("{:?}", v)),
                replay: y.cloned().map(|v| format!("{:?}", v)),
            }),
        }
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(FieldDiff { field: String::new(), base: None, replay: None }.merge_all(diffs))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDiff {
    pub field: String,
    pub base: Option<String>,
    pub replay: Option<String>,
}

impl FieldDiff {
    fn merge_all(self, mut rest: Vec<Self>) -> Self {
        // 单 diff 快路径
        if rest.len() == 1 {
            return rest.remove(0);
        }
        self
    }
}

// ── effect key（行动③判据 1：已确认副作用重复数=0 的实现前提）──

/// 稳定 effect key：hash(plan_fingerprint, effect_index, input_digest)。
/// 已确认副作用（acked effect）以 effect key 做唯一约束——重启后同 key
/// 不重复执行（幂等 inbox/outbox）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EffectKey {
    pub plan_fingerprint: String,
    pub effect_index: u32,
    pub input_digest: String,
}

impl fmt::Display for EffectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}#{}", self.plan_fingerprint, self.effect_index, self.input_digest)
    }
}

// ── 事件账本记录（行动③ §5：审计 JSONL 是唯一事实源）───────

/// 审计事件（追加式 JSONL 行）。prev_hash 链在账本层维护。
#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub seq: u64,
    pub run_id: String,
    pub step_id: String,
    pub attempt: u32,
    pub scenario: Option<String>,
    pub seed: Option<u64>,
    pub fault: Option<String>,
    pub outcome_status: &'static str,
    pub error_code: Option<ErrCode>,
    pub skip_reason: Option<&'static str>,
    pub effect_key: Option<EffectKey>,
    pub input_digest: Option<String>,
    pub output_digest: Option<String>,
    pub prev_hash: String,
}

// ── 单元测试：冻结即验（规范即测试）──────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn outcome_attribute_table() {
        // @x.ok / status / fields / error / reason 合法性表（补丁①）
        let ok = Outcome::success(fields(&[("n", Value::Int(1))]), "out");
        assert!(ok.ok());
        assert_eq!(ok.status(), "Success");
        assert_eq!(ok.field("n"), Ok(&Value::Int(1)));
        assert_eq!(ok.error(), Err(ErrCode::E317)); // Success 无 error
        assert_eq!(ok.reason(), Err(ErrCode::E317));

        let bad = Outcome::failed(ErrCode::E302, "exit 1");
        assert!(!bad.ok());
        assert_eq!(bad.error().unwrap().code, ErrCode::E302);
        assert_eq!(bad.field("n"), Err(ErrCode::E317)); // Failed 无 fields
        assert_eq!(bad.fields().len(), 0);

        let skip = Outcome::skipped(SkipReason::Ineligible("score<80".into()));
        assert_eq!(skip.status(), "Skipped");
        assert_eq!(skip.reason().unwrap(), &SkipReason::Ineligible("score<80".into()));
        assert_eq!(skip.error(), Err(ErrCode::E317));
    }

    #[test]
    fn schema_closed_check() {
        let sch = Schema::Record(BTreeMap::from([
            ("path".to_string(), Schema::Str),
            ("bytes".to_string(), Schema::Int),
        ]));
        // 精确匹配
        assert!(sch
            .check(&Value::Map(fields(&[("path", Value::Str("a".into())), ("bytes", Value::Int(3))])))
            .is_ok());
        // 缺字段
        let e = sch.check(&Value::Map(fields(&[("path", Value::Str("a".into()))]))).unwrap_err();
        assert_eq!(e.detail, "missing field 'bytes'");
        // 多余字段
        let e = sch
            .check(&Value::Map(fields(&[
                ("path", Value::Str("a".into())),
                ("bytes", Value::Int(3)),
                ("extra", Value::Bool(true)),
            ])))
            .unwrap_err();
        assert_eq!(e.detail, "unknown field 'extra'");
        // 类型不符
        let e = sch
            .check(&Value::Map(fields(&[("path", Value::Int(1)), ("bytes", Value::Int(3))])))
            .unwrap_err();
        assert!(e.detail.starts_with("field 'path'"));
        // 无隐式转换："1" 不是 int
        let e = sch
            .check(&Value::Map(fields(&[
                ("path", Value::Str("a".into())),
                ("bytes", Value::Str("1".into())),
            ])))
            .unwrap_err();
        assert_eq!(e.expected, "int");
        assert_eq!(e.got, "str");
    }

    #[test]
    fn contract_fail_rewrites() {
        let ok = Outcome::success(fields(&[("x", Value::Int(1))]), "s");
        let f = ok.clone().contract_fail(false);
        assert_eq!(f.error().unwrap().code, ErrCode::E501);
        let f2 = ok.contract_fail(true);
        assert_eq!(f2.error().unwrap().code, ErrCode::E502);
        // stdout/meta 保留
        assert_eq!(f.stdout(), "s");
    }

    #[test]
    fn result_projection_and_match() {
        let a = Outcome::success(fields(&[("digest", Value::Str("d1".into()))]), "x");
        let b = Outcome::success(fields(&[("digest", Value::Str("d1".into()))]), "y");
        // stdout 是 evidence（不进投影）→ 一致
        assert!(results_match(&a, &b).is_ok());
        let c = Outcome::success(fields(&[("digest", Value::Str("d2".into()))]), "x");
        assert!(results_match(&a, &c).is_err());
        // 状态不同
        let d = Outcome::skipped(SkipReason::EmptyInput);
        assert!(results_match(&a, &d).is_err());
    }

    #[test]
    fn errcode_table_unique() {
        // 码与名称一一对应、无空（中央表自检）
        let all = [
            ErrCode::E101, ErrCode::E102, ErrCode::E103, ErrCode::E104,
            ErrCode::E201, ErrCode::E202, ErrCode::E203, ErrCode::E204, ErrCode::E205,
            ErrCode::E206, ErrCode::E207, ErrCode::E208,
            ErrCode::E301, ErrCode::E302, ErrCode::E303, ErrCode::E304, ErrCode::E305,
            ErrCode::E306, ErrCode::E307, ErrCode::E308, ErrCode::E309, ErrCode::E310,
            ErrCode::E311, ErrCode::E312, ErrCode::E313, ErrCode::E314, ErrCode::E315,
            ErrCode::E316, ErrCode::E317, ErrCode::E318,
            ErrCode::E401, ErrCode::E402, ErrCode::E403, ErrCode::E404, ErrCode::E405,
            ErrCode::E406, ErrCode::E407, ErrCode::E408, ErrCode::E409,
            ErrCode::E420, ErrCode::E421, ErrCode::E422, ErrCode::E423, ErrCode::E424,
            ErrCode::E425,
            ErrCode::E501, ErrCode::E502, ErrCode::E503, ErrCode::E504,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for c in all {
            assert!(seen.insert(c.code()), "dup code {}", c.code());
            assert!(!c.name().is_empty());
        }
    }

    #[test]
    fn float_domain_finite() {
        assert!(Value::finite_float(1.5).is_ok());
        assert_eq!(Value::finite_float(f64::NAN), Err(ErrCode::E201));
        assert_eq!(Value::finite_float(f64::INFINITY), Err(ErrCode::E201));
    }

    #[test]
    fn effect_key_stable() {
        let k = EffectKey {
            plan_fingerprint: "abc".into(),
            effect_index: 2,
            input_digest: "deadbeef".into(),
        };
        assert_eq!(k.to_string(), "abc#2#deadbeef");
    }
}
