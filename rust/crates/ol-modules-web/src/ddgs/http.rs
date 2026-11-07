//! HTTP side of ddgs (`http_client.py` + the per-engine `self.http_client` use).
//!
//! primp (the python client) impersonates a random browser's TLS and header fingerprint. That
//! cannot be reproduced with reqwest, so requests go out with one fixed browser User-Agent and
//! each engine's own headers. Cookies are kept per engine session like primp's client does.

use super::DdgsError;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, COOKIE, SET_COOKIE};
use std::time::Duration;

/// User-Agent sent on every engine request (primp picks one at random per client instead)
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// `DDGS(proxy=..., timeout=...)` settings handed to every engine's client
#[derive(Clone, Debug)]
pub struct Net {
    pub proxy: Option<String>,
    pub timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// the parts of a response that ddgs reads: status code and body bytes
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    /// `resp.text` (utf-8, invalid sequences replaced)
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

struct Cookie {
    domain: String,
    name: String,
    value: String,
}

/// one engine's HTTP client: a reqwest client plus the cookies it has been given or has received
pub struct Session {
    client: reqwest::Client,
    cookies: Vec<Cookie>,
}

fn fail(e: impl std::fmt::Display) -> DdgsError {
    DdgsError::Failed(format!("{e}"))
}

fn map_request_error(e: reqwest::Error) -> DdgsError {
    if e.is_timeout() {
        DdgsError::Timeout(format!("request timed out: {e}"))
    } else {
        fail(e)
    }
}

/// host part of an absolute url ("" when it has none)
fn host_of(url: &str) -> String {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_lowercase)).unwrap_or_default()
}

/// cookiejar-style domain match for a cookie stored for `domain`
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

impl Session {
    /// `HttpClient(proxy=..., timeout=...)` plus `headers_update` of the engine
    pub fn new(net: &Net, headers: &[(&str, &str)]) -> Result<Self, DdgsError> {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(fail)?;
            let value = HeaderValue::from_str(v).map_err(fail)?;
            map.insert(name, value);
        }
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(map)
            .timeout(net.timeout);
        if let Some(proxy) = net.proxy.as_deref() {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(fail)?);
        }
        let client = builder.build().map_err(fail)?;
        Ok(Self { client, cookies: Vec::new() })
    }

    /// `client.set_cookies(url, cookies)`: cookies scoped to the url's host
    pub fn set_cookies(&mut self, url: &str, cookies: &[(&str, &str)]) {
        let host = host_of(url);
        for (name, value) in cookies {
            self.store(host.clone(), name, value);
        }
    }

    fn store(&mut self, domain: String, name: &str, value: &str) {
        self.cookies.retain(|c| !(c.domain == domain && c.name == name));
        self.cookies.push(Cookie { domain, name: name.to_string(), value: value.to_string() });
    }

    fn cookie_header(&self, host: &str) -> Option<String> {
        let parts: Vec<String> = self
            .cookies
            .iter()
            .filter(|c| domain_matches(host, &c.domain))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    /// stores one response's Set-Cookie headers (name=value, Domain, and Max-Age<=0 deletion)
    fn absorb_set_cookies(&mut self, host: &str, headers: &HeaderMap) {
        for raw in headers.get_all(SET_COOKIE) {
            let Ok(raw) = raw.to_str() else { continue };
            let mut parts = raw.split(';');
            let Some((name, value)) = parts.next().and_then(|p| p.split_once('=')) else { continue };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let mut domain = host.to_string();
            let mut delete = false;
            for attr in parts {
                let (k, v) = attr.split_once('=').unwrap_or((attr, ""));
                match k.trim().to_ascii_lowercase().as_str() {
                    "domain" => domain = v.trim().trim_start_matches('.').to_lowercase(),
                    "max-age" => delete = v.trim().parse::<i64>().map(|n| n <= 0).unwrap_or(false),
                    _ => {}
                }
            }
            if delete {
                self.cookies.retain(|c| !(c.domain == domain && c.name == name));
            } else {
                self.store(domain, name, value.trim());
            }
        }
    }

    /// the request as python builds it: `params=payload` for GET, `data=payload` (form) for POST
    fn prepare(&self, method: Method, url: &str, payload: &[(String, String)]) -> reqwest::RequestBuilder {
        let req = match method {
            Method::Get => {
                let req = self.client.get(url);
                if payload.is_empty() { req } else { req.query(payload) }
            }
            Method::Post => self.client.post(url).form(payload),
        };
        match self.cookie_header(&host_of(url)) {
            Some(cookie) => req.header(COOKIE, cookie),
            None => req,
        }
    }

    /// `self.request(method, url, params=payload | data=payload)` without the status check
    pub async fn request(&mut self, method: Method, url: &str, payload: &[(String, String)]) -> Result<Reply, DdgsError> {
        let host = host_of(url);
        let resp = self.prepare(method, url, payload).send().await.map_err(map_request_error)?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        self.absorb_set_cookies(&host, &headers);
        let body = resp.bytes().await.map_err(map_request_error)?.to_vec();
        Ok(Reply { status, body })
    }

    /// `BaseSearchEngine.request`: the text of a 200 response, None otherwise
    pub async fn request_text(&mut self, method: Method, url: &str, payload: &[(String, String)]) -> Result<Option<String>, DdgsError> {
        let reply = self.request(method, url, payload).await?;
        Ok((reply.status == 200).then(|| reply.text()))
    }
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > haystack.len() || needle.is_empty() {
        return None;
    }
    haystack[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// `_extract_vqd(html_bytes, query)`: the vqd token DuckDuckGo embeds in its search page
pub fn extract_vqd(html: &[u8], query: &str) -> Result<String, DdgsError> {
    let patterns: [(&[u8], usize, &[u8]); 3] = [(b"vqd=\"", 5, b"\""), (b"vqd=", 4, b"&"), (b"vqd='", 5, b"'")];
    for (c1, c1_len, c2) in patterns {
        if let Some(i) = find(html, c1, 0) {
            let start = i + c1_len;
            if let Some(end) = find(html, c2, start) {
                return String::from_utf8(html[start..end].to_vec()).map_err(fail);
            }
        }
    }
    Err(DdgsError::Failed(format!("_extract_vqd() query={} Could not extract vqd.", crate::util::py_quote(query))))
}

/// `get_vqd(query)` of the duckduckgo images/news/videos engines
pub async fn get_vqd(session: &mut Session, query: &str) -> Result<String, DdgsError> {
    let reply = session.request(Method::Get, "https://duckduckgo.com", &[("q".to_string(), query.to_string())]).await?;
    extract_vqd(&reply.body, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vqd_is_taken_from_the_first_pattern_that_closes() {
        let page = br#"<script>var x = {vqd="3-1234567890-abc", other=1};</script>"#;
        assert_eq!(extract_vqd(page, "cats").unwrap(), "3-1234567890-abc");
        // the bare `vqd=` form ends at '&'
        assert_eq!(extract_vqd(b"a?vqd=4-99&x=1", "q").unwrap(), "4-99");
        // the quoted form has no closing quote after its start, so the bare form is tried:
        // python takes the text from the first `vqd=` up to the next `&`
        assert_eq!(extract_vqd(b"vqd=\"broken&vqd=7&", "q").unwrap(), "\"broken");
    }

    #[test]
    fn vqd_missing_is_an_error_with_python_message() {
        let err = extract_vqd(b"<html>nothing</html>", "rust").unwrap_err();
        assert_eq!(err.message(), "_extract_vqd() query='rust' Could not extract vqd.");
    }

    #[test]
    fn payload_is_sent_as_query_for_get_and_form_for_post() {
        let net = Net { proxy: None, timeout: Duration::from_secs(5) };
        let s = Session::new(&net, &[]).unwrap();
        let payload = vec![("q".to_string(), "cats & dogs".to_string()), ("vqd".to_string(), "4-1".to_string())];
        let get = s.prepare(Method::Get, "https://duckduckgo.com/i.js", &payload).build().unwrap();
        assert_eq!(get.url().as_str(), "https://duckduckgo.com/i.js?q=cats+%26+dogs&vqd=4-1");
        let post = s.prepare(Method::Post, "https://html.duckduckgo.com/html/", &payload).build().unwrap();
        assert_eq!(post.url().as_str(), "https://html.duckduckgo.com/html/");
        let body = post.body().and_then(|b| b.as_bytes()).unwrap();
        assert_eq!(body, b"q=cats+%26+dogs&vqd=4-1");
        let bare = s.prepare(Method::Get, "https://en.wikipedia.org/w/api.php", &[]).build().unwrap();
        assert_eq!(bare.url().as_str(), "https://en.wikipedia.org/w/api.php");
    }

    #[test]
    fn cookies_are_scoped_to_the_host_and_replaced_by_name() {
        let net = Net { proxy: None, timeout: Duration::from_secs(5) };
        let mut s = Session::new(&net, &[]).unwrap();
        s.set_cookies("https://search.brave.com/x", &[("us", "us"), ("useLocation", "0")]);
        s.set_cookies("https://search.brave.com/x", &[("us", "gb")]);
        assert_eq!(s.cookie_header("search.brave.com").as_deref(), Some("useLocation=0; us=gb"));
        assert_eq!(s.cookie_header("example.com"), None);
        assert!(domain_matches("www.mojeek.com", "mojeek.com"));
        assert!(!domain_matches("notmojeek.com", "mojeek.com"));
    }
}
