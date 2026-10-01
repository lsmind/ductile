//! TOON canonical codec（TOON-1.3-ductile-1 profile，冻结规格 docs/sonet_toon_spec.md v1.1 §一/§二）。
//!
//! P1 阶段：纯新增模块，零改动存量路径。设计要点：
//! - profile 固定：UTF-8、禁 BOM/CR/CRLF/Tab 缩进/行尾空格/注释；LF=0x0A 唯一物理换行。
//! - 根=对象；行=`key: scalar`；嵌套对象=父行 `key:` 无值 + 子行缩进 2 空格/层。
//! - 固定 schema 对象按 schema 字段序（CANON_ORDER 沿用），不排序；动态键按 UTF-8 字节序。
//! - 字符串最小引号：仅在空串/首尾空白/含分隔符/控制字符/词法歧义（数字、bool、null 形）时加引号。
//! - 转义固定小集合：\" \\ \n \r \t \b \f + \u00xx（小写 hex）；带引号值内禁物理换行。
//! - 标量数组 `key[N]: v1,v2`；同构对象数组表格式 `key[N]{c1,c2}:`+行（单元格仅标量/Option）。
//! - parse_toon_closed：词法→schema 闭合→约束→重编码逐字节比对（非 canonical 即拒）。
//! - 帧格式 v2 见 frame 模块（P1b）；本模块只管文档体 T 的编解码。
//!
//! 依赖纪律：std only（无 serde/无新三方）。

use std::collections::BTreeMap;

/// codec 常量（规格 §1.3.1）：帧内 codec 字节。
pub const TOON_CODEC: u8 = 0x02;
/// profile 名（错误信息/测试锚用）。
pub const TOON_PROFILE: &str = "TOON-1.3-ductile-1";

/// TOON 值模型（序列化中立；JVal 的 TOON 对应物，含数组支撑）。
#[derive(Debug, Clone, PartialEq)]
pub enum TVal {
    Str(String),
    Num(u64),
    Bool(bool),
    Null,
    /// 动态键对象（TOON 文档根/嵌套；键按 UTF-8 字节序）
    Obj(BTreeMap<String, TVal>),
    /// 固定 schema 对象（保序字段）
    SchemaObj(Vec<(String, TVal)>),
    /// 标量数组
    Arr(Vec<TVal>),
    /// 同构对象数组（表格式；每行字段序=首行声明序）
    Table { cols: Vec<String>, rows: Vec<Vec<TVal>> },
}

// ============ 编码 ============

/// 判定字符串是否必须加引号（规格 §1.2.2）。
fn needs_quotes(s: &str) -> bool {
    if s.is_empty() || s != s.trim() {
        return true;
    }
    if s.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return true;
    }
    for ch in s.chars() {
        if matches!(ch, ',' | ':' | '[' | ']' | '{' | '}' | '"' | '#' | '\\') {
            return true;
        }
    }
    // 词法歧义：会解析为 u64/bool/null 的串必须加引号
    if s == "true" || s == "null" || s == "false" {
        return true;
    }
    if is_canonical_u64(s) {
        return true;
    }
    // 数字形开头（01/+1/-1/1.5 等）：解析端会拒裸形，编码端必须引号化保回环
    if s.chars().next().map(|c| c.is_ascii_digit() || c == '+' || c == '-').unwrap_or(false) {
        return true;
    }
    false
}

/// canonical u64 词法（§1.2.1）：`0|[1-9][0-9]*`。
pub fn is_canonical_u64(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes[0] == b'0' {
        return bytes.len() == 1;
    }
    bytes.iter().all(|b| b.is_ascii_digit())
}

/// 转义（§1.2.3）：固定小集合；其余 C0/0x7F 用小写四位 \u00xx。
fn escape_into(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

/// 编码标量为行内值（引号规则+转义）。
fn enc_scalar(v: &TVal, out: &mut String) {
    match v {
        TVal::Str(s) => {
            if needs_quotes(s) {
                out.push('"');
                escape_into(s, out);
                out.push('"');
            } else {
                out.push_str(s);
            }
        }
        TVal::Num(n) => out.push_str(&n.to_string()),
        TVal::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        TVal::Null => out.push_str("null"),
        // 表单元格/标量数组位置只允许标量——调用方保证；此处防御性引号包裹
        other => {
            out.push('"');
            if let TVal::Str(s) = other {
                escape_into(s, out);
            }
            out.push('"');
        }
    }
}

/// 键编码：安全标识符裸写，否则引号（§1.1.4）。
fn enc_key(k: &str, out: &mut String) {
    let safe = !k.is_empty()
        && k.chars().next().map(|c| c.is_ascii_alphanumeric() || c == '_').unwrap_or(false)
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if safe && !needs_quotes(k) {
        out.push_str(k);
    } else {
        out.push('"');
        escape_into(k, out);
        out.push('"');
    }
}

/// 核心编码器（内部，已带缩进）。
///
/// 返回行列表（不含末尾 LF；调用方统一补）。空对象=无子行。
fn encode_lines(v: &TVal, indent: usize, lines: &mut Vec<String>) {
    let pad = "  ".repeat(indent);
    let child_pad = "  ".repeat(indent + 1);
    match v {
        TVal::Obj(map) => {
            for (k, val) in map {
                encode_entry(k, val, &pad, &child_pad, lines);
            }
        }
        TVal::SchemaObj(fields) => {
            for (k, val) in fields {
                encode_entry(k, val, &pad, &child_pad, lines);
            }
        }
        _ => {} // 根=对象（规格 §1.1.2）；标量根由调用方拒绝
    }
}

fn encode_entry(k: &str, val: &TVal, pad: &str, child_pad: &str, lines: &mut Vec<String>) {
    let mut head = String::new();
    enc_key(k, &mut head);
    match val {
        TVal::Str(_) | TVal::Num(_) | TVal::Bool(_) | TVal::Null => {
            head.push_str(": ");
            enc_scalar(val, &mut head);
            lines.push(format!("{pad}{head}"));
        }
        TVal::Obj(m) => {
            if m.is_empty() {
                lines.push(format!("{pad}{head}: {{}}"));
            } else {
                lines.push(format!("{pad}{head}:"));
                let cp2 = format!("{child_pad}  ");
                for (k2, v2) in m {
                    encode_entry(k2, v2, child_pad, &cp2, lines);
                }
            }
        }
        TVal::SchemaObj(f) => {
            if f.is_empty() {
                lines.push(format!("{pad}{head}: {{}}"));
            } else {
                lines.push(format!("{pad}{head}:"));
                let cp2 = format!("{child_pad}  ");
                for (k2, v2) in f {
                    encode_entry(k2, v2, child_pad, &cp2, lines);
                }
            }
        }
        TVal::Arr(items) if items.iter().all(scalar_like) => {
            head.push_str(&format!("[{}]:", items.len()));
            if items.is_empty() {
                lines.push(format!("{pad}{head}"));
            } else {
                lines.push(format!("{pad}{head}"));
                let mut row = String::new();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        row.push(',');
                    }
                    enc_scalar(it, &mut row);
                }
                lines.push(format!("{child_pad}{row}"));
            }
        }
        TVal::Table { cols, rows } => {
            head.push_str(&format!("[{}]{{", rows.len()));
            for (i, c) in cols.iter().enumerate() {
                if i > 0 {
                    head.push(',');
                }
                enc_key(c, &mut head);
            }
            head.push_str("}:");
            lines.push(format!("{pad}{head}"));
            for r in rows {
                let mut row = String::new();
                for (i, cell) in r.iter().enumerate() {
                    if i > 0 {
                        row.push(',');
                    }
                    enc_scalar(cell, &mut row);
                }
                lines.push(format!("{child_pad}{row}"));
            }
        }
        // 异构数组：规格 §1.2.7 禁止——编码期拒绝（fail closed）
        TVal::Arr(_) => panic!("{TOON_PROFILE}: heterogeneous array rejected at encode"),
    }
}

fn scalar_like(v: &TVal) -> bool {
    matches!(v, TVal::Str(_) | TVal::Num(_) | TVal::Bool(_) | TVal::Null)
}

/// 唯一规范编码入口（§二.1）：成功输出以单个 LF 结尾的 canonical 字节。
pub fn toon_canonical(value: &TVal) -> Result<Vec<u8>, String> {
    // 根必须对象；空根对象=零字节（§1.1.5）
    let is_empty_root = match value {
        TVal::Obj(m) => m.is_empty(),
        TVal::SchemaObj(f) => f.is_empty(),
        _ => false,
    };
    if is_empty_root {
        return Ok(Vec::new());
    }
    let mut lines = Vec::new();
    match value {
        TVal::Obj(_) | TVal::SchemaObj(_) => encode_lines(value, 0, &mut lines),
        _ => return Err(format!("{TOON_PROFILE}: root must be object")),
    }
    let mut out = String::new();
    for l in &lines {
        // 行内防线：编码产物不得再含物理换行（引号内已转义）——防御性断言
        debug_assert!(!l.contains('\n') && !l.contains('\r'));
        out.push_str(l);
        out.push('\n');
    }
    Ok(out.into_bytes())
}

// ============ 解析 ============

struct Parser<'a> {
    lines: Vec<&'a str>,
    pos: usize,
}

/// 唯一闭合解析入口（§二.2）：词法→结构→（重编码由调用方或此处执行）。
/// 返回动态键对象（BTreeMap 保 UTF-8 序）。schema 闭合校验由调用方按 schema 执行。
pub fn parse_toon_closed(bytes: &[u8]) -> Result<TVal, String> {
    if bytes.is_empty() {
        return Ok(TVal::Obj(BTreeMap::new()));
    }
    // 字节层防线：UTF-8 / BOM / CR / 行尾空格（§二.5）
    let text = std::str::from_utf8(bytes).map_err(|e| format!("{TOON_PROFILE}: invalid UTF-8: {e}"))?;
    if text.starts_with('\u{FEFF}') {
        return Err(format!("{TOON_PROFILE}: BOM rejected"));
    }
    if bytes.contains(&0x0D) {
        return Err(format!("{TOON_PROFILE}: CR rejected"));
    }
    if !bytes.ends_with(b"\n") {
        return Err(format!("{TOON_PROFILE}: document must end with exactly one LF"));
    }
    // 恰一个末尾 LF：倒数第二字节不得再是 LF（防空行/多余 LF）
    if bytes.len() >= 2 && bytes[bytes.len() - 2] == 0x0A {
        return Err(format!("{TOON_PROFILE}: trailing blank line rejected"));
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    lines.pop(); // 末尾 LF 产生的空尾
    for (i, l) in lines.iter().enumerate() {
        if l.ends_with(' ') || l.ends_with('\t') {
            return Err(format!("{TOON_PROFILE}: trailing whitespace at line {}", i + 1));
        }
        if l.contains('\t') {
            return Err(format!("{TOON_PROFILE}: tab rejected at line {}", i + 1));
        }
    }
    let mut p = Parser { lines, pos: 0 };
    let root = p.parse_block(0)?;
    if p.pos != p.lines.len() {
        return Err(format!("{TOON_PROFILE}: trailing content at line {}", p.pos + 1));
    }
    Ok(root)
}

impl<'a> Parser<'a> {
    /// 解析当前缩进层的一个块（对象）。`indent`=期望缩进空格数。
    fn parse_block(&mut self, indent: usize) -> Result<TVal, String> {
        let mut map: BTreeMap<String, TVal> = BTreeMap::new();
        while self.pos < self.lines.len() {
            let line = self.lines[self.pos];
            if line.is_empty() {
                return Err(format!("{TOON_PROFILE}: blank line at {}", self.pos + 1));
            }
            let cur_indent = line.len() - line.trim_start_matches(' ').len();
            if cur_indent % 2 != 0 {
                return Err(format!("{TOON_PROFILE}: indent must be multiple of 2 at line {}", self.pos + 1));
            }
            if cur_indent < indent {
                break; // 归还给父层
            }
            if cur_indent > indent {
                return Err(format!("{TOON_PROFILE}: unexpected indent at line {}", self.pos + 1));
            }
            let body = &line[cur_indent..];
            // 表/数组头优先（key[N]: / key[N]{c}: 在首个 ':' 之前有 '['）
            let bracket_before_colon = body.find('[').map(|bi| body.find(':').map(|ci| bi < ci).unwrap_or(true)).unwrap_or(false);
            if bracket_before_colon {
                let (k2, v2) = self.parse_table_or_array(cur_indent)?;
                if map.insert(k2, v2).is_some() {
                    return Err(format!("{TOON_PROFILE}: duplicate key at line {}", self.pos));
                }
                continue;
            }
            self.pos += 1;
            let (key, rest) = parse_key(body, self.pos)?;
            if let Some(val) = rest.strip_prefix(": ") {
                // 标量或空对象 {}
                let v = if val == "{}" {
                    TVal::Obj(BTreeMap::new())
                } else {
                    parse_scalar(val, self.pos)?
                };
                if map.insert(key, v).is_some() {
                    return Err(format!("{TOON_PROFILE}: duplicate key at line {}", self.pos));
                }
            } else if let Some(tail) = rest.strip_prefix(':') {
                if !tail.is_empty() {
                    return Err(format!("{TOON_PROFILE}: junk after ':' at line {}", self.pos));
                }
                // 三种容器：子对象 / 标量数组 / 表数组——看下一行形态
                let child_indent = self.peek_indent()?;
                if child_indent.map(|c| c).unwrap_or(0) <= indent {
                    return Err(format!("{TOON_PROFILE}: key '{key}' has no value block at line {}", self.pos));
                }
                // 先看下一行是否为表头行——表头在本行：key[N]{c}: 已在 rest 中？
                // 编码格式中表头=本行 `key[N]{c}:`，rest 恰以 ':' 结尾且含 '['
                // 但我们已按首个 ':' 切分——故表/数组形式在 parse_key 前识别：
                // 此路径只到达普通对象（表头由 try_parse_table_or_array 提前截获）
                let child = self.parse_block(indent + 2)?;
                if map.insert(key, child).is_some() {
                    return Err(format!("{TOON_PROFILE}: duplicate key at line {}", self.pos));
                }
            } else {
                return Err(format!("{TOON_PROFILE}: expected ':' after key at line {}", self.pos));
            }
        }
        Ok(TVal::Obj(map))
    }

    fn peek_indent(&self) -> Result<Option<usize>, String> {
        if self.pos >= self.lines.len() {
            return Ok(None);
        }
        let l = self.lines[self.pos];
        if l.is_empty() {
            return Err(format!("{TOON_PROFILE}: blank line at {}", self.pos + 1));
        }
        Ok(Some(l.len() - l.trim_start_matches(' ').len()))
    }

    /// 解析 `key[N]:` 标量数组或 `key[N]{c}:` 表（§1.2.5/1.2.6）。
    fn parse_table_or_array(&mut self, indent: usize) -> Result<(String, TVal), String> {
        let line = self.lines[self.pos];
        let body = &line[indent..];
        self.pos += 1;
        let lb = body.find('[').ok_or(format!("{TOON_PROFILE}: '[' expected"))?;
        // 键=首个 '[' 前的原文（可能带引号形式）；裸键须过安全字符检查（与 parse_key 对称）
        let key_raw = &body[..lb];
        let key = if key_raw.starts_with('"') {
            parse_key(&format!("{key_raw}:"), self.pos)?.0
        } else if key_raw.is_empty()
            || key_raw.chars().any(|c| matches!(c, ',' | ']' | '{' | '}' | '"' | '#' | ' ' | ':'))
        {
            return Err(format!("{TOON_PROFILE}: bad array key at line {}", self.pos));
        } else {
            key_raw.to_string()
        };
        let rb = body.find(']').ok_or(format!("{TOON_PROFILE}: ']' expected"))?;
        let n_str = &body[lb + 1..rb];
        if !is_canonical_u64(n_str) {
            return Err(format!("{TOON_PROFILE}: array count must be canonical u64"));
        }
        let n: usize = n_str.parse().map_err(|_| format!("{TOON_PROFILE}: count overflow"))?;
        let after = &body[rb + 1..];
        if let Some(cols_str) = after.strip_prefix('{') {
            // 表：{c1,c2}:
            let close = cols_str.rfind('}').ok_or(format!("{TOON_PROFILE}: '}}' expected"))?;
            let cols_raw = &cols_str[..close];
            let tail = &cols_str[close + 1..];
            if tail != ":" {
                return Err(format!("{TOON_PROFILE}: table header must end with ':'"));
            }
            let mut cols = Vec::new();
            for c in split_top(cols_raw) {
                // 列名=纯键（无定界）——与数组键对称的裸键检查
                if c.is_empty() || c.chars().any(|ch| matches!(ch, ',' | ']' | '{' | '}' | '"' | '#' | ' ' | ':')) {
                    return Err(format!("{TOON_PROFILE}: bad table column name at line {}", self.pos));
                }
                cols.push(c);
            }
            // n 行，缩进 indent+2
            let mut rows = Vec::new();
            for _ in 0..n {
                if self.pos >= self.lines.len() {
                    return Err(format!("{TOON_PROFILE}: table truncated"));
                }
                let rl = self.lines[self.pos];
                let ri = rl.len() - rl.trim_start_matches(' ').len();
                if ri != indent + 2 {
                    return Err(format!("{TOON_PROFILE}: table row indent at line {}", self.pos + 1));
                }
                self.pos += 1;
                let cells_raw = &rl[ri..];
                let cells = split_top(cells_raw);
                if cells.len() != cols.len() {
                    return Err(format!("{TOON_PROFILE}: table row cell count mismatch"));
                }
                let mut row = Vec::new();
                for c in cells {
                    row.push(parse_scalar(&c, self.pos)?);
                }
                rows.push(row);
            }
            Ok((key, TVal::Table { cols, rows }))
        } else if after == ":" {
            // 标量数组：一行 N 个逗号分隔；N=0 无数据行（头行即全部）
            if n == 0 {
                return Ok((key, TVal::Arr(Vec::new())));
            }
            if self.pos >= self.lines.len() {
                return Err(format!("{TOON_PROFILE}: array truncated"));
            }
            let rl = self.lines[self.pos];
            let ri = rl.len() - rl.trim_start_matches(' ').len();
            if n > 0 && ri != indent + 2 {
                return Err(format!("{TOON_PROFILE}: array row indent at line {}", self.pos + 1));
            }
            if n == 0 {
                return Ok((key, TVal::Arr(Vec::new())));
            }
            self.pos += 1;
            let parts = split_top(&rl[ri..]);
            if parts.len() != n {
                return Err(format!("{TOON_PROFILE}: array count mismatch: header {n} vs {}", parts.len()));
            }
            let mut items = Vec::new();
            for p in parts {
                items.push(parse_scalar(&p, self.pos)?);
            }
            Ok((key, TVal::Arr(items)))
        } else {
            Err(format!("{TOON_PROFILE}: junk after array header"))
        }
    }
}

/// 解析行首键（裸标识符或引号形式）。返回 (key, rest_after_key)。
fn parse_key(body: &str, line_no: usize) -> Result<(String, &str), String> {
    if body.starts_with('"') {
        // 引号键：找闭引号（处理转义）
        let mut out = String::new();
        let mut esc = false;
        let mut end = None;
        for (i, ch) in body.char_indices().skip(1) {
            if esc {
                match ch {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => return Err(format!("{TOON_PROFILE}: \\u in keys unsupported at line {line_no}")),
                    c => return Err(format!("{TOON_PROFILE}: bad escape \\{c} in key at line {line_no}")),
                }
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                end = Some(i);
                break;
            } else {
                out.push(ch);
            }
        }
        let e = end.ok_or(format!("{TOON_PROFILE}: unterminated quoted key at line {line_no}"))?;
        Ok((out, &body[e + 1..]))
    } else {
        // 裸键：到首个 ':' 或 '[' 前
        let stop = body.find(|c| c == ':' || c == '[').ok_or(format!(
            "{TOON_PROFILE}: key without ':' at line {line_no}"
        ))?;
        let key = &body[..stop];
        if key.is_empty() {
            return Err(format!("{TOON_PROFILE}: empty key at line {line_no}"));
        }
        // 裸键内禁分隔符字符（编码端会引号化——解析端对称拒绝）
        if key.chars().any(|c| matches!(c, ',' | ']' | '{' | '}' | '"' | '#' | ' ')) {
            return Err(format!("{TOON_PROFILE}: unquoted key contains separator at line {line_no}"));
        }
        Ok((key.to_string(), &body[stop..]))
    }
}

/// 解析标量（引号串/u64/bool/null；引号内禁物理换行由行切分天然保证）。
fn parse_scalar(s: &str, line_no: usize) -> Result<TVal, String> {
    if let Some(stripped) = s.strip_prefix('"') {
        if !stripped.ends_with('"') || stripped.len() < 1 {
            return Err(format!("{TOON_PROFILE}: unterminated string at line {line_no}"));
        }
        let inner = &stripped[..stripped.len() - 1];
        let mut out = String::new();
        let mut esc = false;
        let mut chars = inner.chars().peekable();
        while let Some(ch) = chars.next() {
            if esc {
                match ch {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                        let cp = u32::from_str_radix(&hex, 16)
                            .map_err(|_| format!("{TOON_PROFILE}: bad \\u escape at line {line_no}"))?;
                        if hex.chars().any(|c| c.is_uppercase()) {
                            return Err(format!("{TOON_PROFILE}: uppercase hex rejected at line {line_no}"));
                        }
                        // 仅 C0/0x7F 允许 \u（§1.2.3：禁转义非 ASCII）
                        if cp >= 0x80 {
                            return Err(format!("{TOON_PROFILE}: \\u escape of non-ASCII rejected at line {line_no}"));
                        }
                        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                    }
                    c => return Err(format!("{TOON_PROFILE}: bad escape \\{c} at line {line_no}")),
                }
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                return Err(format!("{TOON_PROFILE}: unescaped quote in string at line {line_no}"));
            } else {
                out.push(ch);
            }
        }
        if esc {
            return Err(format!("{TOON_PROFILE}: dangling escape at line {line_no}"));
        }
        Ok(TVal::Str(out))
    } else if s == "null" {
        Ok(TVal::Null)
    } else if s == "true" {
        Ok(TVal::Bool(true))
    } else if s == "false" {
        Ok(TVal::Bool(false))
    } else if is_canonical_u64(s) {
        let n = s.parse::<u64>().map_err(|_| format!("{TOON_PROFILE}: u64 overflow at line {line_no}"))?;
        Ok(TVal::Num(n))
    } else {
        // 裸字符串：TOON 安全串（canonical 解码端对称规则——needs_quotes 的补集）
        // 含分隔符/控制字符/首尾空白/词法歧义形的裸串必须被拒（非 canonical）
        let s_t = s.trim();
        let digitish = s.chars().next().map(|c| c.is_ascii_digit() || c == '+' || c == '-').unwrap_or(false);
        if s.len() != s_t.len()
            || s.is_empty()
            || s == "true" || s == "false" || s == "null"
            || is_canonical_u64(s)
            || digitish  // 数字形开头（含非 canonical：01/+1/-1/1.5/1e3）→ 需引号，裸串拒
            || s.chars().any(|c| matches!(c, ',' | ':' | '[' | ']' | '{' | '}' | '"' | '#' | '\\') || (c as u32) < 0x20 || c as u32 == 0x7F)
        {
            return Err(format!("{TOON_PROFILE}: string requires quotes at line {line_no}"));
        }
        Ok(TVal::Str(s.to_string()))
    }
}

/// 顶层逗号切分（引号感知）。
fn split_top(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut esc = false;
    for ch in s.chars() {
        if in_q {
            cur.push(ch);
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_q = false;
            }
        } else if ch == '"' {
            in_q = true;
            cur.push(ch);
        } else if ch == ',' {
            out.push(cur.clone());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    out.push(cur);
    out
}

/// 回环校验（§二.2 末段）：parse→encode 必须与输入逐字节相等。
pub fn roundtrip_verify(bytes: &[u8]) -> Result<(), String> {
    let v = parse_toon_closed(bytes)?;
    let re = toon_canonical(&v)?;
    if re == bytes {
        Ok(())
    } else {
        Err(format!(
            "{TOON_PROFILE}: non-canonical input (re-encode differs); in={}B out={}B",
            bytes.len(),
            re.len()
        ))
    }
}

// ============ 测试（T01-T05 codec 层；T06/T07 帧层在 frame 模块） ============


// ============ 帧格式 v2（规格 §1.3，P1b） ============
//
// F1 = u64be(N1) || FT || J || LF          （v1 既有，不动）
// F2 = u64be(N2) || FT || 0x02 || T || LF  （v2 新增）
//
// N=len(body)，不含 8 字节长度前缀、FT、codec 字节、终止 LF。
// 首帧定链模式；v1/v2 不可混链。codec=0x02 唯一权威判别位（确认审#1）。

/// 编码 v2 帧：body=canonical TOON 字节（T，内部 LF 计入 N）。
pub fn encode_frame_v2(frame_type: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 1 + 1 + body.len() + 1);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes());
    out.push(frame_type);
    out.push(TOON_CODEC);
    out.extend_from_slice(body);
    out.push(b'\n');
    out
}

/// v2 帧判定：typed 帧头后紧跟 codec 字节 0x02（只查固定槽，禁扫 body——确认审#1）。
///
/// 头长依格式而异：v1=9 字节（8 len+FT，J[0] 恒 '{'=0x7B）；v2=10 字节
/// （8 len+FT+0x02）。codec 槽即 offset+9 处字节：对 v2 它是 0x02，对 v1 它是
/// body 首字节 '{'——两值无碰撞，唯一权威判别位。end 算式按格式分开。
pub fn frame_is_v2(data: &[u8], offset: usize) -> Result<bool, String> {
    if offset + 10 > data.len() {
        return Err(format!("{TOON_PROFILE}: truncated frame header at {offset}"));
    }
    let n64 = u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());
    let n = usize::try_from(n64).map_err(|_| format!("{TOON_PROFILE}: frame length overflow"))?;
    let codec = data[offset + 9];
    let (hdr, is_v2) = if codec == TOON_CODEC {
        (10usize, true)
    } else if codec == b'{' {
        (9usize, false)
    } else {
        // 既非 0x02 也非 '{'：非法帧（v1 J[0] 恒 '{'——fail closed）
        return Err(format!("{TOON_PROFILE}: bad codec slot {codec:#x} at {offset}"));
    };
    let end = offset
        .checked_add(hdr)
        .and_then(|v| v.checked_add(n))
        .and_then(|v| v.checked_add(1))
        .ok_or(format!("{TOON_PROFILE}: frame boundary overflow"))?;
    if data.len() < end {
        return Err(format!("{TOON_PROFILE}: truncated frame body"));
    }
    if data[end - 1] != b'\n' {
        return Err(format!("{TOON_PROFILE}: bad frame terminator"));
    }
    Ok(is_v2)
}

/// 解码 v2 帧：返回 ((FT, T 字节), 下一帧偏移)；T 由调用方过 parse_toon_closed。
pub fn decode_frame_v2(data: &[u8], offset: usize) -> Result<((u8, Vec<u8>), usize), String> {
    if offset + 10 > data.len() {
        return Err(format!("{TOON_PROFILE}: truncated v2 frame header"));
    }
    let ft = data[offset + 8];
    let codec = data[offset + 9];
    if codec != TOON_CODEC {
        return Err(format!("{TOON_PROFILE}: not a v2 frame (codec={codec:#x})"));
    }
    let n64 = u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());
    let n = usize::try_from(n64).map_err(|_| format!("{TOON_PROFILE}: frame length overflow"))?;
    let start = offset + 10;
    let end = start.checked_add(n).and_then(|v| v.checked_add(1)).ok_or(format!("{TOON_PROFILE}: frame boundary overflow"))?;
    if data.len() < end || data[end - 1] != b'\n' {
        return Err(format!("{TOON_PROFILE}: bad v2 frame boundary"));
    }
    Ok(((ft, data[start..start + n].to_vec()), end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(v: TVal) -> Vec<u8> {
        toon_canonical(&v).unwrap()
    }

    #[test]
    fn t01_scalar_roundtrip() {
        // u64/bool/null/字符串全类回环
        let mut m = BTreeMap::new();
        m.insert("a".into(), TVal::Num(42));
        m.insert("b".into(), TVal::Num(0));
        m.insert("c".into(), TVal::Num(u64::MAX));
        m.insert("d".into(), TVal::Bool(true));
        m.insert("e".into(), TVal::Bool(false));
        m.insert("f".into(), TVal::Null);
        m.insert("g".into(), TVal::Str("plain".into()));
        m.insert("h".into(), TVal::Str("with space".into()));
        m.insert("i".into(), TVal::Str("with:colon".into()));
        m.insert("j".into(), TVal::Str("with,comma".into()));
        m.insert("k".into(), TVal::Str("123".into())); // 词法歧义→引号
        m.insert("l".into(), TVal::Str("true".into()));
        m.insert("m".into(), TVal::Str("".into()));
        let doc = rt(TVal::Obj(m));
        roundtrip_verify(&doc).unwrap();
    }

    #[test]
    fn t01_nested_and_empty() {
        let mut inner = BTreeMap::new();
        inner.insert("x".into(), TVal::Num(1));
        let mut m = BTreeMap::new();
        m.insert("outer".into(), TVal::Obj(inner));
        m.insert("empty".into(), TVal::Obj(BTreeMap::new()));
        let doc = rt(TVal::Obj(m));
        let s = String::from_utf8(doc.clone()).unwrap();
        assert!(s.contains("outer:\n  x: 1\n"));
        assert!(s.contains("empty: {}\n"));
        roundtrip_verify(&doc).unwrap();
    }

    #[test]
    fn t01_arrays_and_table() {
        let mut m = BTreeMap::new();
        m.insert(
            "tags".into(),
            TVal::Arr(vec![TVal::Str("a".into()), TVal::Str("b".into()), TVal::Num(3)]),
        );
        m.insert("empty_arr".into(), TVal::Arr(vec![]));
        m.insert(
            "users".into(),
            TVal::Table {
                cols: vec!["id".into(), "name".into()],
                rows: vec![
                    vec![TVal::Num(1), TVal::Str("Alice".into())],
                    vec![TVal::Num(2), TVal::Str("Bob".into())],
                ],
            },
        );
        let doc = rt(TVal::Obj(m));
        let s = String::from_utf8(doc.clone()).unwrap();
        assert!(s.contains("tags[3]:\n  a,b,3\n"));
        assert!(s.contains("empty_arr[0]:\n"));
        assert!(s.contains("users[2]{id,name}:\n  1,Alice\n  2,Bob\n"));
        roundtrip_verify(&doc).unwrap();
    }

    #[test]
    fn t02_canonical_golden() {
        // 逐字节 golden：字段序=插入序（BTreeMap 字节序）
        let mut m = BTreeMap::new();
        m.insert("alpha".into(), TVal::Num(1));
        m.insert("beta".into(), TVal::Str("x".into()));
        let doc = rt(TVal::Obj(m));
        assert_eq!(doc, b"alpha: 1\nbeta: x\n");
        // 引号规则 golden
        let mut m2 = BTreeMap::new();
        m2.insert("k".into(), TVal::Str("sha256:abc".into())); // 含 ':'→引号
        let d2 = rt(TVal::Obj(m2));
        assert_eq!(d2, b"k: \"sha256:abc\"\n");
    }

    #[test]
    fn t03_byte_rejections() {
        for bad in [
            &b"key: value\r\n"[..],            // CR
            b"\xef\xbb\xbfk: 1\n".as_slice(),     // BOM (UTF-8 EF BB BF)
            &b"k: 1 "[..],                     // 行尾空格
            &b"k: 1\n\n"[..],                  // 尾空行
            &b"k:1\n"[..],                     // 缺空格（key: value 需空格）→ 裸键含':'错误路径
            &b"k: 1\n  j: 2\n"[..],            // 意外缩进
            &b"k: 1\nk: 2\n"[..],              // 重复键
            &b"k: 1\nunknown tail"[..],        // 尾随内容（无 LF）
        ] {
            assert!(parse_toon_closed(bad).is_err(), "must reject: {bad:?}");
        }
        // tab 拒绝
        assert!(parse_toon_closed(b"k:\t1\n").is_err());
    }

    #[test]
    fn t04_escape_rejections() {
        // 非 canonical 转义等价形式
        for bad in [
            &b"k: \"\\U0041\"\n"[..],   // 大写 hex
            &b"k: \"\\u4e2d\"\n"[..],   // 转义非 ASCII
            &b"k: \"a\nb\"\n"[..],      // 物理多行（引号跨行=行结构破坏）
        ] {
            assert!(parse_toon_closed(bad).is_err(), "must reject: {bad:?}");
        }
        // 无必要引号：plain 串加了引号→重编码不等→roundtrip 拒
        assert!(roundtrip_verify(b"k: \"plain\"\n").is_err());
        // 转义合法路径
        let mut m = BTreeMap::new();
        m.insert("k".into(), TVal::Str("a\"b\\c\nd".into()));
        let d = rt(TVal::Obj(m));
        assert_eq!(d, b"k: \"a\\\"b\\\\c\\nd\"\n");
        roundtrip_verify(&d).unwrap();
    }

    #[test]
    fn t05_numeric_and_option() {
        for bad in ["01", "+1", "-1", "1.5", "1e3", " 1"] {
            let doc = format!("k: {bad}\n");
            assert!(parse_toon_closed(doc.as_bytes()).is_err(), "must reject {bad:?}");
        }
        // u64 边界
        let mut m = BTreeMap::new();
        m.insert("max".into(), TVal::Num(u64::MAX));
        let d = rt(TVal::Obj(m));
        assert_eq!(d, format!("max: {}\n", u64::MAX).as_bytes());
        // 溢出
        let over = format!("k: {}\n", u128::from(u64::MAX) + 1);
        assert!(parse_toon_closed(over.as_bytes()).is_err());
        // 数组计数不符
        assert!(parse_toon_closed(b"k[2]:\n  a\n").is_err());
        // Some/None：值层=值或 null（语义层由 schema 管）
        assert!(parse_toon_closed(b"k: null\n").is_ok());
    }

    #[test]
    fn t05_table_constraints() {
        // 列数不符
        assert!(parse_toon_closed(b"t[1]{a,b}:\n  1\n").is_err());
        // 表截断
        assert!(parse_toon_closed(b"t[2]{a}:\n  1\n").is_err());
        // 表头非法计数
        assert!(parse_toon_closed(b"t[02]{a}:\n  1\n").is_err());
    }

    #[test]
    fn t05_empty_root() {
        assert_eq!(toon_canonical(&TVal::Obj(BTreeMap::new())).unwrap(), Vec::<u8>::new());
        assert!(matches!(parse_toon_closed(b""), Ok(TVal::Obj(m)) if m.is_empty()));
        // 标量根拒绝
        assert!(toon_canonical(&TVal::Num(1)).is_err());
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    #[test]
    fn t06_frame_v2_roundtrip() {
        let body = b"k: 1\nnested:\n  x: true\n";
        let f = encode_frame_v2(0x00, body);
        assert_eq!(&f[..8], &(body.len() as u64).to_be_bytes());
        assert_eq!(f[8], 0x00);
        assert_eq!(f[9], TOON_CODEC);
        assert_eq!(&f[10..10 + body.len()], body);
        assert_eq!(*f.last().unwrap(), b'\n');
        assert!(frame_is_v2(&f, 0).unwrap());
        let ((ft, t), end) = decode_frame_v2(&f, 0).unwrap();
        assert_eq!(ft, 0x00);
        assert_eq!(t, body);
        assert_eq!(end, f.len());
    }

    fn v1_bytes() -> Vec<u8> {
        let j = b"{\"k\":1}";
        let mut f = Vec::new();
        f.extend_from_slice(&(j.len() as u64).to_be_bytes());
        f.push(0x00);
        f.extend_from_slice(j);
        f.push(b'\n');
        f
    }

    #[test]
    fn t06_v1_vs_v2_discriminant() {
        // v1 帧：FT 后直接 '{'（0x7B）——与 0x02 无碰撞（确认审#1）
        let v1 = v1_bytes();
        assert!(!frame_is_v2(&v1, 0).unwrap());
        let v2 = encode_frame_v2(0x00, b"k: 1\n");
        assert!(frame_is_v2(&v2, 0).unwrap());
    }

    #[test]
    fn t06_frame_rejections() {
        let f = encode_frame_v2(0x00, b"k: 1\n");
        assert!(frame_is_v2(&f[..f.len() - 1], 0).is_err());
        assert!(frame_is_v2(&f[..7], 0).is_err());
        let mut bad = f.clone();
        *bad.last_mut().unwrap() = b'x';
        assert!(frame_is_v2(&bad, 0).is_err());
        let mut ov = Vec::new();
        ov.extend_from_slice(&u64::MAX.to_be_bytes());
        ov.push(0x00);
        ov.push(TOON_CODEC);
        ov.extend_from_slice(b"k: 1\n");
        assert!(frame_is_v2(&ov, 0).is_err());
        assert!(decode_frame_v2(&v1_bytes(), 0).is_err());
    }

    #[test]
    fn t07_internal_lf_counted_in_n() {
        let body = b"a: 1\nb:\n  c: 2\ntags[2]:\n  x,y\n";
        let f = encode_frame_v2(0x01, body);
        let n = u64::from_be_bytes(f[..8].try_into().unwrap()) as usize;
        assert_eq!(n, body.len());
        assert_eq!(&f[10..10 + n], body);
        let ((ft, t), _) = decode_frame_v2(&f, 0).unwrap();
        assert_eq!(ft, 0x01);
        assert_eq!(t, body);
        roundtrip_verify(&t).unwrap();
    }

    #[test]
    fn t07_multi_frame_stream() {
        let mut stream = Vec::new();
        for (ft, body) in [(0x00, &b"a: 1\n"[..]), (0x01, &b"trust_seq: \"1\"\n"[..]), (0x00, &b"b: 2\n"[..])] {
            stream.extend_from_slice(&encode_frame_v2(ft, body));
        }
        let mut off = 0usize;
        let mut count = 0;
        while off < stream.len() {
            assert!(frame_is_v2(&stream, off).unwrap(), "frame {count} must be v2");
            let (_, next) = decode_frame_v2(&stream, off).unwrap();
            off = next;
            count += 1;
        }
        assert_eq!(count, 3);
    }
}
