//! Drive the packed component through `act run --mcp` with a real MCP client.
//!
//! Rust translation of the python fastmcp/pytest suite that still lives in
//! this directory (kept for reference); the tests observe exactly what an
//! agent observes, over the same client stack (`rmcp`) the host bridge itself
//! is built on.
//!
//! search-brave is a session-provider, but the Brave subscription token is no
//! longer carried by a session argument: since 0.2.0 the session names a
//! credential *key* and the token itself is fetched from the host credential
//! store on the first tool call — `get-secret` needs a session the host has
//! already marked live, so the fetch cannot happen inside `open-session` at
//! all (ACT-AUTH §1.1.4). Every real `search` call needs `std:session-id` in
//! its request `_meta`, taken here from the virtual
//! `open_session`/`close_session` tools the MCP adapter synthesises for any
//! session-provider component — the path an agent actually uses — rather than
//! from a `--session-args` session-of-1, which would hide the session
//! machinery this suite exists to test (closing a session, calling with none
//! at all). Session-of-1 itself is covered by act-cli's own
//! `tests/session_of_1_mcp.rs` suite.
//!
//! No live Brave API key or endpoint is used anywhere in the offline suite —
//! see the module comment above `test_a_real_search_returns_the_normalised_shape`
//! for the one test that does.
//!
//! Env: WASM — path to the packed component (default: the component's
//!      release build output);
//!      ACT  — the act invocation (default `act`; the component justfile's
//!             `npx @actcore/act`, two words, also works — whitespace-split,
//!             like the shlex.split the python conftest did);
//!      BRAVE_API_KEY — the live test.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex as AsyncMutex;

/// `().serve(transport)` hands back the client-role service running over the
/// child process: role first, the unit client handler second.
type Client = rmcp::service::RunningService<rmcp::service::RoleClient, ()>;

/// Deliberately loose relative to what a lightweight component like
/// search-brave actually costs: `act run --mcp` instantiates the component
/// before it answers `initialize`, so "connect" includes that cost, and it
/// varies on a loaded runner. (The python conftest sits at 120s because the
/// same constant serves servo's suite, where a healthy connect is ~8s;
/// search-brave instantiates in well under a second.) The bound exists to
/// give a clear "did not connect" diagnostic instead of a stalled handshake
/// with none.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

fn wasm_path() -> PathBuf {
    let path = PathBuf::from(std::env::var("WASM").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/wasm32-wasip2/release/component_search_brave.wasm"
        )
        .into()
    }));
    ensure_packed(&path);
    path
}

/// The python `wasm_path` fixture's probe, run once per process. Existence is
/// not enough and neither is a fresh mtime: `cargo build` produces a wasm
/// with no `act:component` custom section, and an unpacked artifact declares
/// no capability ceiling, so every grant is refused as "outside ceiling" and
/// the failures point anywhere but here. This has already bitten this
/// workspace repeatedly. The justfile's `test: build` ordering exists so
/// this check passes.
fn ensure_packed(path: &Path) {
    static CHECKED: OnceLock<()> = OnceLock::new();
    CHECKED.get_or_init(|| {
        if !path.exists() {
            panic!("{path:?} is missing — run `just build && just pack` first");
        }
        let mut cmd = new_std_command();
        cmd.args(["inspect", "component-manifest"]).arg(path);
        let output = cmd.output().expect("run act inspect component-manifest");
        let manifest: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        let name = manifest["std"]["name"].as_str().unwrap_or("unknown");
        if name.is_empty() || name == "unknown" {
            panic!("{path:?} is built but not packed — run `just pack`");
        }
    });
}

/// The ACT invocation, honouring the same override the component justfile
/// uses. Its default there is `npx @actcore/act` — two words — which cannot
/// be `argv[0]` for a non-shell spawn, so the value is whitespace-split into
/// program + leading args. Quoted paths with spaces are not a form this
/// fleet passes through `ACT`; a full shlex is deliberately not pulled in.
fn act_argv() -> Vec<String> {
    std::env::var("ACT")
        .unwrap_or_else(|_| "act".into())
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn new_std_command() -> std::process::Command {
    let argv = act_argv();
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

/// Spawn `act run <wasm> --mcp` with the grants this component needs.
///
/// Grants are NOT optional: the default policy mode is `ask` and a headless
/// run degrades it to deny. `--allow wasi:http --allow act:credentials` opens
/// exactly the classes the component declares and nothing wider: its own
/// ceiling already scopes `wasi:http` to `api.search.brave.com` (act.toml,
/// fixed at build time) and `act:credentials` is a bare table. Every test
/// needs both (even the ones that never reach the network or the store:
/// session validation happens before either, but the grants are checked at
/// component start, not per-call), so they are unconditional here rather than
/// per-test.
///
/// No `--credentials-backend`, so this spawn reads the platform's default
/// store — exactly what the python suite's shared `client` fixture did, and
/// nothing using it reaches `get-secret` (every offline failure fires before
/// the store is consulted; `build_url` runs before `ensure_token`). The one
/// path that does reach it brings its own, empty store — see
/// [`empty_credential_backend`].
fn act_command() -> tokio::process::Command {
    act_command_with_store(None)
}

fn act_command_with_store(backend: Option<&str>) -> tokio::process::Command {
    let argv = act_argv();
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.arg("run").arg(wasm_path()).arg("--mcp");
    cmd.args(["--allow", "wasi:http", "--allow", "act:credentials"]);
    if let Some(backend) = backend {
        cmd.args(["--credentials-backend", backend]);
    }
    cmd
}

/// Spawn with stderr captured: the audit trail (refusals, per-call rollup)
/// writes there unconditionally — RUST_LOG never silences it — and the
/// python conftest redirected it to a log file to keep it out of the test
/// output. The [`ActStderr`] guard reprints it when a test fails, which is
/// that suite's `pytest_sessionfinish` hook's job.
fn spawn_with_captured_stderr_in(
    backend: Option<&str>,
) -> (TokioChildProcess, Arc<AsyncMutex<String>>) {
    let (transport, stderr) = TokioChildProcess::builder(act_command_with_store(backend))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn act run --mcp with piped stderr");

    let captured = Arc::new(AsyncMutex::new(String::new()));
    let sink = captured.clone();
    let mut lines = BufReader::new(stderr.expect("stderr was piped")).lines();
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            let mut buf = sink.lock().await;
            buf.push_str(&line);
            buf.push('\n');
        }
    });

    (transport, captured)
}

/// Reprints the captured audit trail if the test is unwinding — on an
/// ephemeral CI runner nothing would otherwise ever read it. Diagnosing a
/// CI-only hang in this fleet cost several rounds of probing that one line
/// of this stream would have answered.
struct ActStderr(Arc<AsyncMutex<String>>);

impl Drop for ActStderr {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        // The drain task holds the lock only per line; a short retry is
        // enough to catch a quiet moment.
        for _ in 0..20 {
            if let Ok(buf) = self.0.try_lock() {
                eprintln!("--- act stderr ---\n{}", buf);
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!("--- act stderr: buffer busy, not dumped ---");
    }
}

/// A connected MCP client, one `act` process per test: a session opened in
/// one test must not leak into the next. The connect — not the test body —
/// is bounded, so a stalled handshake produces a diagnostic of its own
/// instead of consuming the whole test timeout silently.
async fn connect() -> (Client, ActStderr) {
    connect_in(None).await
}

/// The python `test_credentials.py` client override: same two grants, plus a
/// store whose contents this suite controls.
async fn connect_in(backend: Option<&str>) -> (Client, ActStderr) {
    let (transport, captured) = spawn_with_captured_stderr_in(backend);
    let client = tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport))
        .await
        .expect("MCP client did not connect within 30s; act's stderr is dumped by the failure guard")
        .expect("rmcp handshake with act run --mcp");
    (client, ActStderr(captured))
}

fn text_blocks(result: &rmcp::model::CallToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .filter_map(|b| match b {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

fn first_text_block(result: &rmcp::model::CallToolResult) -> &rmcp::model::TextContent {
    match result.content.first() {
        Some(rmcp::model::ContentBlock::Text(t)) => t,
        other => panic!("expected the first content block to be Text, got: {other:?}"),
    }
}

/// A tool-call request. `session_id` rides in request `_meta` — the same
/// channel fastmcp's `call_tool(meta=…)` kwarg used, not a key inside
/// `arguments`. That channel keeps its `std:` spelling verbatim; only
/// *response* metadata (`dev.actcore/error-kind` itself) gets the
/// `dev.actcore/` respelling.
fn tool_params(
    tool: &str,
    arguments: Value,
    session_id: Option<&str>,
) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(tool.to_string());
    if let Some(map) = arguments.as_object() {
        params = params.with_arguments(map.clone());
    }
    if let Some(sid) = session_id {
        params.meta = Some(rmcp::model::RequestMetaObject(rmcp::model::MetaObject(
            json!({ "std:session-id": sid })
                .as_object()
                .expect("session meta is an object")
                .clone(),
        )));
    }
    params
}

/// The kind and message of a failed call may arrive on either path: as a
/// JSON-RPC error response (`ErrorData.data` / `message`) or as an isError
/// result (`_meta` / first text content). The python conftest's
/// `expect_error` fixture handled both; so does this. `call-tool` has no
/// `result<>` wrapper, so a guest reporting a failed call can only do it
/// through `tool-event::error` — the isError path, which is where search's
/// own session/argument validation lands (ordinary guest tool-body logic,
/// not a host-enforced gate) — while the JSON-RPC path exists for failures
/// that are not the guest's tool body: `list-tools`, the session
/// *operations themselves* (`open_session` with a malformed `credential_key`
/// fails there: the bridge propagates the guest's error with `?`, which rmcp
/// turns into a JSON-RPC error), a wasmtime trap, an unreachable actor.
async fn error_kind_of(
    client: &Client,
    params: CallToolRequestParams,
) -> Option<(String, String)> {
    match client.call_tool(params).await {
        Err(rmcp::ServiceError::McpError(e)) => {
            let kind = e
                .data
                .as_ref()
                .and_then(|d| d.get("dev.actcore/error-kind"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            kind.map(|k| (k, e.message.to_string()))
        }
        Ok(result) => {
            assert_eq!(result.is_error, Some(true), "call must fail: {result:?}");
            let kind = result
                .meta
                .as_ref()
                .and_then(|m| m.0.get("dev.actcore/error-kind"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let message = text_blocks(&result).into_iter().next().unwrap_or_default();
            kind.map(|k| (k, message))
        }
        Err(other) => panic!("unexpected transport failure: {other:?}"),
    }
}

/// The python conftest's `expect_error` fixture: assert a call fails with a
/// specific ACT error kind (and, optionally, a substring of its
/// human-readable message).
async fn expect_error(
    client: &Client,
    tool: &str,
    arguments: Value,
    kind: &str,
    contains: Option<&str>,
    session_id: Option<&str>,
) {
    let params = tool_params(tool, arguments, session_id);
    let Some((actual_kind, message)) = error_kind_of(client, params).await else {
        panic!("expected {tool} to fail with {kind}, but no named error kind came back");
    };
    assert_eq!(
        actual_kind, kind,
        "expected {kind}, got {actual_kind} ({message:?})"
    );
    if let Some(needle) = contains {
        assert!(message.contains(needle), "expected {needle:?} in {message:?}");
    }
}

async fn open_session(client: &Client, arguments: Value) -> String {
    let result = client
        .call_tool(tool_params("open_session", arguments, None))
        .await
        .expect("call open_session");
    assert_ne!(result.is_error, Some(true), "open_session failed: {result:?}");
    // The id arrives in `content[0].text` as JSON, not `structured_content`:
    // the virtual session tools build their `CallToolResult` by hand in
    // `rmcp_bridge.rs` and bypass the normal content-part folding that would
    // otherwise structure a lone JSON object.
    let reply: Value =
        serde_json::from_str(&first_text_block(&result).text).expect("reply is JSON");
    reply["id"]
        .as_str()
        .expect("open_session reply carries an id")
        .to_string()
}

async fn close_session(client: &Client, session_id: &str) {
    let result = client
        .call_tool(tool_params(
            "close_session",
            json!({ "session_id": session_id }),
            None,
        ))
        .await
        .expect("call close_session");
    assert_ne!(result.is_error, Some(true), "close_session failed: {result:?}");
}

/// The python `session` fixture: a per-test session opened with no arguments
/// at all and closed by the test's last line. No arguments is the whole
/// point: the token is not one, and naming a `credential_key` is only needed
/// when a deployment keeps more than one credential. Opening a session
/// touches neither the store nor the network.
async fn open_default_session(client: &Client) -> String {
    open_session(client, json!({})).await
}

fn tool_named<'a>(tools: &'a [rmcp::model::Tool], name: &str) -> &'a rmcp::model::Tool {
    tools
        .iter()
        .find(|t| t.name.as_ref() == name)
        .unwrap_or_else(|| {
            panic!(
                "no `{name}` tool in {:?}",
                tools.iter().map(|t| t.name.to_string()).collect::<Vec<_>>()
            )
        })
}

fn schema_properties(tool: &rmcp::model::Tool) -> serde_json::Map<String, Value> {
    tool.input_schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn schema_required(tool: &rmcp::model::Tool) -> Vec<String> {
    // The python original read `schema.get("required", [])`: the key is
    // optional, and a schema without it simply has no required args.
    tool.input_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().expect("required entries are strings").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The python `empty_credential_store` fixture: a `--credentials-backend`
/// argument naming a store with nothing in it.
///
/// An empty store rather than no store at all: without this the host reads
/// the platform's default credential store, and whether *that* holds a
/// search-brave token is a property of the machine running the suite, not of
/// the component. Empty is the deterministic version of the same question.
fn empty_credential_backend() -> &'static str {
    static BACKEND: OnceLock<String> = OnceLock::new();
    BACKEND.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "search-brave-e2e-credentials-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock is set")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create the empty credential store");
        format!("file:{}", dir.display())
    })
    .as_str()
}

/// The python fixture's only skip: an `act` with no credential store has no
/// feature under test here. Everything past this check is a regression.
fn credentials_backend_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let mut cmd = new_std_command();
        cmd.args(["secret", "--help"]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// The python test's `re.search(r"search-brave_\d+", session)` — an
/// *unanchored* search, not a full match (the original pattern carries no
/// `^`/`$` of its own either, so it is kept exactly as written rather than
/// anchored).
fn has_prefixed_numeric_id(s: &str) -> bool {
    const PREFIX: &str = "search-brave_";
    s.match_indices(PREFIX)
        .any(|(pos, _)| s[pos + PREFIX.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

// --- test_info.py -----------------------------------------------------------

#[test]
fn test_manifest_reports_name_and_version() {
    let wasm = wasm_path();
    let mut cmd = new_std_command();
    cmd.args(["inspect", "component-manifest"]).arg(&wasm);
    let output = cmd.output().expect("run act inspect component-manifest");
    assert!(
        output.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: Value =
        serde_json::from_slice(&output.stdout).expect("manifest is JSON");
    assert_eq!(
        manifest["std"]["name"], "search-brave",
        "packed manifest must carry the component name"
    );
    assert!(
        manifest["std"]["version"].is_string(),
        "packed manifest must carry a version, got: {}",
        manifest["std"]["version"]
    );
}

// --- test_tools.py ----------------------------------------------------------

#[tokio::test]
async fn test_component_exposes_its_tools() {
    let (client, _stderr) = connect().await;
    let tools = client.list_all_tools().await.expect("list_all_tools");
    assert!(
        !tools.is_empty(),
        "component must expose at least one tool"
    );
    // `search`, plus the virtual session tools the MCP adapter synthesises
    // for any session-provider component (ACT-SESSIONS §6.1) — the suite
    // drives the session path through exactly these.
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in ["search", "open_session", "close_session"] {
        assert!(
            names.contains(&expected),
            "`{expected}` must be among the tools, got: {names:?}"
        );
    }
    client.cancel().await.ok();
}

// --- test_search.py ---------------------------------------------------------
//
// search's session/argument validation, end to end.
//
// No live Brave API key or endpoint is used anywhere in this section. Every
// scenario here fails before any network call would happen, and before the
// credential store is consulted at all: schema checks, session-presence and
// lifecycle checks (the guest's own `ctx.metadata()` logic — src/lib.rs),
// and argument validation in `build_url`, which runs *before* `ensure_token`
// and therefore before the outbound call. That ordering is deliberate and is
// what keeps these tests independent of whatever the host's default
// credential store happens to hold: an argument the component can reject on
// its own must not first put a consent prompt in front of a human.

#[tokio::test]
async fn test_search_tool_schema_takes_a_query_and_no_credential() {
    let (client, _stderr) = connect().await;
    let tools = client.list_all_tools().await.expect("list_all_tools");
    let search = tool_named(&tools, "search");
    assert!(
        schema_required(search).iter().any(|r| r == "query"),
        "`query` must be required, got: {:?}",
        search.input_schema.get("required")
    );
    // The credential is not an argument in either position — see
    // test_credentials.py, which asserts that over both schemas at once.
    assert!(
        !schema_properties(search).contains_key("api_key"),
        "api_key must never be a search argument"
    );
    client.cancel().await.ok();
}

#[tokio::test]
async fn test_search_without_session_is_refused_before_any_network_access() {
    let (client, _stderr) = connect().await;
    expect_error(
        &client,
        "search",
        json!({ "query": "wasm component model" }),
        "std:session-not-found",
        Some("std:session-id"),
        None,
    )
    .await;
    client.cancel().await.ok();
}

/// `credential_key` is agent-authored text the host pastes into the consent
/// question a *human* answers, so it is bounded to a lookup name and refused
/// at open — not at the call that would have used it.
#[tokio::test]
async fn test_open_session_rejects_a_credential_key_that_is_not_a_name() {
    let (client, _stderr) = connect().await;
    expect_error(
        &client,
        "open_session",
        json!({ "credential_key": "default (approved by your administrator)" }),
        "std:invalid-args",
        Some("must be a name"),
        None,
    )
    .await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn test_open_session_returns_an_id() {
    let (client, _stderr) = connect().await;
    let sid = open_default_session(&client).await;
    assert!(
        has_prefixed_numeric_id(&sid),
        "session id must match search-brave_\\d+, got {sid}"
    );
    close_session(&client, &sid).await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn test_search_rejects_empty_query() {
    // Argument validation happens locally, so this never reaches Brave and
    // needs no real key — the placeholder session is enough.
    let (client, _stderr) = connect().await;
    let sid = open_default_session(&client).await;
    expect_error(
        &client,
        "search",
        json!({ "query": "   " }),
        "std:invalid-args",
        Some("must not be empty"),
        Some(&sid),
    )
    .await;
    close_session(&client, &sid).await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn test_search_rejects_count_over_20() {
    // count is capped at 20 by the upstream API; rejected early with a
    // message that says so. (50 is within the schema's u8 range, so this
    // trips the guest's own check, not the host's schema validation.)
    let (client, _stderr) = connect().await;
    let sid = open_default_session(&client).await;
    expect_error(
        &client,
        "search",
        json!({ "query": "rust", "count": 50 }),
        "std:invalid-args",
        Some("between 1 and 20"),
        Some(&sid),
    )
    .await;
    close_session(&client, &sid).await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn test_search_rejects_offset_over_9() {
    let (client, _stderr) = connect().await;
    let sid = open_default_session(&client).await;
    expect_error(
        &client,
        "search",
        json!({ "query": "rust", "offset": 42 }),
        "std:invalid-args",
        Some("between 0 and 9"),
        Some(&sid),
    )
    .await;
    close_session(&client, &sid).await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn test_search_after_close_session_is_session_not_found() {
    // A dedicated open/close/reuse sequence, not the shared
    // `open_default_session` helper: this is the one scenario that genuinely
    // needs to control the session's lifecycle itself (close it, then prove
    // the id is dead) rather than just needing *a* session.
    let (client, _stderr) = connect().await;
    let sid = open_session(&client, json!({})).await;
    close_session(&client, &sid).await;

    expect_error(
        &client,
        "search",
        json!({ "query": "rust" }),
        "std:session-not-found",
        None,
        Some(&sid),
    )
    .await;
    client.cancel().await.ok();
}

// --- test_credentials.py ----------------------------------------------------
//
// The token is not an argument — not a tool one, not a session one — and the
// component reads it from the host credential store instead.
//
// Nothing here uses a live Brave token or reaches the network. Two of the
// three assertions inspect published schemas; the third drives the real
// `get-secret` path against a store this section creates and deliberately
// leaves **empty**, which is the one branch of that path reachable without a
// subscription: the refusal an operator actually meets, and the command it
// hands them.

#[tokio::test]
async fn test_no_schema_offers_a_place_to_put_the_token() {
    // The property this whole design exists to protect, asserted over what an
    // agent can actually see: both schemas at once, since a credential
    // removed from one and left in the other has not gone anywhere.
    let (client, _stderr) = connect_in(Some(empty_credential_backend())).await;
    let tools = client.list_all_tools().await.expect("list_all_tools");
    for name in ["search", "open_session"] {
        let props = schema_properties(tool_named(&tools, name));
        for forbidden in ["api_key", "token", "secret", "password", "brave:api-key"] {
            assert!(
                !props.contains_key(forbidden),
                "{forbidden} must never be a {name} argument"
            );
        }
    }
    client.cancel().await.ok();
}

#[tokio::test]
async fn test_open_session_publishes_the_credential_key_and_nothing_else() {
    let (client, _stderr) = connect_in(Some(empty_credential_backend())).await;
    let tools = client.list_all_tools().await.expect("list_all_tools");
    let schema = tool_named(&tools, "open_session").input_schema.clone();
    assert_eq!(
        schema.get("type").and_then(Value::as_str),
        Some("object"),
        "open_session takes an object, got: {schema:?}"
    );
    let props = schema_properties(tool_named(&tools, "open_session"));
    let names: Vec<&str> = props.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["credential_key"],
        "open_session takes exactly a credential name, got: {names:?}"
    );
    // Optional: a deployment with one credential opens a session with `{}`.
    assert!(
        !schema_required(tool_named(&tools, "open_session"))
            .iter()
            .any(|r| r == "credential_key"),
        "credential_key must be optional, got: {:?}",
        schema.get("required")
    );
    client.cancel().await.ok();
}

/// The real `get-secret` path, all the way to the store and back.
///
/// `not-found` and `denied` are one error on purpose (ACT-AUTH §1.1.7), so
/// this asserts the kind and the fix rather than the cause — and the fix has
/// to be runnable as printed, which is the part that rots silently.
#[tokio::test]
async fn test_search_without_a_stored_token_names_the_command_that_stores_one() {
    if !credentials_backend_supported() {
        eprintln!(
            "skipping: this `act` has no credential store (`act secret`); nothing to drive"
        );
        return;
    }
    let (client, _stderr) = connect_in(Some(empty_credential_backend())).await;
    let sid = open_default_session(&client).await;

    expect_error(
        &client,
        "search",
        json!({ "query": "wasm component model" }),
        "std:credential-required",
        Some("act secret set <component-ref> --key default --field brave:api-key --fields-stdin"),
        Some(&sid),
    )
    .await;

    close_session(&client, &sid).await;
    client.cancel().await.ok();
}

// --- live -------------------------------------------------------------------
//
// The one test that talks to Brave, and it skips unless a token is
// configured.
//
// needs: BRAVE_API_KEY — a Brave **Web Search** subscription token from
//        api-dashboard.search.brave.com (a token for another Brave product
//        is refused with the same 422).
//
// Everything else in this suite is deliberately offline: the offline tests
// can prove the component refuses correctly and cannot prove it *searches*
// correctly — no fixture can show that the request Brave actually accepts is
// the one this component builds, or that the normalised shape survives real
// upstream payloads.
//
// The token is written into a throwaway credential store (`--credentials-backend
// file:<dir>`, the same flag `act run` gets) rather than the developer's own
// store, and it is never passed as an argument, because the component has
// nowhere to accept one.
//
// **Each run is billed.** Brave's free tier allows one request per second and
// 2,000 per month.

fn live_api_key() -> Option<String> {
    match std::env::var("BRAVE_API_KEY") {
        Ok(key) if !key.is_empty() => Some(key),
        _ => {
            eprintln!(
                "skipping live test: no BRAVE_API_KEY in the environment; \
                 the live path is not exercised"
            );
            None
        }
    }
}

/// Store the token in a throwaway credential store and hand the backend
/// argument back so the test's `act run` reads the same store.
///
/// `act secret set` reads the field values from stdin, so the token never
/// appears in a command line, in the child's environment, or in this test's
/// output.
fn provision_live_credential(api_key: &str, tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "search-brave-e2e-live-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create the live credential store");
    let backend = format!("file:{}", dir.display());
    let payload = json!({ "brave:api-key": api_key }).to_string();

    let argv = act_argv();
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.arg("secret").args(["--credentials-backend", &backend]);
    cmd.arg("set").arg(wasm_path());
    cmd.args([
        "--key",
        "default",
        "--field",
        "brave:api-key",
        "--fields-stdin",
    ]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn act secret set");
    // ChildStdin is a std::io::Write, not tokio's.
    use std::io::Write as _;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(payload.as_bytes())
        .expect("write the credential payload");
    let output = child.wait_with_output().expect("act secret set");
    assert!(
        output.status.success(),
        "act secret set failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    backend
}

#[tokio::test]
async fn test_a_real_search_returns_the_normalised_shape() {
    let Some(api_key) = live_api_key() else { return };
    let backend = provision_live_credential(&api_key, "normalised-shape");
    let (client, _stderr) = connect_in(Some(&backend)).await;
    let sid = open_default_session(&client).await;

    let result = client
        .call_tool(tool_params(
            "search",
            json!({
                "query": "wasm component model",
                "count": 5,
            }),
            Some(&sid),
        ))
        .await
        .expect("call search");
    assert_ne!(
        result.is_error,
        Some(true),
        "search failed: {}",
        first_text_block(&result).text
    );
    let payload: Value =
        serde_json::from_str(&first_text_block(&result).text).expect("reply is JSON");

    let mut keys: Vec<&str> = payload
        .as_object()
        .expect("reply is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["answers", "query", "results", "suggestions", "unresponsive_engines"],
        "the normalised shape must carry exactly these keys: {payload}"
    );
    assert!(
        payload["query"].as_str().map_or(false, |q| !q.is_empty()),
        "the query as understood by the engine must be a non-empty string: {payload}"
    );
    let rows = payload["results"].as_array().expect("results is a list");
    assert!(
        !rows.is_empty(),
        "a live search for a common phrase returned nothing"
    );
    for row in rows {
        assert!(
            row["url"].as_str().expect("row url").starts_with("http"),
            "url must start with http, got: {row}"
        );
        assert_eq!(row["engine"], "brave", "row: {row}");
        // Web results are `general`; Brave may also return news hits for a
        // news-y query, normalised to `news` (src/lib.rs).
        assert!(
            matches!(row["category"].as_str(), Some("general") | Some("news")),
            "category must be a normalised section, row: {row}"
        );
        assert!(row["score"].is_null(), "score must be null, row: {row}");
    }
    // Always empty for this component — `unresponsive_engines` is a field the
    // normalised shape carries for the multi-engine components.
    assert_eq!(
        payload["unresponsive_engines"]
            .as_array()
            .expect("unresponsive_engines is a list")
            .len(),
        0
    );

    close_session(&client, &sid).await;
    client.cancel().await.ok();
}
