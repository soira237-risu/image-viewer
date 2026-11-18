//! The one network request the viewer makes, and only when the user asks for it: GET the latest
//! release from GitHub's API with Windows' own HTTP stack (WinHTTP, so the system's proxy and
//! certificate store apply). It sends the version in the User-Agent and nothing else: no cookies,
//! no credentials. [`crate::update`] reads the answer.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, GetLastError};
use windows::Win32::Networking::WinHttp::*;
use windows::core::{PCWSTR, w};

use super::wide;
use crate::update::{self, CURRENT, Failure, MAX_ANSWER, Outcome, REPOSITORY};

/// How long each step (name lookup, connecting, sending, waiting for data) may take, and the
/// whole answer too. WinHTTP rounds its own timeouts up to a coarse step (about 4 s here), so a
/// server that never answers is given up on after about 12 s.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Where to ask. Plain HTTP is allowed only to this computer (`127.0.0.1`, `localhost`, `::1`);
/// anything else goes over TLS.
#[derive(Clone, Debug)]
pub struct Endpoint {
    host: String,
    port: u16,
    path: String,
    secure: bool,
    timeout: Duration,
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

impl Endpoint {
    /// The latest release of the repository in Cargo.toml, over HTTPS.
    pub fn github() -> Option<Endpoint> {
        let (host, path) = update::latest_release_api(REPOSITORY)?;
        Some(Endpoint { host: host.into(), port: 443, path, secure: true, timeout: TIMEOUT })
    }

    /// Any server; `None` for plain HTTP to another computer.
    pub fn new(host: &str, port: u16, path: &str, secure: bool, timeout: Duration) -> Option<Endpoint> {
        (secure || is_loopback(host)).then(|| Endpoint { host: host.into(), port, path: path.into(), secure, timeout })
    }
}

/// A WinHTTP handle, closed when dropped.
struct Handle(*mut c_void);

impl Handle {
    fn new(h: *mut c_void) -> Result<Handle, Failure> {
        if h.is_null() { Err(last_failure()) } else { Ok(Handle(h)) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

/// A WinHTTP (or Win32) error code as a reason.
fn failure(code: u32) -> Failure {
    match code {
        ERROR_WINHTTP_TIMEOUT => Failure::Timeout,
        ERROR_WINHTTP_NAME_NOT_RESOLVED | ERROR_WINHTTP_CANNOT_CONNECT => Failure::Offline,
        ERROR_WINHTTP_SECURE_FAILURE | ERROR_WINHTTP_SECURE_FAILURE_PROXY => Failure::Tls,
        ERROR_WINHTTP_INVALID_SERVER_RESPONSE => Failure::Unreadable,
        c => Failure::Network(c),
    }
}

fn last_failure() -> Failure {
    failure(unsafe { GetLastError() }.0)
}

/// The Win32 code inside an error from a WinHTTP call.
fn error_failure(e: &windows::core::Error) -> Failure {
    let hr = e.code().0 as u32;
    failure(if hr & 0xffff_0000 == 0x8007_0000 { hr & 0xffff } else { hr })
}

fn set_u32(h: &Handle, option: u32, value: u32) -> windows::core::Result<()> {
    unsafe { WinHttpSetOption(Some(h.0), option, Some(&value.to_ne_bytes())) }
}

fn query_number(request: &Handle, what: u32) -> Option<u32> {
    let mut value = 0u32;
    let mut len = 4u32;
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            what | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut value as *mut u32).cast()),
            &mut len,
            std::ptr::null_mut(),
        )
        .ok()?;
    }
    Some(value)
}

/// GETs `ep`: the HTTP status and up to [`MAX_ANSWER`] bytes of body.
pub fn fetch(ep: &Endpoint) -> Result<(u16, Vec<u8>), Failure> {
    let start = Instant::now();
    let agent = wide(&format!("image-viewer/{CURRENT}"));
    let ms = i32::try_from(ep.timeout.as_millis()).unwrap_or(i32::MAX).max(1);
    unsafe {
        // the system's proxy settings (automatic detection and scripts included) where Windows
        // supports that, else the configured proxy; this computer is never reached by a proxy
        let open = |access| WinHttpOpen(PCWSTR(agent.as_ptr()), access, PCWSTR::null(), PCWSTR::null(), 0);
        let session = if is_loopback(&ep.host) {
            Handle::new(open(WINHTTP_ACCESS_TYPE_NO_PROXY))?
        } else {
            let h = open(WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY);
            if h.is_null() && GetLastError() == ERROR_INVALID_PARAMETER {
                Handle::new(open(WINHTTP_ACCESS_TYPE_DEFAULT_PROXY))?
            } else {
                Handle::new(h)?
            }
        };
        WinHttpSetTimeouts(session.0, ms, ms, ms, ms).map_err(|e| error_failure(&e))?;
        if ep.secure {
            let tls = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
            // TLS 1.3 is refused by older Windows 10; 1.2 alone is then still fine
            if set_u32(&session, WINHTTP_OPTION_SECURE_PROTOCOLS, tls).is_err() {
                let _ = set_u32(&session, WINHTTP_OPTION_SECURE_PROTOCOLS, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2);
            }
        }
        let host = wide(ep.host.trim_start_matches('[').trim_end_matches(']'));
        let connection = Handle::new(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), ep.port, 0))?;
        let path = wide(&ep.path);
        let flags = if ep.secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
        let request = Handle::new(WinHttpOpenRequest(
            connection.0,
            w!("GET"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        ))?;
        // no cookies kept or sent, and never the user's Windows credentials
        let _ = set_u32(&request, WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_DISABLE_COOKIES);
        let _ = set_u32(&request, WINHTTP_OPTION_AUTOLOGON_POLICY, WINHTTP_AUTOLOGON_SECURITY_LEVEL_HIGH);
        // waiting for the response headers has its own limit (90 s unless set)
        set_u32(&request, WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT, ms as u32).map_err(|e| error_failure(&e))?;
        let headers: Vec<u16> = "Accept: application/vnd.github+json\r\n".encode_utf16().collect();
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0).map_err(|e| error_failure(&e))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| error_failure(&e))?;
        let status = query_number(&request, WINHTTP_QUERY_STATUS_CODE)
            .and_then(|s| u16::try_from(s).ok())
            .ok_or(Failure::Unreadable)?;
        if query_number(&request, WINHTTP_QUERY_CONTENT_LENGTH).is_some_and(|n| n as usize > MAX_ANSWER) {
            return Err(Failure::TooLarge);
        }
        let mut body = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            if start.elapsed() > ep.timeout {
                return Err(Failure::Timeout);
            }
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut read)
                .map_err(|e| error_failure(&e))?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > MAX_ANSWER {
                return Err(Failure::TooLarge);
            }
        }
        Ok((status, body))
    }
}

/// Asks `ep` for the latest release and compares it with this version.
pub fn check_at(ep: &Endpoint) -> Result<Outcome, Failure> {
    let (status, body) = fetch(ep)?;
    update::evaluate(status, &body, CURRENT, REPOSITORY)
}

/// Asks GitHub for the latest release of this program.
pub fn check() -> Result<Outcome, Failure> {
    // the repository URL is fixed at build time and tested, so this does not happen
    let ep = Endpoint::github().ok_or(Failure::Unreadable)?;
    check_at(&ep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    /// A one-request HTTP server on this computer: it reads the request, hands its head back
    /// through the channel, waits `delay`, and answers `response`.
    fn serve(response: Vec<u8>, delay: Duration) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else { return };
            let head = read_head(&mut s);
            let _ = tx.send(head);
            std::thread::sleep(delay);
            let _ = s.write_all(&response);
            let _ = s.flush();
            let _ = s.shutdown(std::net::Shutdown::Write);
            // let the client read everything before the socket closes
            let mut rest = [0u8; 256];
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            while matches!(s.read(&mut rest), Ok(n) if n > 0) {}
        });
        (port, rx)
    }

    fn read_head(s: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
            match s.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => break,
            }
        }
        String::from_utf8_lossy(&head).into_owned()
    }

    fn answer(status: &str, body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    const PATH: &str = "/repos/soira237-risu/image-viewer/releases/latest";

    fn ask(response: Vec<u8>, delay: Duration, timeout: Duration) -> (Result<Outcome, Failure>, String) {
        let (port, head) = serve(response, delay);
        let ep = Endpoint::new("127.0.0.1", port, PATH, false, timeout).unwrap();
        let result = check_at(&ep);
        (result, head.recv_timeout(Duration::from_secs(5)).unwrap_or_default())
    }

    fn release(tag: &str) -> Vec<u8> {
        format!(
            r#"{{"html_url":"https://github.com/soira237-risu/image-viewer/releases/tag/{tag}","tag_name":"{tag}","assets":[],"body":"x"}}"#
        )
        .into_bytes()
    }

    #[test]
    fn plain_http_only_to_this_computer() {
        assert!(Endpoint::new("api.github.com", 80, "/", false, TIMEOUT).is_none());
        assert!(Endpoint::new("192.168.0.1", 80, "/", false, TIMEOUT).is_none());
        assert!(Endpoint::new("127.0.0.1", 80, "/", false, TIMEOUT).is_some());
        assert!(Endpoint::new("example.com", 443, "/", true, TIMEOUT).is_some());
        let gh = Endpoint::github().unwrap();
        assert!(gh.secure && gh.port == 443 && gh.host == "api.github.com" && gh.path == PATH, "{gh:?}");
        assert_eq!(gh.timeout, TIMEOUT);
    }

    #[test]
    fn a_newer_release_is_found_and_only_the_version_is_sent() {
        let (r, head) = ask(answer("200 OK", &release("v99.0.0")), Duration::ZERO, TIMEOUT);
        assert_eq!(
            r,
            Ok(Outcome::Newer {
                latest: "v99.0.0".into(),
                page: "https://github.com/soira237-risu/image-viewer/releases/tag/v99.0.0".into()
            })
        );
        let lines: Vec<&str> = head.lines().collect();
        assert_eq!(lines[0], format!("GET {PATH} HTTP/1.1"));
        assert!(lines.contains(&format!("User-Agent: image-viewer/{CURRENT}").as_str()), "{head}");
        assert!(lines.contains(&"Accept: application/vnd.github+json"), "{head}");
        // nothing about the user: only the headers HTTP itself needs
        for line in &lines[1..] {
            let name = line.split(':').next().unwrap_or("").to_ascii_lowercase();
            assert!(
                matches!(name.as_str(), "" | "user-agent" | "accept" | "host" | "connection"),
                "unexpected header {line:?} in\n{head}"
            );
        }
    }

    #[test]
    fn the_same_version_is_up_to_date() {
        let tag = format!("v{CURRENT}");
        let (r, _) = ask(answer("200 OK", &release(&tag)), Duration::ZERO, TIMEOUT);
        assert_eq!(r, Ok(Outcome::UpToDate { latest: tag }));
    }

    #[test]
    fn http_errors_and_rate_limits_are_reported() {
        let (r, _) = ask(answer("404 Not Found", br#"{"message":"Not Found"}"#), Duration::ZERO, TIMEOUT);
        assert_eq!(r, Err(Failure::Http(404)));
        let (r, _) = ask(answer("403 Forbidden", br#"{"message":"API rate limit exceeded"}"#), Duration::ZERO, TIMEOUT);
        assert_eq!(r, Err(Failure::RateLimited(403)));
        let (r, _) = ask(answer("429 Too Many Requests", b""), Duration::ZERO, TIMEOUT);
        assert_eq!(r, Err(Failure::RateLimited(429)));
    }

    #[test]
    fn oversized_answers_are_refused() {
        let big = vec![b' '; MAX_ANSWER + 10];
        // announced by Content-Length
        let (r, _) = ask(answer("200 OK", &big), Duration::ZERO, TIMEOUT);
        assert_eq!(r, Err(Failure::TooLarge));
        // or not announced: counted while reading
        let mut unannounced =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n".to_vec();
        unannounced.extend_from_slice(&big);
        let (r, _) = ask(unannounced, Duration::ZERO, TIMEOUT);
        assert_eq!(r, Err(Failure::TooLarge));
    }

    #[test]
    fn garbage_is_unreadable() {
        let (r, _) = ask(answer("200 OK", b"<html>rate limited</html>"), Duration::ZERO, TIMEOUT);
        assert_eq!(r, Err(Failure::Unreadable));
        let (r, _) = ask(answer("200 OK", br#"{"tag_name":"v1.0.0""#), Duration::ZERO, TIMEOUT);
        assert_eq!(r, Err(Failure::Unreadable));
        let (r, _) = ask(b"NOT HTTP AT ALL\r\n\r\n".to_vec(), Duration::ZERO, TIMEOUT);
        assert!(matches!(r, Err(Failure::Unreadable | Failure::Network(_))), "{r:?}");
    }

    /// A server that takes the request and does not answer. WinHTTP checks its timeouts on a
    /// coarse clock (measured here: a timeout of 0.5 to 3 s fires after 4 s, 6 s after 8 s), so
    /// this takes about 4 s whatever shorter timeout is set.
    #[test]
    fn a_slow_server_times_out() {
        let started = Instant::now();
        let (r, head) = ask(answer("200 OK", &release("v99.0.0")), Duration::from_secs(9), Duration::from_millis(500));
        assert_eq!(r, Err(Failure::Timeout));
        assert!(head.starts_with("GET "), "the request arrived: {head:?}");
        assert!(started.elapsed() < Duration::from_secs(7), "gave up after {:?}", started.elapsed());
    }

    #[test]
    fn nobody_listening_is_offline() {
        // a port that was free a moment ago
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let ep = Endpoint::new("127.0.0.1", port, PATH, false, Duration::from_secs(2)).unwrap();
        assert_eq!(check_at(&ep), Err(Failure::Offline));
    }
}
