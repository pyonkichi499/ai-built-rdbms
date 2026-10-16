//! `yuzhu_hba.conf`: parsing and matching (`m5/08` §3.4, §5.6, §6.2).
//!
//! A subset of PostgreSQL's `pg_hba.conf`: `host` lines with `trust`, `reject`,
//! `password` and `scram-sha-256` (`md5` is treated as `scram-sha-256`). There are no
//! Unix sockets, TLS or GSSAPI, so `local` / `hostssl` / `hostgssenc` lines never
//! match and are dropped with a warning.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Authentication method of a rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HbaMethod {
    Trust,
    Reject,
    Password,
    ScramSha256,
}

impl HbaMethod {
    /// The name used in the file (and in logs).
    pub fn as_str(self) -> &'static str {
        match self {
            HbaMethod::Trust => "trust",
            HbaMethod::Reject => "reject",
            HbaMethod::Password => "password",
            HbaMethod::ScramSha256 => "scram-sha-256",
        }
    }
}

/// A syntax error in `yuzhu_hba.conf`. `line` is 1-based (the first line of a
/// continued line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HbaError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for HbaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "yuzhu_hba.conf line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for HbaError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HbaDatabase {
    All,
    SameUser,
    SameRole,
    Replication,
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HbaUser {
    All,
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HbaAddress {
    All,
    Cidr { ip: IpAddr, prefix: u8 },
}

/// One usable line of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HbaRule {
    pub line: usize,
    pub databases: Vec<HbaDatabase>,
    pub users: Vec<HbaUser>,
    pub address: HbaAddress,
    pub method: HbaMethod,
}

/// The parsed file.
#[derive(Debug, Clone)]
pub struct HbaRules {
    rules: Vec<HbaRule>,
    warnings: Vec<String>,
}

/// One token; `quoted` tokens are never keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    text: String,
    quoted: bool,
}

/// A field: one or more comma-separated tokens.
type Field = Vec<Token>;

const NOT_SUPPORTED_METHODS: [&str; 9] = [
    "ident", "peer", "ldap", "radius", "cert", "pam", "bsd", "gss", "sspi",
];

fn err(line: usize, message: impl Into<String>) -> HbaError {
    HbaError {
        line,
        message: message.into(),
    }
}

/// Splits one (joined) logical line into fields.
fn tokenize(line_no: usize, text: &str) -> Result<Vec<Field>, HbaError> {
    let mut fields: Vec<Field> = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        match chars.peek() {
            None | Some('#') => break,
            _ => {}
        }
        let mut field: Field = Vec::new();
        loop {
            let mut tok = String::new();
            let mut quoted = false;
            let mut in_quote = false;
            while let Some(&c) = chars.peek() {
                if in_quote {
                    chars.next();
                    if c == '"' {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            tok.push('"');
                        } else {
                            in_quote = false;
                        }
                    } else {
                        tok.push(c);
                    }
                } else if c == '"' {
                    chars.next();
                    in_quote = true;
                    quoted = true;
                } else if c.is_whitespace() || c == ',' {
                    break;
                } else if c == '#' {
                    // A comment starts here (outside quotes), also inside a token.
                    while chars.next().is_some() {}
                    break;
                } else {
                    chars.next();
                    tok.push(c);
                }
            }
            if in_quote {
                return Err(err(line_no, "unterminated quoted string"));
            }
            if !tok.is_empty() || quoted {
                field.push(Token { text: tok, quoted });
            }
            // A comma (possibly after white space) continues the field.
            let mut look = chars.clone();
            while look.next_if(|c| c.is_whitespace()).is_some() {}
            if look.peek() == Some(&',') {
                look.next();
                while look.next_if(|c| c.is_whitespace()).is_some() {}
                chars = look;
                if chars.peek().is_none() {
                    break;
                }
                continue;
            }
            break;
        }
        if !field.is_empty() {
            fields.push(field);
        }
    }
    Ok(fields)
}

/// Joins `\`-continued lines. Returns `(first line number, text)` pairs.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let (n, mut buf) = current.take().unwrap_or((i + 1, String::new()));
        if let Some(stripped) = line.strip_suffix('\\') {
            buf.push_str(stripped);
            buf.push(' ');
            current = Some((n, buf));
        } else {
            buf.push_str(line);
            out.push((n, buf));
        }
    }
    if let Some(pending) = current {
        out.push(pending);
    }
    out
}

fn single(f: &Field) -> Option<&Token> {
    (f.len() == 1).then(|| &f[0])
}

fn parse_databases(line: usize, f: &Field) -> Result<Vec<HbaDatabase>, HbaError> {
    f.iter()
        .map(|t| {
            if !t.quoted {
                match t.text.as_str() {
                    "all" => return Ok(HbaDatabase::All),
                    "sameuser" => return Ok(HbaDatabase::SameUser),
                    "samerole" => return Ok(HbaDatabase::SameRole),
                    "replication" => return Ok(HbaDatabase::Replication),
                    _ => {}
                }
                check_name_prefix(line, &t.text, false)?;
            }
            Ok(HbaDatabase::Name(t.text.clone()))
        })
        .collect()
}

fn parse_users(line: usize, f: &Field) -> Result<Vec<HbaUser>, HbaError> {
    f.iter()
        .map(|t| {
            if !t.quoted {
                if t.text == "all" {
                    return Ok(HbaUser::All);
                }
                check_name_prefix(line, &t.text, true)?;
            }
            Ok(HbaUser::Name(t.text.clone()))
        })
        .collect()
}

fn check_name_prefix(line: usize, name: &str, is_user: bool) -> Result<(), HbaError> {
    if name.starts_with('@') {
        return Err(err(line, "file inclusion (@...) is not supported by yuzhu"));
    }
    if name.starts_with('/') {
        return Err(err(
            line,
            "regular expressions are not supported by yuzhu yet",
        ));
    }
    if is_user && name.starts_with('+') {
        return Err(err(
            line,
            "group names (+...) are not supported by yuzhu yet",
        ));
    }
    Ok(())
}

/// Characters an IP literal can consist of.
fn looks_like_ip(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':')
}

fn max_prefix(ip: IpAddr) -> u8 {
    if ip.is_ipv4() { 32 } else { 128 }
}

/// Number of leading ones of a netmask that is a run of ones followed by zeros.
fn mask_prefix(mask: IpAddr) -> Option<u8> {
    let (bits, width) = match mask {
        IpAddr::V4(m) => (u128::from(u32::from(m)), 32u32),
        IpAddr::V6(m) => (u128::from(m), 128u32),
    };
    let ones = (bits << (128 - width)).leading_ones();
    let rebuilt = if ones == 0 {
        0
    } else {
        u128::MAX << (128 - ones)
    };
    ((bits << (128 - width)) == rebuilt).then(|| u8::try_from(ones).unwrap_or(0))
}

/// Parses the address field (and the mask field of the `IP mask` form).
/// Returns the address and the number of fields consumed (1 or 2).
fn parse_address(
    line: usize,
    fields: &[Field],
    idx: usize,
) -> Result<(HbaAddress, usize), HbaError> {
    let Some(tok) = single(&fields[idx]) else {
        let shown = fields[idx]
            .iter()
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join(",");
        return Err(err(line, format!("invalid IP address \"{shown}\"")));
    };
    let s = tok.text.as_str();
    if !tok.quoted && s == "all" {
        return Ok((HbaAddress::All, 1));
    }
    if !tok.quoted && (s == "samehost" || s == "samenet") {
        return Err(host_names_error(line, s));
    }
    if let Some((ip_part, prefix_part)) = s.split_once('/') {
        let Ok(ip) = ip_part.parse::<IpAddr>() else {
            return Err(if looks_like_ip(ip_part) {
                err(line, format!("invalid IP address \"{ip_part}\""))
            } else {
                err(
                    line,
                    format!("specifying both host name and CIDR mask is invalid: \"{s}\""),
                )
            });
        };
        let prefix = prefix_part
            .parse::<u8>()
            .ok()
            .filter(|p| *p <= max_prefix(ip) && !prefix_part.starts_with('+'))
            .ok_or_else(|| err(line, format!("invalid CIDR mask in address \"{s}\"")))?;
        return Ok((HbaAddress::Cidr { ip, prefix }, 1));
    }
    let Ok(ip) = s.parse::<IpAddr>() else {
        return Err(if looks_like_ip(s) {
            err(line, format!("invalid IP address \"{s}\""))
        } else {
            host_names_error(line, s)
        });
    };
    // `IP mask` form.
    let Some(mask_field) = fields.get(idx + 1) else {
        return Err(err(line, "end-of-line before netmask specification"));
    };
    let mask_text = single(mask_field).map_or_else(
        || {
            mask_field
                .iter()
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>()
                .join(",")
        },
        |t| t.text.clone(),
    );
    let bad_mask = || err(line, format!("invalid IP mask \"{mask_text}\""));
    let mask = mask_text.parse::<IpAddr>().map_err(|_| bad_mask())?;
    if mask.is_ipv4() != ip.is_ipv4() {
        return Err(bad_mask());
    }
    let prefix = mask_prefix(mask).ok_or_else(bad_mask)?;
    Ok((HbaAddress::Cidr { ip, prefix }, 2))
}

fn host_names_error(line: usize, s: &str) -> HbaError {
    err(
        line,
        format!("host names and samehost / samenet are not supported by yuzhu: \"{s}\""),
    )
}

fn parse_method(line: usize, f: &Field) -> Result<HbaMethod, HbaError> {
    let shown = f
        .iter()
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let Some(tok) = single(f).filter(|t| !t.quoted) else {
        return Err(err(
            line,
            format!("invalid authentication method \"{shown}\""),
        ));
    };
    match tok.text.as_str() {
        "trust" => Ok(HbaMethod::Trust),
        "reject" => Ok(HbaMethod::Reject),
        "password" => Ok(HbaMethod::Password),
        "scram-sha-256" | "md5" => Ok(HbaMethod::ScramSha256),
        m if NOT_SUPPORTED_METHODS.contains(&m) => Err(err(
            line,
            format!("authentication method \"{m}\" is not supported by yuzhu"),
        )),
        m => Err(err(line, format!("invalid authentication method \"{m}\""))),
    }
}

impl HbaRules {
    /// Parses the text of `yuzhu_hba.conf`. Stops at the first error.
    pub fn parse(text: &str) -> Result<HbaRules, HbaError> {
        let mut rules = Vec::new();
        let mut warnings = Vec::new();
        for (line, content) in logical_lines(text) {
            let fields = tokenize(line, &content)?;
            if fields.is_empty() {
                continue;
            }
            let conn_type = match single(&fields[0]) {
                Some(t) if !t.quoted => t.text.as_str(),
                _ => {
                    let shown = fields[0]
                        .iter()
                        .map(|t| t.text.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    return Err(err(line, format!("invalid connection type \"{shown}\"")));
                }
            };
            let never_matches = match conn_type {
                "host" | "hostnossl" | "hostnogssenc" => false,
                "local" | "hostssl" | "hostgssenc" => true,
                other => {
                    return Err(err(line, format!("invalid connection type \"{other}\"")));
                }
            };
            if fields.len() < 2 {
                return Err(err(line, "end-of-line before database specification"));
            }
            let databases = parse_databases(line, &fields[1])?;
            if fields.len() < 3 {
                return Err(err(line, "end-of-line before role specification"));
            }
            let users = parse_users(line, &fields[2])?;
            // `local` lines have no address field (as in PostgreSQL).
            let (address, method_idx) = if conn_type == "local" {
                (HbaAddress::All, 3)
            } else {
                if fields.len() < 4 {
                    return Err(err(line, "end-of-line before IP address specification"));
                }
                let (a, used) = parse_address(line, &fields, 3)?;
                (a, 3 + used)
            };
            let Some(method_field) = fields.get(method_idx) else {
                return Err(err(line, "end-of-line before authentication method"));
            };
            let method = parse_method(line, method_field)?;
            if let Some(extra) = fields.get(method_idx + 1) {
                let shown = extra
                    .iter()
                    .map(|t| t.text.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                return Err(err(
                    line,
                    format!("authentication options are not supported by yuzhu: \"{shown}\""),
                ));
            }
            if never_matches {
                warnings.push(format!(
                    "line {line}: connection type \"{conn_type}\" never matches (yuzhu has no Unix sockets, TLS or GSSAPI); line ignored"
                ));
                continue;
            }
            if method == HbaMethod::Password {
                warnings.push(format!(
                    "line {line}: \"password\" sends the password in clear text (no TLS)"
                ));
            }
            rules.push(HbaRule {
                line,
                databases,
                users,
                address,
                method,
            });
        }
        if rules.is_empty() {
            warnings.push("no valid entries; every connection will be rejected".to_owned());
        }
        Ok(HbaRules { rules, warnings })
    }

    /// The first rule matching the connection. `database` and `user` are the names from the
    /// startup packet (`database` already defaults to `user`).
    pub fn find_rule(&self, addr: IpAddr, database: &str, user: &str) -> Option<&HbaRule> {
        let addr = addr.to_canonical();
        self.rules.iter().find(|rule| {
            address_matches(&rule.address, addr)
                && rule.databases.iter().any(|d| match d {
                    HbaDatabase::All => true,
                    HbaDatabase::SameUser | HbaDatabase::SameRole => database == user,
                    HbaDatabase::Replication => false,
                    HbaDatabase::Name(n) => n == database,
                })
                && rule.users.iter().any(|u| match u {
                    HbaUser::All => true,
                    HbaUser::Name(n) => n == user,
                })
        })
    }

    /// The method of the first matching rule.
    pub fn find(&self, addr: IpAddr, database: &str, user: &str) -> Option<HbaMethod> {
        self.find_rule(addr, database, user).map(|r| r.method)
    }

    /// Lines dropped, `password` rules, an empty rule set.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn rules(&self) -> &[HbaRule] {
        &self.rules
    }

    /// Every connection is allowed with `trust` (tests; reads no file).
    pub fn allow_all_trust() -> HbaRules {
        HbaRules {
            rules: vec![HbaRule {
                line: 0,
                databases: vec![HbaDatabase::All],
                users: vec![HbaUser::All],
                address: HbaAddress::All,
                method: HbaMethod::Trust,
            }],
            warnings: Vec::new(),
        }
    }
}

fn address_matches(rule: &HbaAddress, addr: IpAddr) -> bool {
    match rule {
        HbaAddress::All => true,
        HbaAddress::Cidr { ip, prefix } => match (ip, addr) {
            (IpAddr::V4(r), IpAddr::V4(a)) => v4_prefix_eq(*r, a, *prefix),
            (IpAddr::V6(r), IpAddr::V6(a)) => v6_prefix_eq(*r, a, *prefix),
            _ => false,
        },
    }
}

fn v4_prefix_eq(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    let mask = u32::MAX
        .checked_shl(32 - u32::from(prefix.min(32)))
        .unwrap_or(0);
    u32::from(a) & mask == u32::from(b) & mask
}

fn v6_prefix_eq(a: Ipv6Addr, b: Ipv6Addr, prefix: u8) -> bool {
    let mask = u128::MAX
        .checked_shl(128 - u32::from(prefix.min(128)))
        .unwrap_or(0);
    u128::from(a) & mask == u128::from(b) & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# TYPE  DATABASE  USER      ADDRESS               METHOD
host    all       all       127.0.0.1/32          trust
host    all       all       ::1/128               trust
host    app       appuser   10.0.0.0/8            scram-sha-256
host    all       all       192.168.0.0 255.255.0.0   password
host    all       all       all                   reject
";

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn e(text: &str) -> HbaError {
        HbaRules::parse(text).unwrap_err()
    }

    #[test]
    fn parses_the_sample() {
        let r = HbaRules::parse(SAMPLE).unwrap();
        assert_eq!(r.rules().len(), 5);
        assert_eq!(r.rules()[0].line, 2);
        assert_eq!(r.rules()[3].method, HbaMethod::Password);
        assert_eq!(
            r.rules()[3].address,
            HbaAddress::Cidr {
                ip: ip("192.168.0.0"),
                prefix: 16
            }
        );
        assert_eq!(r.rules()[4].address, HbaAddress::All);
        assert_eq!(r.warnings().len(), 1);
        assert!(r.warnings()[0].contains("clear text"));
    }

    #[test]
    fn comments_blank_lines_continuations_and_tabs() {
        let r = HbaRules::parse(
            "\n   # comment only\nhost\tall\tall\t127.0.0.1/32 \\\n   trust # tail\n\nhost all all ::1/128 reject",
        )
        .unwrap();
        assert_eq!(r.rules().len(), 2);
        assert_eq!(r.rules()[0].line, 3);
        assert_eq!(r.rules()[0].method, HbaMethod::Trust);
        assert_eq!(r.rules()[1].line, 6);
    }

    #[test]
    fn quoted_names_and_lists() {
        let r = HbaRules::parse(
            "host \"all\" \"all\" all trust\nhost a,b c, d all trust\nhost a ,b \"x y\" all trust\nhost \"a\"\"b\" u all trust",
        )
        .unwrap();
        assert_eq!(
            r.rules()[0].databases,
            vec![HbaDatabase::Name("all".into())]
        );
        assert_eq!(r.rules()[0].users, vec![HbaUser::Name("all".into())]);
        assert_eq!(r.rules()[1].databases.len(), 2);
        assert_eq!(
            r.rules()[1].users,
            vec![HbaUser::Name("c".into()), HbaUser::Name("d".into())]
        );
        assert_eq!(r.rules()[2].databases.len(), 2);
        assert_eq!(r.rules()[2].users, vec![HbaUser::Name("x y".into())]);
        assert_eq!(
            r.rules()[3].databases,
            vec![HbaDatabase::Name("a\"b".into())]
        );
    }

    #[test]
    fn unusable_connection_types_are_dropped_with_warnings() {
        let r = HbaRules::parse(
            "local all all trust\nhostssl all all all trust\nhostgssenc all all all trust\nhostnossl all all all trust\nhostnogssenc all all all trust",
        )
        .unwrap();
        assert_eq!(r.rules().len(), 2);
        assert_eq!(r.warnings().len(), 3);
        assert!(r.warnings()[0].contains("line 1"));
        assert!(r.warnings()[0].contains("never matches"));
    }

    #[test]
    fn md5_is_scram_and_ip_masks_and_ipv6() {
        let r = HbaRules::parse(
            "host all all 10.0.0.0 255.0.0.0 md5\nhost all all fe80::/10 scram-sha-256\nhost all all ::1 ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff trust\nhost all all 1.2.3.4 0.0.0.0 reject",
        )
        .unwrap();
        assert_eq!(r.rules()[0].method, HbaMethod::ScramSha256);
        assert_eq!(
            r.rules()[0].address,
            HbaAddress::Cidr {
                ip: ip("10.0.0.0"),
                prefix: 8
            }
        );
        assert_eq!(
            r.rules()[1].address,
            HbaAddress::Cidr {
                ip: ip("fe80::"),
                prefix: 10
            }
        );
        assert_eq!(
            r.rules()[2].address,
            HbaAddress::Cidr {
                ip: ip("::1"),
                prefix: 128
            }
        );
        assert_eq!(
            r.rules()[3].address,
            HbaAddress::Cidr {
                ip: ip("1.2.3.4"),
                prefix: 0
            }
        );
    }

    #[test]
    fn empty_file_warns() {
        let r = HbaRules::parse("# nothing\n").unwrap();
        assert!(r.rules().is_empty());
        assert!(r.warnings()[0].contains("no valid entries"));
        assert!(r.find(ip("127.0.0.1"), "d", "u").is_none());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // 期待するメッセージの一覧表。
    fn error_messages() {
        let cases: &[(&str, usize, &str)] = &[
            (
                "foo all all all trust",
                1,
                "invalid connection type \"foo\"",
            ),
            ("host", 1, "end-of-line before database specification"),
            ("host all", 1, "end-of-line before role specification"),
            (
                "host all all",
                1,
                "end-of-line before IP address specification",
            ),
            (
                "host all all all",
                1,
                "end-of-line before authentication method",
            ),
            (
                "host all all 10.0.0.0",
                1,
                "end-of-line before netmask specification",
            ),
            (
                "host all all all peer",
                1,
                "authentication method \"peer\" is not supported by yuzhu",
            ),
            (
                "host all all all bogus",
                1,
                "invalid authentication method \"bogus\"",
            ),
            (
                "host all all 999.1.1.1/8 trust",
                1,
                "invalid IP address \"999.1.1.1\"",
            ),
            (
                "host all all 10.0.0.0/33 trust",
                1,
                "invalid CIDR mask in address \"10.0.0.0/33\"",
            ),
            (
                "host all all ::/129 trust",
                1,
                "invalid CIDR mask in address \"::/129\"",
            ),
            (
                "host all all 10.0.0.0/x trust",
                1,
                "invalid CIDR mask in address \"10.0.0.0/x\"",
            ),
            (
                "host all all 10.0.0.0 255.0.255.0 trust",
                1,
                "invalid IP mask \"255.0.255.0\"",
            ),
            (
                "host all all 10.0.0.0 ffff:: trust",
                1,
                "invalid IP mask \"ffff::\"",
            ),
            (
                "host all all example.com/24 trust",
                1,
                "specifying both host name and CIDR mask is invalid: \"example.com/24\"",
            ),
            (
                "host all all example.com trust",
                1,
                "host names and samehost / samenet are not supported by yuzhu: \"example.com\"",
            ),
            (
                "host all all samehost trust",
                1,
                "host names and samehost / samenet are not supported by yuzhu: \"samehost\"",
            ),
            (
                "host all +grp all trust",
                1,
                "group names (+...) are not supported by yuzhu yet",
            ),
            (
                "host /re all all trust",
                1,
                "regular expressions are not supported by yuzhu yet",
            ),
            (
                "host all /re all trust",
                1,
                "regular expressions are not supported by yuzhu yet",
            ),
            (
                "host @file all all trust",
                1,
                "file inclusion (@...) is not supported by yuzhu",
            ),
            (
                "host all all all trust clientcert=1",
                1,
                "authentication options are not supported by yuzhu: \"clientcert=1\"",
            ),
            ("host \"all all all trust", 1, "unterminated quoted string"),
        ];
        for (text, line, msg) in cases {
            let got = e(text);
            assert_eq!((got.line, got.message.as_str()), (*line, *msg), "{text}");
        }
        assert_eq!(
            e("x").to_string(),
            "yuzhu_hba.conf line 1: invalid connection type \"x\""
        );
    }

    #[test]
    fn error_line_numbers_follow_continuations_and_first_error_wins() {
        let got = e("host all all all trust\nhost all \\\n  all \\\n  all bogus\nxxx\n");
        assert_eq!(got.line, 2);
        assert!(got.message.contains("bogus"));
    }

    #[test]
    fn first_match_wins() {
        let r = HbaRules::parse(
            "host all all 127.0.0.1/32 reject\nhost all all 127.0.0.1/32 trust\nhost all all all scram-sha-256",
        )
        .unwrap();
        let rule = r.find_rule(ip("127.0.0.1"), "d", "u").unwrap();
        assert_eq!((rule.line, rule.method), (1, HbaMethod::Reject));
        assert_eq!(
            r.find(ip("10.1.1.1"), "d", "u"),
            Some(HbaMethod::ScramSha256)
        );
    }

    #[test]
    fn address_matching() {
        let r = HbaRules::parse(
            "host all all 10.0.0.0/8 trust\nhost all all ::1/128 reject\nhost all all 0.0.0.0/0 password\n",
        )
        .unwrap();
        assert_eq!(r.find(ip("10.9.9.9"), "d", "u"), Some(HbaMethod::Trust));
        assert_eq!(r.find(ip("11.0.0.1"), "d", "u"), Some(HbaMethod::Password));
        // IPv6 never matches an IPv4 rule and vice versa.
        assert_eq!(r.find(ip("::1"), "d", "u"), Some(HbaMethod::Reject));
        assert_eq!(r.find(ip("::2"), "d", "u"), None);
        // IPv4-mapped IPv6 is an IPv4 address.
        assert_eq!(
            r.find(ip("::ffff:10.1.2.3"), "d", "u"),
            Some(HbaMethod::Trust)
        );
    }

    #[test]
    fn prefix_edges_and_host_bits() {
        let r = HbaRules::parse(
            "host all all 192.168.1.77/24 trust\nhost all all 1.2.3.4/32 reject\nhost all all 0.0.0.0/0 password",
        )
        .unwrap();
        // Host bits in the rule are ignored.
        assert_eq!(r.find(ip("192.168.1.1"), "d", "u"), Some(HbaMethod::Trust));
        assert_eq!(
            r.find(ip("192.168.2.1"), "d", "u"),
            Some(HbaMethod::Password)
        );
        // First match wins: the /32 reject rule precedes the catch-all.
        assert_eq!(r.find(ip("1.2.3.4"), "d", "u"), Some(HbaMethod::Reject));
        let r6 =
            HbaRules::parse("host all all ::/0 trust\nhost all all fe80::1/128 reject").unwrap();
        assert_eq!(r6.find(ip("fe80::1"), "d", "u"), Some(HbaMethod::Trust));
        let r6 = HbaRules::parse("host all all fe80::1/128 reject").unwrap();
        assert_eq!(r6.find(ip("fe80::1"), "d", "u"), Some(HbaMethod::Reject));
        assert_eq!(r6.find(ip("fe80::2"), "d", "u"), None);
    }

    #[test]
    fn database_and_user_matching() {
        let r = HbaRules::parse(
            "host sameuser all all reject\nhost replication all all reject\nhost app Bob all trust\nhost samerole all all password",
        )
        .unwrap();
        let a = ip("127.0.0.1");
        assert_eq!(r.find(a, "alice", "alice"), Some(HbaMethod::Reject));
        assert_eq!(r.find(a, "app", "Bob"), Some(HbaMethod::Trust));
        // Names are case sensitive.
        assert_eq!(r.find(a, "app", "bob"), None);
        assert_eq!(r.find(a, "App", "Bob"), None);
        // `replication` never matches.
        let only = HbaRules::parse("host replication all all trust").unwrap();
        assert_eq!(only.find(a, "replication", "u"), None);
        // Database defaults to the user name before the lookup.
        assert_eq!(r.find(a, "carol", "carol"), Some(HbaMethod::Reject));
    }

    #[test]
    fn allow_all_trust_matches_everything() {
        let r = HbaRules::allow_all_trust();
        assert_eq!(r.find(ip("203.0.113.9"), "x", "y"), Some(HbaMethod::Trust));
        assert_eq!(r.find(ip("::1"), "x", "y"), Some(HbaMethod::Trust));
    }
}
