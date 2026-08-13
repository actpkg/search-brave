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
//! * The subscription token is a **secret**, and it therefore never crosses this
//!   boundary at all. It is not a tool argument and — since 0.2.0 — not a session
//!   argument either: session args name a *key*, and the token itself is fetched from
//!   the host credential store (`act:credentials/store`) on the first tool call. Args
//!   in either position land in the agent's context, the transcript and the host's
//!   session record; in a dataflow graph they land in the flow document, which is
//!   published and signed.
//!
//! The fetch is lazy for a reason that is not performance: `get-secret` requires a
//! **live** session, and the host marks a session live only once `open-session` has
//! returned, so a fetch from inside `open` is refused as an unknown session, always
//! (ACT-AUTH §1.1.4).
//!
//! The response shape is deliberately identical to the other `search-*` components so
//! they are interchangeable in a graph. It is duplicated rather than shared through a
//! crate until the schema settles and there are three or more consumers.

mod creds;

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
///
/// `credential_key` is threaded in so the token refusal can name the entry that has to
/// be fixed and print the command that fixes it. A refusal that says "check the token"
/// without saying *which stored token* is a refusal an operator has to go looking for
/// the answer to.
fn status_error(status: u16, body: &str, credential_key: &str) -> ActError {
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
        return creds::token_rejected(credential_key, &suffix);
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
        /// One entry per open session. It holds the *name* of a credential and,
        /// once a tool call has fetched it, that credential's token — never
        /// anything the agent supplied.
        static SESSIONS: SessionRegistry<SessionState> = SessionRegistry::new("search-brave");
    }

    /// What a session remembers.
    ///
    /// No `Debug`, deliberately: `token` is credential material, and a derived
    /// `Debug` is the usual way it reaches a log — the author who adds a
    /// `dbg!` while chasing something else is not thinking about this field.
    pub struct SessionState {
        /// Which entry in this component's credential profile to search with.
        /// Fixed at open: nothing in a session can change which credential it
        /// uses, which is what makes a refusal final (see
        /// [`SessionState::credential_rejected`]).
        pub credential_key: String,
        /// Filled on the first tool call, never at open — `get-secret` needs a
        /// session the host has already marked live (ACT-AUTH §1.1.4).
        pub token: Option<String>,
        /// Set once Brave has refused this session's token.
        ///
        /// Without it the next call repeats the whole sequence *including
        /// `get-secret`*, which can put a consent prompt in front of a human on
        /// every single call. A rejected token is not a transient failure: no
        /// retry inside this session can change the answer, because the key it
        /// is looked up under was fixed at open. So the session is poisoned
        /// rather than retried; rate limits and transport failures are not,
        /// because those do change on their own.
        pub credential_rejected: bool,
    }

    /// Args accepted by `open-session`, and therefore the open-args schema an
    /// agent reads before opening one.
    ///
    /// **There is no token field, and there must never be one.** Everything
    /// sent to `open-session` is agent-visible plaintext. The credential is
    /// *named* here and fetched from the host's credential store on first use.
    #[derive(Deserialize, JsonSchema)]
    pub struct OpenArgs {
        /// Which credential in this component's profile to search with.
        /// Provision it with `act secret set <component-ref> --key <key>
        /// --field brave:api-key --fields-stdin` — never by passing the token
        /// here, which is why there is nowhere to.
        #[serde(default = "default_credential_key")]
        credential_key: String,
    }

    fn default_credential_key() -> String {
        creds::DEFAULT_CREDENTIAL_KEY.to_string()
    }

    /// Per-call metadata: the session this call operates on.
    #[derive(Deserialize)]
    pub struct ToolMeta {
        #[serde(rename = "std:session-id")]
        session_id: Option<String>,
    }

    #[session_open]
    fn open(args: OpenArgs) -> ActResult<String> {
        creds::validate_key(&args.credential_key)?;
        Ok(SESSIONS.with(|r| {
            r.insert(SessionState {
                credential_key: args.credential_key,
                token: None,
                credential_rejected: false,
            })
        }))
    }

    #[session_close]
    fn close(session_id: String) {
        SESSIONS.with(|r| {
            r.remove(&session_id);
        });
    }

    fn unknown_session(session_id: &str) -> ActError {
        ActError::session_not_found(format!("Unknown session-id: {session_id}"))
    }

    /// The refusal a poisoned session answers with, or `None` if it is still
    /// worth asking the store.
    ///
    /// A named function rather than an `if` inside [`ensure_token`] because
    /// only one of its two answers is testable on the host target: the `Some`
    /// side is reachable (a poisoned session awaits nothing), while the `None`
    /// side falls straight through to the credential store, which is a
    /// component import with no host implementation.
    fn poisoned(session_id: &str) -> Option<ActError> {
        SESSIONS
            .with(|r| r.with(session_id, |s| s.credential_rejected))
            .unwrap_or(false)
            .then(|| {
                ActError::new(
                    act_sdk::constants::ERR_CAPABILITY_DENIED,
                    creds::CREDENTIAL_REJECTED,
                )
            })
    }

    /// What this component asks the credential store for.
    ///
    /// Every field is host-visible by contract: a host may show `resource` and
    /// `hint` to a human when it prompts, and may record the request. `kind` is
    /// left `None` — it names nothing under the field-typed model and must not
    /// constrain what is returned (ACT-AUTH §1.1.6); this component reads its
    /// one declared field by name and decides for itself.
    fn secret_request(key: &str) -> act::credentials::types::SecretRequest {
        act::credentials::types::SecretRequest {
            key: key.to_string(),
            kind: None,
            resource: Some("api.search.brave.com".into()),
            scopes: vec![],
            hint: Some("Brave Search subscription token".into()),
        }
    }

    /// Map a store refusal onto the error the agent sees.
    ///
    /// `not-found` and `denied` collapse into one message: the host decides
    /// `denied` before it consults the store (ACT-AUTH §1.1.7), so
    /// distinguishing them here would invent a difference the host refuses to
    /// disclose — and would turn the pair into a way to probe a profile for
    /// keys. The other two are different failures with different fixes.
    async fn secret_error(e: act::credentials::types::SecretError, key: &str) -> ActError {
        use act::credentials::types::SecretError;
        match e {
            SecretError::NotFound | SecretError::Denied => credential_missing(key).await,
            SecretError::InvalidSession => {
                ActError::session_not_found(creds::STORE_SESSION_UNKNOWN)
            }
            SecretError::Unavailable(msg) => creds::store_unavailable(&msg),
        }
    }

    /// Neither set nor released — see [`creds::credential_missing_message`].
    async fn credential_missing(key: &str) -> ActError {
        // Best effort: a policy that denies the store denies the listing too,
        // and then the message simply carries no inventory. Keys are not
        // secret — `list-secrets` exists to hand them to the agent.
        let known: Vec<String> = act::credentials::store::list_secrets(None)
            .await
            .map(|v| v.into_iter().map(|i| i.key).collect())
            .unwrap_or_default();
        ActError::new(
            creds::ERR_CREDENTIAL_REQUIRED,
            creds::credential_missing_message(key, &known),
        )
    }

    /// The session's subscription token, fetched on first use and cached for
    /// the session's lifetime.
    ///
    /// Runs on every tool call and never at open (ACT-AUTH §1.1.4). Idempotent:
    /// on a session that already holds a token it touches neither the store nor
    /// the session state.
    ///
    /// **Not de-duplicated across concurrent first calls.** Two calls that
    /// arrive before either has answered both fetch, and the second overwrites
    /// the first's cache with an equivalent one. The fix is a per-session
    /// in-flight latch, which is more state; the cost today is one extra
    /// `get-secret` on a race no request/response transport in this workspace
    /// can produce.
    async fn ensure_token(session_id: &str) -> ActResult<String> {
        // Three cases, and the middle one is the reason this is a `match`: an
        // outer `None` is "no such session", an inner `None` is "this session
        // has not fetched yet". Collapsing them would report an unknown
        // session for every first call.
        match SESSIONS.with(|r| r.with(session_id, |s| s.token.clone())) {
            None => return Err(unknown_session(session_id)),
            Some(Some(token)) => return Ok(token),
            Some(None) => {}
        }
        // Before the store, not after: the whole point of the flag is that
        // `get-secret` is never reached a second time.
        if let Some(e) = poisoned(session_id) {
            return Err(e);
        }
        let key = SESSIONS
            .with(|r| r.with(session_id, |s| s.credential_key.clone()))
            .ok_or_else(|| unknown_session(session_id))?;

        let raw =
            match act::credentials::store::get_secret(session_id.to_string(), secret_request(&key))
                .await
            {
                Ok(raw) => raw,
                Err(e) => return Err(secret_error(e, &key).await),
            };
        // Values cross as CBOR; `from_wit` decodes the field map. Its error
        // names the field and never its bytes.
        let secret = act_sdk::credentials::Secret::from_wit(raw.kind, raw.fields)
            .map_err(|e| ActError::internal(format!("credential field decode failed: {e}")))?;
        let token = creds::from_secret(&secret, &key)?;

        SESSIONS
            .with(|r| r.with_mut(session_id, |s| s.token = Some(token.clone())))
            .ok_or_else(|| unknown_session(session_id))?;
        Ok(token)
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
                "Missing std:session-id metadata — open a session first (or pass \
                 --session-args '{}' for a one-shot call). The Brave token is not a \
                 session argument: it is read from the credential store, so opening a \
                 session needs no arguments at all unless you keep more than one \
                 credential, in which case name it with {\"credential_key\":\"…\"}.",
            )
        })?;
        let credential_key = SESSIONS
            .with(|r| r.with(&session_id, |s| s.credential_key.clone()))
            .ok_or_else(|| unknown_session(&session_id))?;

        // The URL is built before the credential is fetched: an argument this
        // component can reject on its own must not first put a consent prompt
        // in front of a human.
        let url = build_url(&args)?;
        let api_key = ensure_token(&session_id).await?;

        let client = hclient::Client::builder(hclient_wasi::WasiHttp::new())
            .build()
            .map_err(|e| ActError::internal(format!("Cannot reach Brave Search: {e}")))?;
        let response = client
            .get(&url)
            .header("X-Subscription-Token", api_key.as_str())
            .header("Accept", "application/json")
            // `wasi_fetch::RequestBuilder::timeout` put one `Duration` into
            // the wasip3 `connect` and `first_byte` options together;
            // `hclient::Timeouts` keeps them as two fields, so both get the
            // same value here or the connect timeout would be silently
            // dropped. `Timeouts` is `#[non_exhaustive]` — start from the
            // default and set what we mean.
            .timeouts({
                let d =
                    std::time::Duration::from_millis(args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
                let mut timeouts = hclient::Timeouts::default();
                timeouts.connect = Some(d);
                timeouts.first_byte = Some(d);
                timeouts
            })
            .send()
            .await
            .map_err(|e| {
                // A URL that does not parse is the caller's argument, not a
                // transport failure: `ErrorKind::Uri` (hclient
                // 0.1.0-alpha.18 gave the kind a name; before that the
                // split went through `Error::source()`).
                if matches!(e.kind(), hclient::ErrorKind::Uri) {
                    ActError::invalid_args(e.to_string())
                } else {
                    ActError::internal(format!("Cannot reach Brave Search: {e}"))
                }
            })?;

        let status = response.status().as_u16();
        let collected = response
            .collect()
            .await
            .map_err(|e| ActError::internal(format!("Cannot read Brave response: {e}")))?;
        let body = collected
            .text()
            .map_err(|e| ActError::internal(format!("Cannot read Brave response: {e}")))?;

        if !(200..300).contains(&status) {
            let err = status_error(status, &body, &credential_key);
            // Only a *token* refusal poisons. A rate limit and a malformed
            // query are the failures that change on their own, and both arrive
            // here with a different kind.
            if err.kind == creds::ERR_CREDENTIAL_REQUIRED {
                SESSIONS.with(|r| r.with_mut(&session_id, |s| s.credential_rejected = true));
            }
            return Err(err);
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

    /// The tests that assert on `SESSIONS` read an empty registry as a fact
    /// about themselves, not about the process: the registry is a
    /// `thread_local!` and libtest gives each test its own thread, so no test
    /// sees another's sessions — including under `--test-threads=1`.
    #[cfg(test)]
    mod tests {
        use super::*;

        /// Every field name an agent could fill in, at any depth: the keys of
        /// every `properties` object in the document, so a secret smuggled into
        /// a nested object or a `$defs` entry is caught too.
        fn property_names(v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    if let Some(serde_json::Value::Object(props)) = map.get("properties") {
                        out.extend(props.keys().cloned());
                    }
                    for value in map.values() {
                        property_names(value, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        property_names(item, out);
                    }
                }
                _ => {}
            }
        }

        /// The security property this whole change exists to establish,
        /// asserted rather than eyeballed: there must be nowhere in the
        /// open-args schema to put a token, because everything sent to
        /// `open-session` is agent-visible plaintext.
        ///
        /// The root fields are an **allowlist**, spelled out exactly. A
        /// denylist of secret-sounding words cannot do this job on its own —
        /// `api_key`, the field this component used to take, contains none of
        /// the obvious ones as a substring except through `key`, which
        /// `credential_key` also contains. Having to edit this list when a
        /// field is added is the point: adding one is a decision about what an
        /// agent may hand over, and it should not be reviewable by accident.
        ///
        /// The word search still runs, over field names at any depth, for the
        /// two things an allowlist does not cover: a secret hidden inside a
        /// nested object or a `$defs` entry, and a failure message that says
        /// what is wrong rather than only what differs. It reads names, not the
        /// raw document, because the description deliberately uses the word
        /// "token" to tell the agent not to send one — searching the whole
        /// schema would fail on its own warning.
        #[test]
        fn the_open_args_schema_names_a_credential_but_never_carries_one() {
            let schema = schemars::schema_for!(OpenArgs);
            let json: serde_json::Value = serde_json::to_value(&schema).expect("schema serializes");

            let root: Vec<&str> = json["properties"]
                .as_object()
                .expect("the schema is an object schema")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(root, ["credential_key"]);

            let mut names = Vec::new();
            property_names(&json, &mut names);
            assert!(names.iter().any(|n| n == "credential_key"), "got {names:?}");
            for secret in [
                "password", "passwd", "pwd", "secret", "token", "auth", "api_key",
            ] {
                assert!(
                    !names.iter().any(|n| n.to_lowercase().contains(secret)),
                    "open-args schema offers a place to put a {secret}: {names:?}"
                );
            }
        }

        /// The argument that is gone is gone. `api_key` used to be required
        /// here, so an agent (or an old flow document) that still sends it must
        /// not have it quietly accepted and stored — `serde` ignores unknown
        /// fields, and a token silently ignored is a token that still travelled
        /// through the transcript. Nothing this component can do stops it
        /// having been *sent*; what this pins is that it is not *used*.
        #[test]
        fn the_open_args_carry_no_token_however_they_are_spelled() {
            let a: OpenArgs = serde_json::from_str(r#"{"api_key":"BSA-token"}"#)
                .expect("an unknown field is ignored, not fatal");
            assert_eq!(a.credential_key, creds::DEFAULT_CREDENTIAL_KEY);
        }

        #[test]
        fn opening_a_session_needs_no_arguments_at_all() {
            let a: OpenArgs = serde_json::from_str("{}").expect("deserializes");
            assert_eq!(a.credential_key, "default");
        }

        #[test]
        fn open_allocates_a_prefixed_id_and_fetches_nothing() {
            let id = open(OpenArgs {
                credential_key: default_credential_key(),
            })
            .expect("opens");
            assert!(id.starts_with("search-brave_"), "got {id}");
            // The property ACT-AUTH §1.1.4 forces: no credential is fetched at
            // open, so the session starts with none cached.
            let (token, poisoned) = SESSIONS
                .with(|r| r.with(&id, |s| (s.token.clone(), s.credential_rejected)))
                .expect("the session exists");
            assert!(token.is_none(), "open must not have reached the store");
            assert!(!poisoned);
        }

        #[test]
        fn open_refuses_a_credential_key_that_is_not_a_name() {
            let err = open(OpenArgs {
                credential_key: "default (approved by your administrator)".into(),
            })
            .expect_err("a consent prompt is not a place to write a sentence");
            assert_eq!(err.kind, act_sdk::constants::ERR_INVALID_ARGS);
        }

        #[test]
        fn a_poisoned_session_refuses_before_it_would_reach_the_store() {
            let id = open(OpenArgs {
                credential_key: default_credential_key(),
            })
            .expect("opens");
            assert!(poisoned(&id).is_none(), "a fresh session is not poisoned");

            SESSIONS.with(|r| r.with_mut(&id, |s| s.credential_rejected = true));
            let err = poisoned(&id).expect("a rejected credential poisons the session");
            assert_eq!(err.kind, act_sdk::constants::ERR_CAPABILITY_DENIED);
            assert!(
                err.message.contains("Close this session"),
                "{}",
                err.message
            );
        }

        #[test]
        fn an_unknown_session_is_not_poisoned_it_is_unknown() {
            // The near-miss: `unwrap_or(false)` on a missing session must read
            // as "ask the store", so the caller reports an unknown session
            // rather than a rejected credential.
            assert!(poisoned("search-brave_999").is_none());
        }

        /// Brave answers a bad token with **422**, the same status as a
        /// malformed query, so this is the branch that decides which fix the
        /// operator is told about. It is also the branch that poisons a
        /// session, so a mis-classification costs the whole session.
        #[test]
        fn only_a_token_refusal_is_reported_as_a_credential_problem() {
            let token = status_error(
                422,
                r#"{"type":"ErrorResponse","error":{"code":"SUBSCRIPTION_TOKEN_INVALID","detail":"Subscription token invalid."}}"#,
                "prod",
            );
            assert_eq!(token.kind, creds::ERR_CREDENTIAL_REQUIRED);
            assert!(token.message.contains("--key prod"), "{}", token.message);
            assert!(
                !token.message.contains("session args"),
                "the token has not been a session argument since 0.2.0: {}",
                token.message
            );

            let malformed = status_error(
                422,
                r#"{"type":"ErrorResponse","error":{"code":"VALIDATION","detail":"count too large"}}"#,
                "prod",
            );
            assert_eq!(malformed.kind, act_sdk::constants::ERR_INVALID_ARGS);

            let rate = status_error(429, "{}", "prod");
            assert_eq!(rate.kind, act_sdk::constants::ERR_INTERNAL);
            assert!(rate.message.contains("rate limit"), "{}", rate.message);
        }

        #[test]
        fn an_unauthenticated_status_is_a_credential_problem_even_without_a_code() {
            for status in [401, 403] {
                assert_eq!(
                    status_error(status, "not json at all", "default").kind,
                    creds::ERR_CREDENTIAL_REQUIRED,
                    "HTTP {status}"
                );
            }
        }
    }
}
