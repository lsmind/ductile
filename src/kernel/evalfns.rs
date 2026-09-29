//! 内建函数求值（len/contains/unique/get）——与 check.rs 的封闭签名一一对应。
//! 运行期与常量折叠共用一个实现（无第二套真相）。

use crate::kernel::ast::CmpOp;
use crate::kernel::types::Value;

pub fn apply(name: &str, args: &[Value]) -> Option<Value> {
    match (name, args) {
        ("len", [Value::Str(s)]) => Some(Value::Int(s.chars().count() as i64)),
        ("len", [Value::List(v)]) => Some(Value::Int(v.len() as i64)),
        ("len", [Value::Map(m)]) => Some(Value::Int(m.len() as i64)),
        ("contains", [Value::List(v), needle]) => {
            Some(Value::Bool(v.contains(needle)))
        }
        ("unique", [Value::List(v)]) => {
            let mut seen = std::collections::BTreeSet::new();
            for item in v {
                if !seen.insert(format!("{:?}", item)) {
                    return Some(Value::Bool(false));
                }
            }
            Some(Value::Bool(true))
        }
        ("get", [Value::Map(m), Value::Str(k)]) => m.get(k).cloned(),
        _ => None, // 签名不符 → None；类型层已拦（E201/E207），这是运行期兜底
    }
}

/// 同型比较（check 层已保证同型；这里只做值比较）。
/// 浮点：bit-exact（规范：禁隐式转换；恢复一致性要求 exact）。
pub fn cmp_value(op: CmpOp, l: &Value, r: &Value) -> Option<bool> {
    use crate::kernel::ast::CmpOp::*;
    let ord = |o: std::cmp::Ordering| -> bool {
        match op {
            Eq => o == std::cmp::Ordering::Equal,
            Ne => o != std::cmp::Ordering::Equal,
            Lt => o == std::cmp::Ordering::Less,
            Le => o != std::cmp::Ordering::Greater,
            Gt => o == std::cmp::Ordering::Greater,
            Ge => o != std::cmp::Ordering::Less,
        }
    };
    match (l, r) {
        (Value::Str(a), Value::Str(b)) => Some(ord(a.cmp(b))),
        (Value::Int(a), Value::Int(b)) => Some(ord(a.cmp(b))),
        (Value::Float(a), Value::Float(b)) => Some(ord(a.partial_cmp(b)?)),
        (Value::Bool(a), Value::Bool(b)) if matches!(op, Eq | Ne) => Some(if matches!(op, Eq) { a == b } else { a != b }),
        (Value::List(a), Value::List(b)) if matches!(op, Eq | Ne) => {
            let eq = a == b;
            Some(if matches!(op, Eq) { eq } else { !eq })
        }
        _ => None, // 序比较用于 list/map → None（check 层 E207 已拦）
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn len_contains_unique_get() {
        assert_eq!(apply("len", &[Value::Str("轨道".into())]), Some(Value::Int(2)));
        let list = Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]);
        assert_eq!(apply("len", &[list.clone()]), Some(Value::Int(2)));
        assert_eq!(
            apply("contains", &[list.clone(), Value::Str("a".into())]),
            Some(Value::Bool(true))
        );
        assert_eq!(
            apply("unique", &[Value::List(vec![Value::Int(1), Value::Int(1)])]),
            Some(Value::Bool(false))
        );
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(9));
        assert_eq!(
            apply("get", &[Value::Map(m), Value::Str("k".into())]),
            Some(Value::Int(9))
        );
        // 未知函数 → None（fail-closed）
        assert_eq!(apply("freestyle", &[Value::Int(1)]), None);
    }

    #[test]
    fn cmp_exact() {
        use crate::kernel::ast::CmpOp::*;
        assert_eq!(cmp_value(Eq, &Value::Int(1), &Value::Int(1)), Some(true));
        assert_eq!(cmp_value(Lt, &Value::Int(1), &Value::Int(2)), Some(true));
        assert_eq!(cmp_value(Ge, &Value::Str("a".into()), &Value::Str("b".into())), Some(false));
        // NaN 比较为 None（partial_cmp）——值域已在 finite_float 拦 NaN
        assert_eq!(cmp_value(Lt, &Value::List(vec![]), &Value::List(vec![])), None);
    }
}
