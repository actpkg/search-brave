//! Search the web with the [Brave Search API](https://api-dashboard.search.brave.com/).
//!
//! Brave runs its own index rather than reselling Bing or Google, which is why it is the
//! hosted engine we wrap first.
//!
//! Two design points worth keeping:
//!
//! * The endpoint is fixed at build time, so `act.toml` declares a ceiling of exactly
//!   `api.search.brave.com`. That is a property of the artifact — this component cannot
//!   reach anything else no matter how it is deployed.
//! * The subscription token is a **secret**, so it arrives through session args rather
//!   than tool arguments. In a dataflow graph a node's `params` live in the flow
//!   document, which is published and signed; credentials must never go there.
//!
//! The response shape is deliberately identical to the other `search-*` components so
//! they are interchangeable in a graph. It is duplicated rather than shared through a
//! crate until the schema settles and there are three or more consumers.

use act_sdk::prelude::*;
use serde::Serialize;

const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const DEFAULT_TIMEOUT_MS: u64 = 15_000;

// ── Public (normalised) shape ────────────────────────────────────────────────

/// One search result.
#[derive(Serialize, JsonSchema)]
struct SearchResult {
    /// Result title.
    title: String,
    /// Result URL.
    url: String,
    /// Short extract, when one was provided.
    snippet: Option<String>,
    /// Publication date as reported upstream; format varies.
    published: Option<String>,
    /// Relevance score. Always null here — Brave does not expose one.
    score: Option<f64>,
    /// Engine that produced this result. Always `brave`.
    engine: Option<String>,
    /// Result section, e.g. `general` or `news`.
    category: Option<String>,
}

/// A normalised search response.
#[derive(Serialize, JsonSchema)]
struct SearchResponse {
    /// The query as understood by the engine.
    query: String,
    /// Ranked results.
    results: Vec<SearchResult>,
    /// Instant answers, from Brave's FAQ block when present.
    answers: Vec<String>,
    /// Spelling corrections — Brave reports these as an altered query.
    suggestions: Vec<String>,
    /// Always empty. Present so the shape matches the other search components,
    /// where a metasearch engine can report upstream failures.
    unresponsive_engines: Vec<String>,
}

// ── Brave wire format ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    query: Option<WireQuery>,
    #[serde(default)]
    web: Option<WireSection>,
    #[serde(default)]
    news: Option<WireSection>,
    #[serde(default)]
    faq: Option<WireFaq>,
}

#[derive(Deserialize)]
struct WireQuery {
    #[serde(default)]
    original: Option<String>,
    #[serde(default)]
    altered: Option<String>,
}

#[derive(Deserialize)]
struct WireSection {
    #[serde(default)]
    results: Vec<WireResult>,
}

#[derive(Deserialize)]
struct WireResult {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    page_age: Option<String>,
    #[serde(default)]
    age: Option<String>,
}

#[derive(Deserialize)]
struct WireFaq {
    #[serde(default)]
    results: Vec<WireFaqEntry>,
}

#[derive(Deserialize)]
struct WireFaqEntry {
    #[serde(default)]
    answer: Option<String>,
}

impl WireResult {
    fn normalise(self, category: &str) -> SearchResult {
        SearchResult {
            title: self.title,
            url: self.url,
            snippet: self.description.filter(|s| !s.is_empty()),
            // `page_age` is an exact timestamp; `age` is a coarse "3 days ago". Prefer
            // the precise one and fall back rather than dropping the information.
            published: self.page_age.or(self.age).filter(|s| !s.is_empty()),
            score: None,
            engine: Some("brave".to_string()),
            category: Some(category.to_string()),
        }
    }
}

/// Brave's error envelope: `{"error":{"code":…,"detail":…,"status":…},"type":…}`.
#[derive(Deserialize)]
struct WireErrorEnvelope {
    error: WireError,
}

#[derive(Deserialize)]
struct WireError {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    detail: Option<String>,
}

/// Map a failed response onto an actionable error.
///
/// The status code alone is not enough to tell these apart: Brave answers an invalid
/// subscription token with **HTTP 422**, the same status it uses for a malformed query,
/// so dispatching on status would name the wrong fix. The machine-readable `code` in the
/// body is the reliable signal — verified against the live API on 2026-08-01.
fn status_error(status: u16, body: &str) -> ActError {
    let parsed: Option<WireError> = serde_json::from_str::<WireErrorEnvelope>(body)
        .ok()
        .map(|e| e.error);
    let code = parsed
        .as_ref()
        .and_then(|e| e.code.clone())
        .unwrap_or_default();
    let detail = parsed
        .as_ref()
        .and_then(|e| e.detail.clone())
        .unwrap_or_else(|| truncate(body.trim(), 300));
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(" {detail}")
    };

    if code.contains("TOKEN") || matches!(status, 401 | 403) {
        return ActError::invalid_args(format!(
            "Brave Search rejected the subscription token.{suffix} Check the `api_key` \
             passed in the session args — it must be a Web Search token from \
             api-dashboard.search.brave.com."
        ));
    }
    if code.contains("RATE") || status == 429 {
        return ActError::internal(format!(
            "Brave Search rate limit reached.{suffix} The free tier allows one request \
             per second."
        ));
    }
    if status == 422 {
        return ActError::invalid_args(format!(
            "Brave Search rejected the request (HTTP 422).{suffix} Note `count` is capped \
             at 20 and `offset` at 9."
        ));
    }
    ActError::internal(format!("Brave Search returned HTTP {status}.{suffix}"))
}

/// Trim a body to a readable length for inclusion in an error message.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// Search query.
    query: String,
    /// Number of results to return, 1–20 (Brave's maximum).
    count: Option<u8>,
    /// Zero-based page offset, 0–9.
    offset: Option<u8>,
    /// Two-letter country code, e.g. `us`, `de`.
    country: Option<String>,
    /// Result language, e.g. `en`.
    search_lang: Option<String>,
    /// Safe search level: `off`, `moderate` or `strict`.
    safesearch: Option<String>,
    /// Restrict results by age: `pd` (day), `pw` (week), `pm` (month), `py` (year),
    /// or a `YYYY-MM-DDtoYYYY-MM-DD` range.
    freshness: Option<String>,
    /// Request timeout in milliseconds (default 15000).
    timeout_ms: Option<u64>,
}

fn build_url(args: &SearchArgs) -> ActResult<String> {
    if args.query.trim().is_empty() {
        return Err(ActError::invalid_args("query must not be empty"));
    }
    if let Some(c) = args.count
        && !(1..=20).contains(&c)
    {
        return Err(ActError::invalid_args(format!(
            "count must be between 1 and 20, got {c}"
        )));
    }
    if let Some(o) = args.offset
        && o > 9
    {
        return Err(ActError::invalid_args(format!(
            "offset must be between 0 and 9, got {o}"
        )));
    }

    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("q", &args.query);
    if let Some(v) = args.count {
        q.append_pair("count", &v.to_string());
    }
    if let Some(v) = args.offset {
        q.append_pair("offset", &v.to_string());
    }
    if let Some(v) = &args.country {
        q.append_pair("country", v);
    }
    if let Some(v) = &args.search_lang {
        q.append_pair("search_lang", v);
    }
    if let Some(v) = &args.safesearch {
        q.append_pair("safesearch", v);
    }
    if let Some(v) = &args.freshness {
        q.append_pair("freshness", v);
    }

    Ok(format!("{ENDPOINT}?{}", q.finish()))
}

#[act_component]
mod component {
    use super::*;

    thread_local! {
        /// One entry per open session, holding that session's subscription token.
        static SESSIONS: SessionRegistry<String> = SessionRegistry::new("search-brave");
    }

    /// open-session args: the credential this session searches with.
    #[derive(Deserialize, JsonSchema)]
    pub struct OpenArgs {
        /// Brave Search subscription token (`X-Subscription-Token`).
        api_key: String,
    }

    /// Per-call metadata: the session this call operates on.
    #[derive(Deserialize)]
    pub struct ToolMeta {
        #[serde(rename = "std:session-id")]
        session_id: Option<String>,
    }

    #[session_open]
    fn open(args: OpenArgs) -> ActResult<String> {
        if args.api_key.trim().is_empty() {
            return Err(ActError::invalid_args("api_key must not be empty"));
        }
        Ok(SESSIONS.with(|r| r.insert(args.api_key)))
    }

    #[session_close]
    fn close(session_id: String) {
        SESSIONS.with(|r| {
            r.remove(&session_id);
        });
    }

    #[act_tool(
        description = "Search the web with Brave Search and return ranked results",
        read_only
    )]
    async fn search(
        #[args] args: SearchArgs,
        ctx: &mut ActContext<ToolMeta>,
    ) -> ActResult<SearchResponse> {
        let session_id = ctx.metadata().session_id.clone().ok_or_else(|| {
            ActError::session_not_found(
                "Missing std:session-id metadata — open a session with your Brave API key \
                 first (or pass --session-args '{\"api_key\":\"…\"}' for a one-shot call)",
            )
        })?;
        let api_key = SESSIONS
            .with(|r| r.with(&session_id, |k| k.clone()))
            .ok_or_else(|| {
                ActError::session_not_found(format!("Unknown session-id: {session_id}"))
            })?;

        let url = build_url(&args)?;

        let response = wasi_fetch::Client::new()
            .get(&url)
            .header("X-Subscription-Token", api_key.as_str())
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_millis(
                args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            ))
            .send()
            .await
            .map_err(|e| match e {
                wasi_fetch::Error::Url(msg) => ActError::invalid_args(msg),
                other => ActError::internal(format!("Cannot reach Brave Search: {other}")),
            })?;

        let status = response.status().as_u16();
        let body = response
            .into_body()
            .text()
            .await
            .map_err(|e| ActError::internal(format!("Cannot read Brave response: {e}")))?;

        if !(200..300).contains(&status) {
            return Err(status_error(status, &body));
        }

        let wire: WireResponse = serde_json::from_str(&body)
            .map_err(|e| ActError::internal(format!("Brave response was not valid JSON: {e}")))?;

        let (original, altered) = match wire.query {
            Some(q) => (q.original, q.altered),
            None => (None, None),
        };

        let mut results: Vec<SearchResult> = Vec::new();
        if let Some(web) = wire.web {
            results.extend(web.results.into_iter().map(|r| r.normalise("general")));
        }
        if let Some(news) = wire.news {
            results.extend(news.results.into_iter().map(|r| r.normalise("news")));
        }

        Ok(SearchResponse {
            query: original.unwrap_or_else(|| args.query.clone()),
            results,
            answers: wire
                .faq
                .map(|f| f.results.into_iter().filter_map(|e| e.answer).collect())
                .unwrap_or_default(),
            // Brave reports a spelling correction as a rewritten query rather than as a
            // suggestion list; surface it in the same field the other components use.
            suggestions: altered.into_iter().collect(),
            unresponsive_engines: Vec::new(),
        })
    }
}
