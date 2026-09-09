# hyper semantics fixtures

对照夹具，供 `examples/scripts/hyper_semantics_drill.sh` 使用。语义见 SPEC §2.4b / DESIGN §4.0。

| 文件 | 覆盖 |
|------|------|
| `gate_clean.hyper` | 显式 gate 端口投影 |
| `gate_bad_positional.hyper` | 拒绝裸成员 gate（预期解析失败） |
| `xor_alts.hyper` | xor 只投影 slot |
| `bundle_ok.hyper` / `bundle_missing.pipeline` | bundle 共现 check |
| `iso_a.hyper` / `iso_b.hyper` | 改名同构（`hypergraph_key`） |
| `legacy_gate.hyper` | `.stage` 降糖与显式 key 对齐 |
