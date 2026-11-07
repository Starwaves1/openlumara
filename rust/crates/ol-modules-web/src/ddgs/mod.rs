//! Port of the parts of the `ddgs` 9.16.1 pip library that `modules/web_search/module.py` calls:
//! `DDGS(proxy=...)` and its `text / images / news / videos / books` methods (`ddgs/ddgs.py`).
//!
//! The backends (`ddgs/engines/*.py`) that are enabled in that release are ported one file each.
//! Disabled engines (google, bing text, yandex) are not ported, as in python's registry.
//! `extract()`, the api server, the CLI and the `keywords=` alias are not used by web_search and
//! are not ported.

pub mod annasarchive;
pub mod bing_images;
pub mod bing_news;
pub mod brave;
pub mod duckduckgo;
pub mod duckduckgo_images;
pub mod duckduckgo_news;
pub mod duckduckgo_videos;
pub mod grokipedia;
pub mod html;
pub mod http;
pub mod mojeek;
pub mod results;
pub mod startpage;
pub mod wikipedia;
pub mod yahoo;
pub mod yahoo_news;

use futures::future::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};
use http::Net;
use rand::seq::SliceRandom;
use results::{Aggregator, Record};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

/// `DDGSException` / `TimeoutException`
#[derive(Debug, Clone, PartialEq)]
pub enum DdgsError {
    Timeout(String),
    Failed(String),
}

impl DdgsError {
    pub fn message(&self) -> String {
        match self {
            DdgsError::Timeout(m) | DdgsError::Failed(m) => m.clone(),
        }
    }
}

impl std::fmt::Display for DdgsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for DdgsError {}

/// the `DDGS` method families (`BaseSearchEngine.category`)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Text,
    Images,
    News,
    Videos,
    Books,
}

/// the keyword arguments `DDGS.text(...)` and friends receive from web_search
#[derive(Clone, Debug)]
pub struct SearchArgs {
    pub region: String,
    pub safesearch: String,
    pub timelimit: Option<String>,
    pub max_results: Option<usize>,
    pub page: i64,
    pub backend: String,
    /// category kwargs (size, color, type_image, layout, license_image, resolution, duration,
    /// license_videos); an absent or empty value is python's None / ""
    pub extra: HashMap<String, String>,
}

impl Default for SearchArgs {
    fn default() -> Self {
        Self {
            region: "us-en".into(),
            safesearch: "moderate".into(),
            timelimit: None,
            max_results: Some(10),
            page: 1,
            backend: "auto".into(),
            extra: HashMap::new(),
        }
    }
}

impl SearchArgs {
    /// `timelimit` when truthy
    pub fn timelimit(&self) -> Option<&str> {
        self.timelimit.as_deref().filter(|t| !t.is_empty())
    }

    /// a category kwarg when truthy, else ""
    pub fn extra(&self, key: &str) -> &str {
        self.extra.get(key).map(String::as_str).unwrap_or("")
    }
}

/// a request payload: ordered key/value pairs (python dict order)
pub type Payload = Vec<(String, String)>;

pub fn kv(key: &str, value: impl ToString) -> (String, String) {
    (key.to_string(), value.to_string())
}

/// `region.lower().split("-")` unpacked into two parts (ValueError otherwise)
pub fn region_parts(region: &str) -> Result<(String, String), DdgsError> {
    let lower = region.to_lowercase();
    let parts: Vec<&str> = lower.split('-').collect();
    match parts.as_slice() {
        [a, b] => Ok((a.to_string(), b.to_string())),
        [_] => Err(DdgsError::Failed("not enough values to unpack (expected 2, got 1)".into())),
        _ => Err(DdgsError::Failed(format!("too many values to unpack (expected 2, got {})", parts.len()))),
    }
}

/// `mapping[key]` for a python dict literal of str -> str (KeyError when missing)
pub fn lookup(map: &[(&str, &str)], key: &str) -> Result<String, DdgsError> {
    map.iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| (*v).to_string())
        .ok_or_else(|| DdgsError::Failed(format!("KeyError: {key:?}")))
}

/// every engine that is enabled in ddgs 9.16.1 (`ENGINES` after the disabled ones are dropped)
pub const ENGINES: &[EngineKind] = &[
    EngineKind::Brave,
    EngineKind::DuckduckgoText,
    EngineKind::Mojeek,
    EngineKind::Startpage,
    EngineKind::Wikipedia,
    EngineKind::YahooText,
    EngineKind::Grokipedia,
    EngineKind::DuckduckgoImages,
    EngineKind::BingImages,
    EngineKind::DuckduckgoNews,
    EngineKind::BingNews,
    EngineKind::YahooNews,
    EngineKind::DuckduckgoVideos,
    EngineKind::AnnasArchive,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind {
    Brave,
    DuckduckgoText,
    Mojeek,
    Startpage,
    Wikipedia,
    YahooText,
    Grokipedia,
    DuckduckgoImages,
    BingImages,
    DuckduckgoNews,
    BingNews,
    YahooNews,
    DuckduckgoVideos,
    AnnasArchive,
}

impl EngineKind {
    /// `BaseSearchEngine.name`
    pub fn name(self) -> &'static str {
        match self {
            EngineKind::Brave => "brave",
            EngineKind::DuckduckgoText | EngineKind::DuckduckgoImages | EngineKind::DuckduckgoNews | EngineKind::DuckduckgoVideos => "duckduckgo",
            EngineKind::Mojeek => "mojeek",
            EngineKind::Startpage => "startpage",
            EngineKind::Wikipedia => "wikipedia",
            EngineKind::YahooText | EngineKind::YahooNews => "yahoo",
            EngineKind::Grokipedia => "grokipedia",
            EngineKind::BingImages | EngineKind::BingNews => "bing",
            EngineKind::AnnasArchive => "annasarchive",
        }
    }

    pub fn category(self) -> Category {
        match self {
            EngineKind::Brave
            | EngineKind::DuckduckgoText
            | EngineKind::Mojeek
            | EngineKind::Startpage
            | EngineKind::Wikipedia
            | EngineKind::YahooText
            | EngineKind::Grokipedia => Category::Text,
            EngineKind::DuckduckgoImages | EngineKind::BingImages => Category::Images,
            EngineKind::DuckduckgoNews | EngineKind::BingNews | EngineKind::YahooNews => Category::News,
            EngineKind::DuckduckgoVideos => Category::Videos,
            EngineKind::AnnasArchive => Category::Books,
        }
    }

    /// `BaseSearchEngine.provider`: the upstream whose results this engine returns
    pub fn provider(self) -> &'static str {
        match self {
            EngineKind::Brave => "brave",
            EngineKind::Mojeek => "mojeek",
            EngineKind::Startpage => "google",
            EngineKind::Wikipedia => "wikipedia",
            EngineKind::YahooNews => "yahoo",
            EngineKind::Grokipedia => "grokipedia",
            EngineKind::AnnasArchive => "annasarchive",
            EngineKind::DuckduckgoText
            | EngineKind::YahooText
            | EngineKind::DuckduckgoImages
            | EngineKind::BingImages
            | EngineKind::DuckduckgoNews
            | EngineKind::BingNews
            | EngineKind::DuckduckgoVideos => "bing",
        }
    }

    /// `BaseSearchEngine.priority`
    pub fn priority(self) -> f64 {
        match self {
            EngineKind::Wikipedia => 2.0,
            EngineKind::Grokipedia => 1.9,
            _ => 1.0,
        }
    }

    async fn search(self, net: &Net, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
        match self {
            EngineKind::Brave => brave::search(net, query, args).await,
            EngineKind::DuckduckgoText => duckduckgo::search(net, query, args).await,
            EngineKind::Mojeek => mojeek::search(net, query, args).await,
            EngineKind::Startpage => startpage::search(net, query, args).await,
            EngineKind::Wikipedia => wikipedia::search(net, query, args).await,
            EngineKind::YahooText => yahoo::search(net, query, args).await,
            EngineKind::Grokipedia => grokipedia::search(net, query, args).await,
            EngineKind::DuckduckgoImages => duckduckgo_images::search(net, query, args).await,
            EngineKind::BingImages => bing_images::search(net, query, args).await,
            EngineKind::DuckduckgoNews => duckduckgo_news::search(net, query, args).await,
            EngineKind::BingNews => bing_news::search(net, query, args).await,
            EngineKind::YahooNews => yahoo_news::search(net, query, args).await,
            EngineKind::DuckduckgoVideos => duckduckgo_videos::search(net, query, args).await,
            EngineKind::AnnasArchive => annasarchive::search(net, query, args).await,
        }
    }
}

/// `DDGS._get_engines(category, backend)`: the engines to run, best priority first
pub fn get_engines(cat: Category, backend: &str) -> Vec<EngineKind> {
    let names: Vec<&str> = backend.split(',').map(str::trim).collect();
    let mut shuffled: Vec<EngineKind> = ENGINES.iter().copied().filter(|e| e.category() == cat).collect();
    shuffled.shuffle(&mut rand::thread_rng());

    let selected: Vec<EngineKind> = if names.contains(&"auto") || names.contains(&"all") {
        if cat == Category::Text {
            let mut keys = vec![EngineKind::Wikipedia, EngineKind::Grokipedia];
            keys.extend(shuffled.into_iter().filter(|e| !matches!(e, EngineKind::Wikipedia | EngineKind::Grokipedia)));
            keys
        } else {
            shuffled
        }
    } else {
        // unknown names are skipped (python logs a warning for them)
        names
            .iter()
            .filter_map(|n| ENGINES.iter().copied().find(|e| e.category() == cat && e.name() == *n))
            .collect()
    };

    if selected.is_empty() {
        // "backend is not set. Using 'auto'"
        return get_engines(cat, "auto");
    }
    // stable sort by priority, highest first
    let mut sorted = selected;
    sorted.sort_by(|a, b| b.priority().partial_cmp(&a.priority()).unwrap_or(std::cmp::Ordering::Equal));
    sorted
}

type Done = (usize, EngineKind, Result<Vec<Record>, DdgsError>);

async fn run_engine(seq: usize, engine: EngineKind, net: &Net, query: &str, args: &SearchArgs) -> Done {
    (seq, engine, engine.search(net, query, args).await)
}

/// `wait(futures, timeout=self._timeout, return_when="FIRST_EXCEPTION")` followed by the
/// processing loop: returns the finished futures in submission order
async fn wait_batch<F: Future<Output = Done>>(pending: &mut FuturesUnordered<F>, timeout: Duration) -> Vec<Done> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut done = Vec::new();
    while !pending.is_empty() {
        match tokio::time::timeout_at(deadline, pending.next()).await {
            Ok(Some(item)) => {
                let failed = item.2.is_err();
                done.push(item);
                if failed {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    // futures that finished by now are part of the same wait
    while let Some(Some(item)) = pending.next().now_or_never() {
        done.push(item);
    }
    done.sort_by_key(|d| d.0);
    done
}

/// `DDGS(proxy=..., timeout=5)`
#[derive(Clone, Debug)]
pub struct Ddgs {
    net: Net,
}

impl Ddgs {
    /// `_expand_proxy_tb_alias(proxy) or os.environ.get("DDGS_PROXY")`
    pub fn new(proxy: Option<&str>) -> Self {
        let proxy = proxy
            .filter(|p| !p.is_empty())
            .map(|p| if p == "tb" { "socks5h://127.0.0.1:9150".to_string() } else { p.to_string() })
            .or_else(|| std::env::var("DDGS_PROXY").ok().filter(|p| !p.is_empty()));
        Self { net: Net { proxy, timeout: Duration::from_secs(5) } }
    }

    pub async fn text(&self, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
        self.search_sync(Category::Text, query, args).await
    }

    pub async fn images(&self, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
        self.search_sync(Category::Images, query, args).await
    }

    pub async fn news(&self, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
        self.search_sync(Category::News, query, args).await
    }

    pub async fn videos(&self, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
        self.search_sync(Category::Videos, query, args).await
    }

    pub async fn books(&self, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
        self.search_sync(Category::Books, query, args).await
    }

    /// `DDGS._search_sync`
    async fn search_sync(&self, cat: Category, query: &str, args: &SearchArgs) -> Result<Vec<Record>, DdgsError> {
        if query.is_empty() {
            return Err(DdgsError::Failed("query is mandatory.".into()));
        }
        let engines = get_engines(cat, &args.backend);
        let len_unique_providers = engines.iter().map(|e| e.provider()).collect::<HashSet<_>>().len();
        let max_results = args.max_results.filter(|m| *m > 0);
        // ceil(max_results / 10) + 1
        let max_workers = match max_results {
            Some(m) => len_unique_providers.min(m.div_ceil(10) + 1),
            None => len_unique_providers,
        };

        let mut agg = Aggregator::new(cat);
        let mut seen_providers: HashSet<&'static str> = HashSet::new();
        let mut err: Option<DdgsError> = None;
        let mut pending = FuturesUnordered::new();
        let mut seq = 0usize;

        for i in 1..=engines.len() + 1 {
            if i <= engines.len() {
                let engine = engines[i - 1];
                if seen_providers.contains(engine.provider()) {
                    continue;
                }
                pending.push(run_engine(seq, engine, &self.net, query, args));
                seq += 1;
            }
            if pending.len() >= max_workers || i >= max_workers {
                for (_, engine, res) in wait_batch(&mut pending, self.net.timeout).await {
                    match res {
                        Ok(items) if !items.is_empty() => match agg.extend(items) {
                            Ok(()) => {
                                seen_providers.insert(engine.provider());
                            }
                            Err(e) => err = Some(e),
                        },
                        Ok(_) => {}
                        Err(e) => err = Some(e),
                    }
                }
            }
            if let Some(m) = max_results {
                if agg.len() >= m {
                    break;
                }
            }
        }
        // python's `with ThreadPoolExecutor` waits for leftovers, but their results are discarded
        drop(pending);
        results::finish(&agg, query, max_results, err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_auto_puts_wikipedia_then_grokipedia_first() {
        for _ in 0..20 {
            let order = get_engines(Category::Text, "auto");
            assert_eq!(order.len(), 7);
            assert_eq!(order[0], EngineKind::Wikipedia);
            assert_eq!(order[1], EngineKind::Grokipedia);
            for expected in [EngineKind::Brave, EngineKind::DuckduckgoText, EngineKind::Mojeek, EngineKind::Startpage, EngineKind::YahooText] {
                assert!(order.contains(&expected));
            }
        }
    }

    #[test]
    fn explicit_backend_keeps_order_and_priority_sort_is_stable() {
        let order = get_engines(Category::Text, "mojeek, brave, wikipedia");
        assert_eq!(order, vec![EngineKind::Wikipedia, EngineKind::Mojeek, EngineKind::Brave]);
        // disabled or unknown names fall back to auto
        let order = get_engines(Category::Images, "google");
        assert_eq!(order.len(), 2);
        assert!(order.contains(&EngineKind::DuckduckgoImages));
        assert!(order.contains(&EngineKind::BingImages));
    }

    #[test]
    fn region_must_have_two_parts() {
        assert_eq!(region_parts("US-en").unwrap(), ("us".to_string(), "en".to_string()));
        assert!(region_parts("us").is_err());
        assert!(region_parts("us-en-gb").is_err());
    }

    #[test]
    fn proxy_tb_alias_and_empty_values() {
        let d = Ddgs::new(Some("tb"));
        assert_eq!(d.net.proxy.as_deref(), Some("socks5h://127.0.0.1:9150"));
        let d = Ddgs::new(Some("http://u:p@127.0.0.1:3128"));
        assert_eq!(d.net.proxy.as_deref(), Some("http://u:p@127.0.0.1:3128"));
        assert_eq!(d.net.timeout, Duration::from_secs(5));
    }

    #[tokio::test]
    async fn empty_query_is_rejected_before_any_request() {
        let err = Ddgs::new(None).text("", &SearchArgs::default()).await.unwrap_err();
        assert_eq!(err, DdgsError::Failed("query is mandatory.".into()));
    }
}
