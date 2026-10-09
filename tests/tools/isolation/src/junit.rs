//! `--format junit` の出力（CI の集計用）。

use std::fmt::Write as _;

/// 1 テストの結果。
#[derive(Debug, Clone)]
pub(crate) enum CaseResult {
    Pass,
    /// 失敗の内容（差分など）。
    Fail(String),
    /// 飛ばした理由。
    Skip(String),
}

#[derive(Debug, Clone)]
pub(crate) struct Case {
    pub(crate) name: String,
    pub(crate) millis: u128,
    pub(crate) result: CaseResult,
}

fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            // XML 1.0 で使えない制御文字は捨てる。
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => o.push(c),
        }
    }
    o
}

/// `JUnit` XML を作る。
pub(crate) fn render(suite: &str, cases: &[Case]) -> String {
    let failures = cases
        .iter()
        .filter(|c| matches!(c.result, CaseResult::Fail(_)))
        .count();
    let skipped = cases
        .iter()
        .filter(|c| matches!(c.result, CaseResult::Skip(_)))
        .count();
    let total_ms: u128 = cases.iter().map(|c| c.millis).sum();
    let mut o = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        o,
        "<testsuite name=\"{}\" tests=\"{}\" failures=\"{failures}\" skipped=\"{skipped}\" time=\"{:.3}\">",
        escape(suite),
        cases.len(),
        millis_to_secs(total_ms),
    );
    for c in cases {
        let head = format!(
            "  <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\"",
            escape(suite),
            escape(&c.name),
            millis_to_secs(c.millis)
        );
        match &c.result {
            CaseResult::Pass => {
                let _ = writeln!(o, "{head}/>");
            }
            CaseResult::Fail(m) => {
                let _ = writeln!(
                    o,
                    "{head}>\n    <failure message=\"output differs\">{}</failure>\n  </testcase>",
                    escape(m)
                );
            }
            CaseResult::Skip(m) => {
                let _ = writeln!(
                    o,
                    "{head}>\n    <skipped message=\"{}\"/>\n  </testcase>",
                    escape(m)
                );
            }
        }
    }
    o.push_str("</testsuite>\n");
    o
}

#[allow(clippy::cast_precision_loss)]
fn millis_to_secs(ms: u128) -> f64 {
    ms as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_kinds_and_escapes() {
        let x = render(
            "isolation",
            &[
                Case {
                    name: "a".into(),
                    millis: 1500,
                    result: CaseResult::Pass,
                },
                Case {
                    name: "b<1>".into(),
                    millis: 10,
                    result: CaseResult::Fail("-x & +y\u{1}".into()),
                },
                Case {
                    name: "c".into(),
                    millis: 0,
                    result: CaseResult::Skip("KD-3".into()),
                },
            ],
        );
        assert!(x.contains("tests=\"3\" failures=\"1\" skipped=\"1\" time=\"1.510\""));
        assert!(x.contains("name=\"a\" time=\"1.500\"/>"));
        assert!(x.contains("name=\"b&lt;1&gt;\""));
        assert!(x.contains("-x &amp; +y</failure>"));
        assert!(x.contains("<skipped message=\"KD-3\"/>"));
    }
}
