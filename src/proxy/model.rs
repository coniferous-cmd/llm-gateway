//! Byte-exact top-level JSON `model` inspection and replacement.
//!
//! Only the first top-level `model` key whose value is a string is considered.
//! Everything outside that one string literal is preserved byte for byte, so
//! whitespace, escaping, key order and number formats survive unchanged.

/// A decoded model value and the byte span of its JSON string literal
/// (including the surrounding quotes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSpan {
    pub value: String,
    pub start: usize,
    pub end: usize,
}

/// Locate the first top-level `model` string in a JSON object body.
pub fn model_value(body: &[u8]) -> Option<ModelSpan> {
    let mut scanner = Scanner { body, index: 0 };
    scanner.skip_whitespace();
    scanner.expect(b'{')?;
    scanner.skip_whitespace();
    if scanner.peek() == Some(b'}') {
        return None;
    }
    loop {
        let key = scanner.parse_string()?;
        scanner.skip_whitespace();
        scanner.expect(b':')?;
        scanner.skip_whitespace();
        if key.0 == "model" {
            if scanner.peek() != Some(b'"') {
                return None;
            }
            let (value, start, end) = scanner.parse_string_with_span()?;
            return Some(ModelSpan { value, start, end });
        }
        scanner.skip_value()?;
        scanner.skip_whitespace();
        match scanner.next()? {
            b',' => scanner.skip_whitespace(),
            b'}' => return None,
            _ => return None,
        }
    }
}

/// Replace a model literal with `name`, using Go's `encoding/json` escaping so
/// the result matches the reference implementation byte for byte.
pub fn splice_model_value(body: &[u8], span: &ModelSpan, name: &str) -> Vec<u8> {
    let quoted = go_json_quote(name);
    let mut spliced = Vec::with_capacity(body.len() - (span.end - span.start) + quoted.len());
    spliced.extend_from_slice(&body[..span.start]);
    spliced.extend_from_slice(quoted.as_bytes());
    spliced.extend_from_slice(&body[span.end..]);
    spliced
}

/// `encoding/json.Marshal` string escaping, including its HTML-safe defaults.
pub fn go_json_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct Scanner<'a> {
    body: &'a [u8],
    index: usize,
}

impl<'a> Scanner<'a> {
    fn peek(&self) -> Option<u8> {
        self.body.get(self.index).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.index += 1;
        Some(byte)
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        (self.next()? == byte).then_some(())
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.index += 1;
        }
    }

    fn parse_string(&mut self) -> Option<(String, usize, usize)> {
        self.parse_string_with_span()
    }

    fn parse_string_with_span(&mut self) -> Option<(String, usize, usize)> {
        let start = self.index;
        if self.next()? != b'"' {
            return None;
        }
        let mut escaped = false;
        loop {
            let byte = self.next()?;
            if escaped {
                escaped = false;
                continue;
            }
            match byte {
                b'\\' => escaped = true,
                b'"' => {
                    let end = self.index;
                    let decoded: String = serde_json::from_slice(&self.body[start..end]).ok()?;
                    return Some((decoded, start, end));
                }
                _ => {}
            }
        }
    }

    /// Validate and skip one JSON value. Nested values are parsed structurally
    /// so malformed input makes the whole body unresolvable, as in Go.
    fn skip_value(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => {
                self.parse_string()?;
                Some(())
            }
            b'{' => {
                self.index += 1;
                self.skip_whitespace();
                if self.peek() == Some(b'}') {
                    self.index += 1;
                    return Some(());
                }
                loop {
                    self.parse_string()?;
                    self.skip_whitespace();
                    self.expect(b':')?;
                    self.skip_whitespace();
                    self.skip_value()?;
                    self.skip_whitespace();
                    match self.next()? {
                        b',' => self.skip_whitespace(),
                        b'}' => return Some(()),
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.index += 1;
                self.skip_whitespace();
                if self.peek() == Some(b']') {
                    self.index += 1;
                    return Some(());
                }
                loop {
                    self.skip_value()?;
                    self.skip_whitespace();
                    match self.next()? {
                        b',' => self.skip_whitespace(),
                        b']' => return Some(()),
                        _ => return None,
                    }
                }
            }
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            b'-' | b'0'..=b'9' => {
                self.index += 1;
                while matches!(
                    self.peek(),
                    Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                ) {
                    self.index += 1;
                }
                Some(())
            }
            _ => None,
        }
    }

    fn literal(&mut self, literal: &[u8]) -> Option<()> {
        if self.body.get(self.index..self.index + literal.len())? == literal {
            self.index += literal.len();
            Some(())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn model_value_and_splice_table() {
        let cases = [
            (
                r#"{"model":"alias","n":1}"#,
                Some("alias"),
                r#"{"model":"real","n":1}"#,
            ),
            (
                "{\n  \"n\" : 1 ,\n  \"model\" : \"alias\"\n}",
                Some("alias"),
                "{\n  \"n\" : 1 ,\n  \"model\" : \"real\"\n}",
            ),
            (
                r#"{"model":"first","model":"second"}"#,
                Some("first"),
                r#"{"model":"real","model":"second"}"#,
            ),
            (
                r#"{"nested":{"model":"inner"},"model":"outer"}"#,
                Some("outer"),
                r#"{"nested":{"model":"inner"},"model":"real"}"#,
            ),
            (r#"{"nested":{"model":"inner"}}"#, None, ""),
            (r#"{"model":123}"#, None, ""),
            (r#"{"model":null}"#, None, ""),
            (r#"{"model":{"id":1}}"#, None, ""),
            (r#"[{"model":"alias"}]"#, None, ""),
            (r#""model""#, None, ""),
            (r#"{"model":"#, None, ""),
            ("", None, ""),
            ("not JSON at all", None, ""),
            ("\u{0}raw\n", None, ""),
            (r#"{"model":"alias"}"#, Some("alias"), r#"{"model":"real"}"#),
            (r#"{"model":"alias"}"#, Some("alias"), r#"{"model":"real"}"#),
            (
                r#"{"model" : "a\"b"}"#,
                Some("a\"b"),
                r#"{"model" : "real"}"#,
            ),
        ];
        for (body, want, spliced) in cases {
            let found = model_value(body.as_bytes());
            match want {
                Some(value) => {
                    let span = found.expect(body);
                    assert_eq!(span.value, value, "{body}");
                    assert_eq!(
                        String::from_utf8(splice_model_value(body.as_bytes(), &span, "real"))
                            .unwrap(),
                        spliced,
                        "{body}"
                    );
                }
                None => assert!(found.is_none(), "{body:?}"),
            }
        }
    }

    #[test]
    fn splice_preserves_surrounding_bytes() {
        let body =
            "{\n\t\"keep\" : \"ünïcodé ✓\",\n  \"model\"   :   \"alias\" ,\n  \"n\": 2e10\n}";
        let span = model_value(body.as_bytes()).unwrap();
        let want =
            "{\n\t\"keep\" : \"ünïcodé ✓\",\n  \"model\"   :   \"模型-真实\" ,\n  \"n\": 2e10\n}";
        assert_eq!(
            String::from_utf8(splice_model_value(body.as_bytes(), &span, "模型-真实")).unwrap(),
            want
        );
    }

    #[test]
    fn go_json_quote_matches_html_escaping() {
        assert_eq!(go_json_quote("a<b>&c"), r#""a\u003cb\u003e\u0026c""#);
        assert_eq!(go_json_quote("line\n\t\""), r#""line\n\t\"""#);
        assert_eq!(go_json_quote("\u{2028}\u{2029}"), r#""\u2028\u2029""#);
        assert_eq!(go_json_quote("模型"), "\"模型\"");
    }

    #[test]
    fn resolved_span_decodes_to_the_value() {
        for body in [
            r#"{"model":"x"}"#,
            r#"{"model":"a b"}"#,
            r#"{"n":1,"model":"é"}"#,
            r#"{"n":[],"model":"x"}"#,
        ] {
            let span = model_value(body.as_bytes()).unwrap();
            let literal = &body.as_bytes()[span.start..span.end];
            let decoded: Value = serde_json::from_slice(literal).unwrap();
            assert_eq!(decoded.as_str(), Some(span.value.as_str()));
        }
    }
}
