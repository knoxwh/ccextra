// 流式工具参数修复:非标准 JSON 单引号 → RFC 8259 双引号
//
// 部分 OpenAI 兼容上游在 tool_calls 增量 streaming 中输出单引号字符串
// (非标准 JSON),客户端拿到非法 `partial_json` 解析失败。本函数在转发
// 前扫描,把单引号字符串转双引号并转义内部双引号,合法 JSON 零副作用。
// 对齐 CLIProxyAPI util.FixJSON 语义。
//
// 只修引号,不修其他非 JSON 特性(截断/不完整 JSON 仍靠调用方兜底)。

/// 单引号字符串 → 双引号字符串(RFC 8259 合法化)
///
/// 规则:
/// - 已有双引号字符串原样保留
/// - 单引号字符串转双引号,内部 `"` 转义为 `\"`
/// - 常见转义 `\n` `\r` `\t` `\b` `\f` `\\` 保留
/// - 单引号内 `\'` 变字面 `'`(双引号内无需转义)
/// - 单引号内 `\uXXXX` 透传(至多 4 个十六进制)
/// - 字符串外其它字符原样;输入在单引号内结束时补闭合 `"`
pub fn fix_json_quotes(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut in_double = false;
    let mut in_single = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        // 双引号字符串内:原样,仅追踪转义尾
        if in_double {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        // 单引号字符串内(正被转换):按转义规则改写
        if in_single {
            if c == '\\' {
                let Some(&next) = chars.get(i + 1) else {
                    // 孤尾反斜杠:原样保留
                    out.push('\\');
                    i += 1;
                    continue;
                };
                match next {
                    'n' | 'r' | 't' | 'b' | 'f' | '/' => {
                        out.push('\\');
                        out.push(next);
                        i += 2;
                    }
                    '"' => {
                        out.push('\\');
                        out.push('"');
                        i += 2;
                    }
                    '\\' => {
                        out.push('\\');
                        out.push('\\');
                        i += 2;
                    }
                    '\'' => {
                        // \' 在双引号内是字面 '
                        out.push('\'');
                        i += 2;
                    }
                    'u' => {
                        out.push('\\');
                        out.push('u');
                        i += 2;
                        // 透传至多 4 个十六进制
                        let mut hex = 0;
                        while hex < 4 && i < chars.len() && chars[i].is_ascii_hexdigit() {
                            out.push(chars[i]);
                            i += 1;
                            hex += 1;
                        }
                    }
                    _ => {
                        // 未知转义:原样保留反斜杠 + 字符
                        out.push('\\');
                        out.push(next);
                        i += 2;
                    }
                }
                continue;
            }
            if c == '\'' {
                // 单引号结束 → 双引号
                out.push('"');
                in_single = false;
                i += 1;
                continue;
            }
            // 单引号字符串内普通字符:内部双引号需转义
            if c == '"' {
                out.push('\\');
            }
            out.push(c);
            i += 1;
            continue;
        }

        // 字符串外
        if c == '"' {
            in_double = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\'' {
            in_single = true;
            out.push('"');
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }

    // 输入在单引号内结束:补闭合双引号
    if in_single {
        out.push('"');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::fix_json_quotes;

    #[test]
    fn test_fix_json_quotes_cases() {
        for (input, expected) in [
            ("{'a': 1, 'b': '2'}", r#"{"a": 1, "b": "2"}"#),
            (r#"{"t": 'He said "hi"'}"#, r#"{"t": "He said \"hi\""}"#),
            (
                r#"{"city": "beijing", "n": 42, "arr": [1, 2]}"#,
                r#"{"city": "beijing", "n": 42, "arr": [1, 2]}"#,
            ),
            (r#"{"t": "say \"hi\""}"#, r#"{"t": "say \"hi\""}"#),
            (
                r#"{'a': 'x\ny\tz', 'b': 'c\\d'}"#,
                r#"{"a": "x\ny\tz", "b": "c\\d"}"#,
            ),
            (r#"{'a': 'it\'s'}"#, r#"{"a": "it's"}"#),
            (r#"{'a': '中文'}"#, r#"{"a": "中文"}"#),
            (r#"{'a': '\u12z'}"#, r#"{"a": "\u12z"}"#),
            (r#"{"x": "a\\\"b"}"#, r#"{"x": "a\\\"b"}"#),
            (r#"["it's", 'say "hi"']"#, r#"["it's", "say \"hi\""]"#),
            ("{'a': 'abc", r#"{"a": "abc""#),
            ("", ""),
            ("plain text", "plain text"),
            ("{}", "{}"),
        ] {
            assert_eq!(fix_json_quotes(input), expected, "{input}");
        }
    }
}
