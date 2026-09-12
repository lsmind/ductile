//! time — L0 时间原语：Unix 秒 → CST 日历格式化。
//! v0.18.5 从 harvest 下沉：纯日历算法是物理基座（机器码位的"时钟"），
//! L0(db 落库时间戳) 与 L4(harvest 展示) 共用。零 I/O、零依赖。
//!
//! 算法：Howard Hinnant 的 civil_from_days（days since 1970-01-01 → y/m/d），
//! 无 chrono 依赖，时区固定 CST(UTC+8)——引擎约定（与历史库数据一致）。

/// Howard Hinnant's civil_from_days (days since 1970-01-01 → y/m/d).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 当前时刻的 CST 时间戳（`YYYY-MM-DDTHH:MM:SS`，db 落库格式）。
pub fn now_ts() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    fmt_ts_cst(secs, "T", true)
}

/// Unix 秒（UTC）→ CST 展示格式（内部 +8h）。`sep` = 日期时间分隔符
/// （"T" 落库 / " " 展示）。`with_secs` = 是否带秒（落库 true / harvest 展示 false）。
/// 输入 < 0 时返回 "?"（沿用 harvest::fmt_ts 历史约定）。
pub fn fmt_ts_cst(secs_utc: i64, sep: &str, with_secs: bool) -> String {
    let secs = secs_utc + 8 * 3600; // CST (UTC+8)
    if secs < 0 {
        return "?".into(); // 历史约定：+8h 后仍 < 0（1969 年前）显示 "?"
    }
    let days = secs.div_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let rem = secs.rem_euclid(86400);
    if with_secs {
        format!(
            "{:04}-{:02}-{:02}{}{:02}:{:02}:{:02}",
            y,
            m,
            d,
            sep,
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60
        )
    } else {
        format!(
            "{:04}-{:02}-{:02}{}{:02}:{:02}",
            y,
            m,
            d,
            sep,
            rem / 3600,
            (rem % 3600) / 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 2024-01-01
                                                           // 闰日 2024-02-29 = 19782（python 核对）
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn cst_formatting_roundtrip() {
        // 1789171200 = 2026-09-12 00:00 UTC（date -u 核对）→ CST 显示 09-12 08:00:00
        let s = fmt_ts_cst(1_789_171_200, "T", true);
        assert!(s.starts_with("2026-09-12T08:00:00"), "got {}", s);
        // 展示格式（无秒）：+3661s → CST 09:01:01，无秒显示 09:01
        let s2 = fmt_ts_cst(1_789_171_200 + 3661, " ", false);
        assert!(s2.starts_with("2026-09-12 09:01"), "got {}", s2);
        // 负数（+8h 后仍 < 0，即 1969 前）→ "?"（对齐 harvest::fmt_ts 历史约定）
        assert_eq!(fmt_ts_cst(-8 * 3600 - 1, "T", true), "?");
        // 边界内：-8h（CST 1970-01-01 00:00）应正常显示而非 "?"
        assert_eq!(fmt_ts_cst(-8 * 3600, "T", true), "1970-01-01T00:00:00");
    }

    #[test]
    fn now_ts_shape() {
        let s = now_ts();
        assert_eq!(s.len(), 19, "YYYY-MM-DDTHH:MM:SS = 19 chars, got {}", s);
        assert!(s.as_bytes()[10] == b'T');
    }
}
