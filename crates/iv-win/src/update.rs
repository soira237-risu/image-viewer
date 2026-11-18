//! Checking for a newer release, without the network part: where to ask (derived from the
//! repository URL in Cargo.toml), reading GitHub's answer, comparing versions, and wording the
//! result. `win::update_check` makes the one HTTPS request, and only when the user asks.

use std::cmp::Ordering;

use crate::i18n::{Strings, fill};

/// The project's home, from `repository` in the workspace's Cargo.toml: every URL the program
/// uses is derived from it.
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
/// This program's version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// The largest answer read; GitHub's is a few kilobytes.
pub const MAX_ANSWER: usize = 1 << 20;

/// The list of releases.
pub fn releases_page(repo: &str) -> String {
    format!("{}/releases", repo.trim_end_matches('/'))
}

/// The host and path of the API request for the latest release of a `https://github.com/<owner>/<repo>`
/// repository; `None` for anything else.
pub fn latest_release_api(repo: &str) -> Option<(&'static str, String)> {
    let rest = repo.strip_prefix("https://github.com/")?.trim_end_matches('/');
    let (owner, name) = rest.split_once('/')?;
    let part = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    (part(owner) && part(name) && name != "." && name != "..")
        .then(|| ("api.github.com", format!("/repos/{owner}/{name}/releases/latest")))
}

/// The page to open for a release: GitHub's `html_url` when it is a release page of this
/// repository (and holds nothing but plain URL characters), otherwise the list of releases.
pub fn release_page(html_url: Option<&str>, repo: &str) -> String {
    let list = releases_page(repo);
    let prefix = format!("{list}/");
    match html_url.and_then(|u| u.strip_prefix(&prefix).map(|rest| (u, rest))) {
        Some((url, rest))
            if !rest.is_empty()
                && !rest.contains("..")
                && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~/%+".contains(&b)) =>
        {
            url.to_string()
        }
        _ => list,
    }
}

/// One dot-separated part of a pre-release (`beta`, `2`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Text(String),
}

/// A version tag as releases are named: an optional `v`, then `major.minor.patch` (a missing
/// minor or patch counts as 0), an optional `-pre.release`, an optional `+build` (ignored).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<Ident>,
}

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let t = text.trim();
        let t = t.strip_prefix(['v', 'V']).unwrap_or(t);
        let t = t.split_once('+').map_or(t, |(v, build)| if build.is_empty() { "" } else { v });
        let (core_text, pre_text) = match t.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (t, None),
        };
        let number = |s: &str| -> Option<u64> {
            if s.is_empty() || s.len() > 19 || !s.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            s.parse().ok()
        };
        let parts: Vec<&str> = core_text.split('.').collect();
        if parts.len() > 3 {
            return None;
        }
        let mut core = [0u64; 3];
        for (slot, part) in core.iter_mut().zip(&parts) {
            *slot = number(part)?;
        }
        let pre = match pre_text {
            None => Vec::new(),
            Some(p) => p
                .split('.')
                .map(|id| {
                    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                        None
                    } else if id.bytes().all(|b| b.is_ascii_digit()) {
                        number(id).map(Ident::Num)
                    } else {
                        Some(Ident::Text(id.to_string()))
                    }
                })
                .collect::<Option<Vec<_>>>()?,
        };
        Some(Version { core, pre })
    }
}

impl Ord for Version {
    /// Semantic-versioning order: the numbers, then a pre-release below its release, then the
    /// pre-release parts one by one (numbers below words, numbers by value, words by ASCII).
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                for (a, b) in self.pre.iter().zip(&other.pre) {
                    let o = match (a, b) {
                        (Ident::Num(x), Ident::Num(y)) => x.cmp(y),
                        (Ident::Num(_), Ident::Text(_)) => Ordering::Less,
                        (Ident::Text(_), Ident::Num(_)) => Ordering::Greater,
                        (Ident::Text(x), Ident::Text(y)) => x.cmp(y),
                    };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                self.pre.len().cmp(&other.pre.len())
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Why a check gave no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// No connection: the name did not resolve or the server could not be reached.
    Offline,
    Timeout,
    /// The secure connection could not be made (certificate, protocol).
    Tls,
    /// Another network error (a WinHTTP error code).
    Network(u32),
    /// 403 or 429: GitHub limits unauthenticated requests per address.
    RateLimited(u16),
    Http(u16),
    TooLarge,
    /// Not the JSON of a release, or no version in it.
    Unreadable,
}

/// What a check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// This version is the latest release (or newer); `latest` is the release's tag.
    UpToDate { latest: String },
    /// A newer release, and the page to download it from.
    Newer { latest: String, page: String },
}

/// The check as the settings window shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Idle,
    Checking,
    Done(Result<Outcome, Failure>),
}

/// The answer to the request for the latest release: its HTTP status and body.
pub fn evaluate(status: u16, body: &[u8], current: &str, repo: &str) -> Result<Outcome, Failure> {
    match status {
        200 => {}
        403 | 429 => return Err(Failure::RateLimited(status)),
        _ => return Err(Failure::Http(status)),
    }
    if body.len() > MAX_ANSWER {
        return Err(Failure::TooLarge);
    }
    let fields = json::release_fields(body).ok_or(Failure::Unreadable)?;
    let tag = fields.tag_name.ok_or(Failure::Unreadable)?;
    let latest = Version::parse(&tag).ok_or(Failure::Unreadable)?;
    let current = Version::parse(current).ok_or(Failure::Unreadable)?;
    if latest > current {
        Ok(Outcome::Newer { latest: tag, page: release_page(fields.html_url.as_deref(), repo) })
    } else {
        Ok(Outcome::UpToDate { latest: tag })
    }
}

/// Why a check failed, in `s`'s language.
pub fn reason(f: &Failure, s: &Strings) -> String {
    match f {
        Failure::Offline => s.update_offline.to_string(),
        Failure::Timeout => s.update_timeout.to_string(),
        Failure::Tls => s.update_tls.to_string(),
        Failure::Network(code) => fill(s.update_network, &[("code", &code.to_string())]),
        Failure::RateLimited(status) => fill(s.update_rate_limited, &[("status", &status.to_string())]),
        Failure::Http(status) => fill(s.update_http, &[("status", &status.to_string())]),
        Failure::TooLarge => s.update_too_large.to_string(),
        Failure::Unreadable => s.update_unreadable.to_string(),
    }
}

/// The status line under the button (empty before the first check).
pub fn status_text(state: &State, s: &Strings) -> String {
    match state {
        State::Idle => String::new(),
        State::Checking => s.update_checking.to_string(),
        State::Done(Ok(Outcome::UpToDate { .. })) => fill(s.update_current, &[("version", CURRENT)]),
        State::Done(Ok(Outcome::Newer { latest, .. })) => {
            fill(s.update_available, &[("latest", latest), ("version", CURRENT)])
        }
        State::Done(Err(f)) => fill(s.update_failed, &[("reason", &reason(f, s))]),
    }
}

/// Just enough JSON to read two string fields of the top-level object, strictly: anything that is
/// not one well-formed JSON object (or nests deeper than [`json::MAX_DEPTH`]) is refused.
mod json {
    pub const MAX_DEPTH: usize = 64;

    #[derive(Default, Debug)]
    pub struct ReleaseFields {
        pub tag_name: Option<String>,
        pub html_url: Option<String>,
    }

    /// `tag_name` and `html_url` of a release object. `None` if the text is not valid JSON, the
    /// top level is not an object, either field appears twice, or `tag_name` is not a string.
    pub fn release_fields(body: &[u8]) -> Option<ReleaseFields> {
        let text = std::str::from_utf8(body).ok()?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut p = Parser { b: text.as_bytes(), i: 0 };
        let mut out = ReleaseFields::default();
        p.ws();
        p.expect(b'{')?;
        p.ws();
        if p.eat(b'}') {
            p.ws();
            return (p.i == p.b.len()).then_some(out);
        }
        loop {
            p.ws();
            let key = p.string()?;
            p.ws();
            p.expect(b':')?;
            p.ws();
            let slot = match key.as_str() {
                "tag_name" => Some(&mut out.tag_name),
                "html_url" => Some(&mut out.html_url),
                _ => None,
            };
            match slot {
                Some(slot) => {
                    if slot.is_some() {
                        return None;
                    }
                    if p.peek() == Some(b'"') {
                        *slot = Some(p.string()?);
                    } else if key == "tag_name" {
                        return None;
                    } else {
                        p.value(1)?;
                    }
                }
                None => p.value(1)?,
            }
            p.ws();
            if p.eat(b',') {
                continue;
            }
            p.expect(b'}')?;
            break;
        }
        p.ws();
        (p.i == p.b.len()).then_some(out)
    }

    struct Parser<'a> {
        b: &'a [u8],
        i: usize,
    }

    impl Parser<'_> {
        fn peek(&self) -> Option<u8> {
            self.b.get(self.i).copied()
        }

        fn eat(&mut self, c: u8) -> bool {
            let hit = self.peek() == Some(c);
            if hit {
                self.i += 1;
            }
            hit
        }

        fn expect(&mut self, c: u8) -> Option<()> {
            self.eat(c).then_some(())
        }

        fn ws(&mut self) {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                self.i += 1;
            }
        }

        fn literal(&mut self, word: &[u8]) -> Option<()> {
            let end = self.i.checked_add(word.len())?;
            (self.b.get(self.i..end)? == word).then(|| self.i = end)
        }

        fn digits(&mut self) -> usize {
            let start = self.i;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
            self.i - start
        }

        fn number(&mut self) -> Option<()> {
            self.eat(b'-');
            if self.eat(b'0') {
                // no leading zeros
            } else if self.digits() == 0 {
                return None;
            }
            if self.eat(b'.') && self.digits() == 0 {
                return None;
            }
            if self.eat(b'e') || self.eat(b'E') {
                if !self.eat(b'+') {
                    self.eat(b'-');
                }
                if self.digits() == 0 {
                    return None;
                }
            }
            Some(())
        }

        fn hex4(&mut self) -> Option<u32> {
            let end = self.i.checked_add(4)?;
            let s = std::str::from_utf8(self.b.get(self.i..end)?).ok()?;
            if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            self.i = end;
            u32::from_str_radix(s, 16).ok()
        }

        /// A string, its escapes decoded. The input is valid UTF-8, and quotes and backslashes
        /// are ASCII, so the runs between them are whole characters.
        fn string(&mut self) -> Option<String> {
            self.expect(b'"')?;
            let mut out = String::new();
            let mut run = self.i;
            loop {
                let c = self.peek()?;
                match c {
                    b'"' => {
                        out.push_str(std::str::from_utf8(&self.b[run..self.i]).ok()?);
                        self.i += 1;
                        return Some(out);
                    }
                    b'\\' => {
                        out.push_str(std::str::from_utf8(&self.b[run..self.i]).ok()?);
                        self.i += 1;
                        let e = self.peek()?;
                        self.i += 1;
                        let ch = match e {
                            b'"' => '"',
                            b'\\' => '\\',
                            b'/' => '/',
                            b'b' => '\u{8}',
                            b'f' => '\u{c}',
                            b'n' => '\n',
                            b'r' => '\r',
                            b't' => '\t',
                            b'u' => {
                                let hi = self.hex4()?;
                                let code = if (0xd800..0xdc00).contains(&hi) {
                                    // a high surrogate must be followed by an escaped low one
                                    self.literal(b"\\u")?;
                                    let lo = self.hex4()?;
                                    if !(0xdc00..0xe000).contains(&lo) {
                                        return None;
                                    }
                                    0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                                } else {
                                    hi
                                };
                                // a lone low surrogate is refused here
                                char::from_u32(code)?
                            }
                            _ => return None,
                        };
                        out.push(ch);
                        run = self.i;
                    }
                    0..=0x1f => return None,
                    _ => self.i += 1,
                }
            }
        }

        /// Skips one value (checking it), at nesting `depth`.
        fn value(&mut self, depth: usize) -> Option<()> {
            if depth > MAX_DEPTH {
                return None;
            }
            match self.peek()? {
                b'"' => self.string().map(drop),
                b't' => self.literal(b"true"),
                b'f' => self.literal(b"false"),
                b'n' => self.literal(b"null"),
                b'-' | b'0'..=b'9' => self.number(),
                open @ (b'[' | b'{') => {
                    let close = if open == b'[' { b']' } else { b'}' };
                    self.i += 1;
                    self.ws();
                    if self.eat(close) {
                        return Some(());
                    }
                    loop {
                        self.ws();
                        if open == b'{' {
                            self.string()?;
                            self.ws();
                            self.expect(b':')?;
                            self.ws();
                        }
                        self.value(depth + 1)?;
                        self.ws();
                        if self.eat(b',') {
                            continue;
                        }
                        return self.expect(close);
                    }
                }
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{EN, KO};

    const REPO: &str = "https://github.com/owner/repo";

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} does not parse"))
    }

    #[test]
    fn repository_url_is_this_projects() {
        assert_eq!(REPOSITORY, "https://github.com/soira237-risu/image-viewer");
        assert_eq!(
            latest_release_api(REPOSITORY),
            Some(("api.github.com", "/repos/soira237-risu/image-viewer/releases/latest".to_string()))
        );
        assert_eq!(releases_page(REPOSITORY), "https://github.com/soira237-risu/image-viewer/releases");
    }

    #[test]
    fn only_github_repositories_have_an_api_address() {
        assert_eq!(
            latest_release_api("https://github.com/a/b/"),
            Some(("api.github.com", "/repos/a/b/releases/latest".into()))
        );
        for bad in [
            "http://github.com/a/b",
            "https://gitlab.com/a/b",
            "https://github.com/a",
            "https://github.com/a/b/c",
            "https://github.com/a/..",
            "https://github.com/a/b?x=1",
            "https://github.com/a b/c",
            "https://github.com//b",
        ] {
            assert_eq!(latest_release_api(bad), None, "{bad}");
        }
    }

    #[test]
    fn versions_compare_like_semantic_versions() {
        assert_eq!(v("v0.1.0"), v("0.1.0"));
        assert_eq!(v("V1.2"), v("1.2.0"));
        assert_eq!(v("1"), v("1.0.0"));
        assert_eq!(v("1.0.0+build.5"), v("1.0.0"));
        assert!(v("0.1.1") > v("0.1.0"));
        assert!(v("0.2.0") > v("0.1.9"));
        assert!(v("0.10.0") > v("0.9.0"), "numbers, not text");
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("1.0.0-rc.1") < v("1.0.0"), "a pre-release sorts below its release");
        assert!(v("1.0.0-rc.1") > v("0.9.0"));
        // the order from semver.org
        let chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for pair in chain.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v(" v2.0.0 ").cmp(&v("2.0.0")), Ordering::Equal);
    }

    #[test]
    fn odd_version_tags_are_refused() {
        for bad in [
            "",
            "v",
            "latest",
            "1.2.3.4",
            "1..2",
            ".1",
            "1.",
            "-1.0.0",
            "1.0.0-",
            "1.0.0-a..b",
            "1.0.0-a_b",
            "1.0.0+",
            "1.a.0",
            "99999999999999999999.0.0",
            "v 1.0",
            "１.０.０",
            "1.0.0-é",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn release_pages_must_belong_to_the_repository() {
        let ok = "https://github.com/owner/repo/releases/tag/v0.2.0";
        assert_eq!(release_page(Some(ok), REPO), ok);
        let list = "https://github.com/owner/repo/releases";
        for bad in [
            None,
            Some("https://evil.example/owner/repo/releases/tag/v1"),
            Some("https://github.com/owner/repo-evil/releases/tag/v1"),
            Some("https://github.com/owner/repo/releases/"),
            Some("https://github.com/owner/repo/releases/../../../other"),
            Some("https://github.com/owner/repo/releases/tag/v1\" calc"),
            Some("https://github.com/owner/repo/releases/tag/v1 &x"),
            Some("https://github.com/owner/repo/releasesx/tag/v1"),
            Some("http://github.com/owner/repo/releases/tag/v1"),
            Some("file:///C:/Windows/notepad.exe"),
        ] {
            assert_eq!(release_page(bad, REPO), list, "{bad:?}");
        }
    }

    fn body(tag: &str) -> Vec<u8> {
        format!(
            r#"{{"url":"x","html_url":"https://github.com/owner/repo/releases/tag/{tag}","id":1,"tag_name":"{tag}","draft":false,"assets":[{{"name":"a.zip","size":123,"uploader":{{"login":"o"}}}}],"body":"notes \"quoted\" \u00e9\n"}}"#
        )
        .into_bytes()
    }

    #[test]
    fn answers_are_judged_against_the_current_version() {
        assert_eq!(evaluate(200, &body("v0.1.0"), "0.1.0", REPO), Ok(Outcome::UpToDate { latest: "v0.1.0".into() }));
        assert_eq!(evaluate(200, &body("v0.0.9"), "0.1.0", REPO), Ok(Outcome::UpToDate { latest: "v0.0.9".into() }));
        assert_eq!(
            evaluate(200, &body("v0.2.0"), "0.1.0", REPO),
            Ok(Outcome::Newer {
                latest: "v0.2.0".into(),
                page: "https://github.com/owner/repo/releases/tag/v0.2.0".into()
            })
        );
        assert_eq!(
            evaluate(200, &body("v0.2.0-beta.1"), "0.2.0", REPO),
            Ok(Outcome::UpToDate { latest: "v0.2.0-beta.1".into() })
        );
        assert_eq!(evaluate(403, b"{}", "0.1.0", REPO), Err(Failure::RateLimited(403)));
        assert_eq!(evaluate(429, b"", "0.1.0", REPO), Err(Failure::RateLimited(429)));
        assert_eq!(evaluate(404, b"{\"message\":\"Not Found\"}", "0.1.0", REPO), Err(Failure::Http(404)));
        assert_eq!(evaluate(500, b"", "0.1.0", REPO), Err(Failure::Http(500)));
        assert_eq!(evaluate(200, &vec![b' '; MAX_ANSWER + 1], "0.1.0", REPO), Err(Failure::TooLarge));
        // a newer release whose page is elsewhere opens the list of releases instead
        let elsewhere = br#"{"tag_name":"v9.0.0","html_url":"https://example.com/x"}"#;
        assert_eq!(
            evaluate(200, elsewhere, "0.1.0", REPO),
            Ok(Outcome::Newer { latest: "v9.0.0".into(), page: "https://github.com/owner/repo/releases".into() })
        );
    }

    #[test]
    fn json_fields_are_read_from_the_top_level_only() {
        let f =
            json::release_fields(br#" { "a" : {"tag_name":"v9.9.9"}, "tag_name" : "v1.\u0032.3" , "html_url":null } "#)
                .unwrap();
        assert_eq!(f.tag_name.as_deref(), Some("v1.2.3"));
        assert_eq!(f.html_url, None);
        let f = json::release_fields(
            "\u{feff}{\"tag_name\":\"v1\",\"x\":[1,-2.5e+3,0.1,true,false,null,\"\\ud83d\\ude00\"]}".as_bytes(),
        )
        .unwrap();
        assert_eq!(f.tag_name.as_deref(), Some("v1"));
        assert_eq!(json::release_fields(b"{}").unwrap().tag_name, None);
    }

    #[test]
    fn hostile_or_broken_json_is_unreadable() {
        let deep = format!("{{\"x\":{}{}}}", "[".repeat(10_000), "]".repeat(10_000));
        let ok_depth = format!("{{\"tag_name\":\"v1\",\"x\":{}{}}}", "[".repeat(60), "]".repeat(60));
        assert!(json::release_fields(ok_depth.as_bytes()).is_some());
        let cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"null".to_vec(),
            b"[]".to_vec(),
            b"\"v1.0.0\"".to_vec(),
            b"{".to_vec(),
            b"{\"tag_name\":\"v1\"".to_vec(),
            b"{\"tag_name\":\"v1\"} trailing".to_vec(),
            b"{\"tag_name\":\"v1\"}{}".to_vec(),
            b"{\"tag_name\":\"v1\",}".to_vec(),
            b"{\"tag_name\" \"v1\"}".to_vec(),
            b"{'tag_name':'v1'}".to_vec(),
            b"{tag_name:\"v1\"}".to_vec(),
            b"{\"tag_name\":1}".to_vec(),
            b"{\"tag_name\":null}".to_vec(),
            b"{\"tag_name\":[\"v1\"]}".to_vec(),
            b"{\"tag_name\":\"v1\",\"tag_name\":\"v9\"}".to_vec(),
            b"{\"html_url\":\"a\",\"html_url\":\"b\",\"tag_name\":\"v1\"}".to_vec(),
            b"{\"tag_name\":\"v1\x01\"}".to_vec(),
            b"{\"tag_name\":\"v1\nx\"}".to_vec(),
            b"{\"tag_name\":\"\\x41\"}".to_vec(),
            b"{\"tag_name\":\"\\ud800\"}".to_vec(),
            b"{\"tag_name\":\"\\udc00\"}".to_vec(),
            b"{\"tag_name\":\"\\ud800\\u0041\"}".to_vec(),
            b"{\"tag_name\":\"\\u00g1\"}".to_vec(),
            b"{\"tag_name\":\"\\u00\"}".to_vec(),
            b"{\"tag_name\":\"v1\",\"n\":01}".to_vec(),
            b"{\"tag_name\":\"v1\",\"n\":1.}".to_vec(),
            b"{\"tag_name\":\"v1\",\"n\":-}".to_vec(),
            b"{\"tag_name\":\"v1\",\"n\":1e}".to_vec(),
            b"{\"tag_name\":\"v1\",\"n\":NaN}".to_vec(),
            b"{\"tag_name\":\"v1\",\"n\":tru}".to_vec(),
            b"{\"tag_name\":\"v1\",\"a\":[1,]}".to_vec(),
            b"{\"tag_name\":\"v1\",\"a\":[1 2]}".to_vec(),
            b"{\"tag_name\":\"v1\",\"o\":{\"k\"}}".to_vec(),
            b"{\"tag_name\":\"\xff\xfe\"}".to_vec(),
            b"{\"tag_name\":\"v1\"}\x00".to_vec(),
            deep.into_bytes(),
        ];
        for c in &cases {
            assert!(json::release_fields(c).is_none(), "{:?}", String::from_utf8_lossy(c));
            assert_eq!(evaluate(200, c, "0.1.0", REPO), Err(Failure::Unreadable), "{:?}", String::from_utf8_lossy(c));
        }
        // a tag that is not a version
        assert_eq!(evaluate(200, br#"{"tag_name":"nightly"}"#, "0.1.0", REPO), Err(Failure::Unreadable));
        assert_eq!(evaluate(200, br#"{"name":"v1.0.0"}"#, "0.1.0", REPO), Err(Failure::Unreadable));
    }

    #[test]
    fn every_result_is_worded_in_both_languages() {
        let states = [
            State::Checking,
            State::Done(Ok(Outcome::UpToDate { latest: "v0.1.0".into() })),
            State::Done(Ok(Outcome::Newer { latest: "v0.2.0".into(), page: String::new() })),
            State::Done(Err(Failure::Offline)),
            State::Done(Err(Failure::Timeout)),
            State::Done(Err(Failure::Tls)),
            State::Done(Err(Failure::Network(12030))),
            State::Done(Err(Failure::RateLimited(403))),
            State::Done(Err(Failure::Http(404))),
            State::Done(Err(Failure::TooLarge)),
            State::Done(Err(Failure::Unreadable)),
        ];
        assert_eq!(status_text(&State::Idle, &KO), "");
        for s in [&KO, &EN] {
            for st in &states {
                let text = status_text(st, s);
                assert!(!text.is_empty() && !text.contains('{'), "{st:?}: {text}");
            }
        }
        assert_eq!(status_text(&states[2], &EN), format!("New version available: v0.2.0 (this is {CURRENT})"));
        assert!(status_text(&states[7], &EN).contains("403"));
        assert!(status_text(&states[8], &KO).contains("404"));
    }
}
