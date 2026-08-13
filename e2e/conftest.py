"""Shared fixtures for the MCP-driven e2e suite.

The suite drives the packed component through `act run --mcp` over stdio with
a real MCP client, so what the tests observe is what an agent observes.

search-brave is a session-provider: the Brave subscription token is a secret
that travels via session args, never tool arguments (see src/lib.rs's module
docs — a dataflow node's `params` live in a published, signed flow document,
so a credential must never go there). Every real `search` call needs
`std:session-id` in its metadata. This suite gets that id from the virtual
`open_session`/`close_session` tools the MCP adapter synthesises for any
session-provider component — the path an agent actually uses — rather than
starting the host with `--session-args` (session-of-1), which would hide the
session machinery this suite exists to test (closing a session, calling with
none at all). Session-of-1 itself is covered by act-cli's own
`tests/session_of_1_mcp.rs` suite.

No live Brave API key or endpoint is used anywhere in this suite — see the
module docstring on `test_search.py` for what that does and does not cover.
"""

import asyncio
import json
import os
import shlex
import subprocess
import pytest
from contextlib import AsyncExitStack
from pathlib import Path

from fastmcp import Client
from fastmcp.client.transports import StdioTransport

# Measured in docs/specs/2026-08-08-e2e-harness-findings.md, question 1.
from mcp.shared.exceptions import McpError

WASM = "target/wasm32-wasip2/release/component_search_brave.wasm"

# ACT's audit trail writes to stderr unconditionally — it is not governed by
# RUST_LOG — so it is redirected to a file rather than left to flood pytest.
LOG_FILE = Path(".pytest-act-stderr.log")

# Deliberately loose. `act run --mcp` instantiates the component before it
# answers `initialize`, so "connect" includes that cost -- for a heavy
# component (servo embeds a browser engine) it is seconds, and on a loaded
# runner it varies. 30s tripped servo in CI while its healthy connect was
# ~8s, so the bound sits well above the worst observed cost and still well
# below the per-test timeout, keeping this the diagnostic that fires first.
CONNECT_TIMEOUT = 120


@pytest.fixture(scope="session")
def act_command() -> list[str]:
    """The ACT invocation, honouring the same override the justfile uses.

    Parsed with shlex, not treated as a single path: the justfile's own
    default for its `act` variable is `npx @actcore/act` — two words — which
    cannot be `argv[0]` for a non-shell `subprocess.run`/`StdioTransport`
    call. A bare `os.environ.get("ACT", "act")` string breaks that default;
    splitting it is what makes both forms ("act" on PATH, and the npx
    two-word default) actually spawn.
    """
    return shlex.split(os.environ.get("ACT", "act"))


@pytest.fixture(scope="session")
def wasm_path(act_command: list[str]) -> Path:
    """The packed component.

    Existence is not enough and neither is a fresh mtime: `cargo build`
    produces a wasm with no `act:component` custom section, and an unpacked
    artifact declares no capability ceiling, so every grant is refused as
    "outside ceiling" and the failures point anywhere but here. This has
    already bitten this workspace repeatedly, so the fixture checks the
    section rather than the file.
    """
    path = Path(WASM)
    if not path.exists():
        pytest.fail(f"{path} is missing — run `just build && just pack` first")
    probe = subprocess.run(
        [*act_command, "inspect", "component-manifest", str(path)],
        capture_output=True, text=True,
    )
    name = json.loads(probe.stdout or "{}").get("std", {}).get("name", "unknown")
    if name in ("", "unknown"):
        pytest.fail(f"{path} is built but not packed — run `just pack`")
    return path


@pytest.fixture
async def client(act_command: list[str], wasm_path: Path):
    """A connected MCP client, one `act` process per test.

    `--allow wasi:http` moved here verbatim from the old justfile's `grants`
    variable — the component's own ceiling already scopes it to
    `api.search.brave.com` (act.toml), so opening the class grants exactly
    that host, nothing wider. Every test needs it (even the ones that never
    reach the network: session validation happens before any HTTP call, but
    the grant is checked at component start, not per-call), so it is
    unconditional here rather than per-test.

    Function-scoped, one client per test: a session opened in one test must
    not leak into the next.
    """
    transport = StdioTransport(
        command=act_command[0],
        args=[*act_command[1:], "run", str(wasm_path), "--mcp", "--allow", "wasi:http"],
        keep_alive=False,
        log_file=LOG_FILE,
    )
    async with AsyncExitStack() as stack:
        # Bound the connect, not the test body. A stalled handshake otherwise
        # consumes the whole pytest timeout with no diagnostic at all — which
        # is precisely how the webdriver-bidi CI hang presented for hours.
        try:
            async with asyncio.timeout(CONNECT_TIMEOUT):
                connected = await stack.enter_async_context(Client(transport))
        except TimeoutError:
            pytest.fail(
                f"MCP client did not connect within {CONNECT_TIMEOUT}s; "
                f"act's stderr, if it wrote any, is dumped at session end"
            )
        yield connected


@pytest.fixture
async def session(client) -> str:
    """A per-test session, opened via the virtual `open_session` tool with a
    placeholder credential and closed after the test.

    The placeholder is never validated against Brave — see `test_search.py`'s
    module docstring — so any non-empty string does the job; every test that
    uses this fixture is exercising session-scoped *validation*, not a real
    search. The id arrives in `content[0].text` as JSON, not
    `structured_content`: the virtual session tools build their
    `CallToolResult` by hand in `rmcp_bridge.rs` and bypass the normal
    content-part folding that would otherwise structure a lone JSON object.
    """
    opened = await client.call_tool("open_session", {"api_key": "e2e-placeholder-token"})
    sid = json.loads(opened.content[0].text)["id"]
    yield sid
    await client.call_tool("close_session", {"session_id": sid})


@pytest.fixture
def expect_error():
    """Assert a call fails with a specific ACT error kind (and, optionally, a
    substring of its human-readable message).

    Exposed as a fixture rather than a plain function so tests never have to
    import from `conftest` — that import only resolves when the test
    directory happens to be on `sys.path`, which is not something to rely on.

    Measured, not assumed. `call-tool` in `act:tools` returns a bare
    `tool-result` with NO `result<>` wrapper — only `list-tools` has one — so
    a guest reporting a failed tool call can only do it through
    `tool-event::error`, which arrives as a result with `is_error` set and the
    kind in `_meta`, and the message as its one text content part. **That is
    the path search's own session/argument validation takes** — it is
    ordinary guest tool-body logic (see src/lib.rs's `ctx.metadata()` check),
    not a host-enforced gate.

    The JSON-RPC error path exists for failures that are not the guest's tool
    body: `list-tools`, the session *operations themselves* (`open_session`
    with an empty `api_key` fails here — confirmed empirically:
    `virtual_open_session` in rmcp_bridge.rs propagates the guest's error
    with `?`, which rmcp turns into a JSON-RPC error, not an isError result),
    a wasmtime trap, an unreachable actor. It raises
    `mcp.shared.exceptions.McpError`, with the kind at `exc.error.data` and
    the message at `exc.error.message`. Both are handled here so callers need
    not care which one fires.

    `meta` carries `std:session-id` (and anything else) as ordinary MCP
    request `_meta` — the same channel `fastmcp.Client.call_tool`'s own
    `meta=` kwarg uses, not a key inside `arguments`. That channel keeps its
    `std:` spelling verbatim; only *response* metadata (`.meta` on the
    result, e.g. `dev.actcore/error-kind` itself) gets the `dev.actcore/`
    respelling.
    """

    async def _expect(
        client, tool: str, arguments: dict, kind: str,
        contains: str | None = None, meta: dict | None = None,
    ):
        try:
            result = await client.call_tool(tool, arguments, meta=meta, raise_on_error=False)
        except McpError as exc:
            data = getattr(getattr(exc, "error", None), "data", None) or {}
            assert data.get("dev.actcore/error-kind") == kind, (
                f"expected {kind} on the JSON-RPC error path, got {data!r}"
            )
            if contains is not None:
                message = getattr(exc.error, "message", "") or ""
                assert contains in message, f"expected {contains!r} in {message!r}"
            return

        assert result.is_error, f"expected {tool} to fail, got {result!r}"
        result_meta = result.meta or {}
        assert result_meta.get("dev.actcore/error-kind") == kind, (
            f"expected {kind} on the isError path, got {result_meta!r}"
        )
        if contains is not None:
            message = result.content[0].text if result.content else ""
            assert contains in message, f"expected {contains!r} in {message!r}"

    return _expect


def pytest_sessionfinish(session, exitstatus):
    """Print act's stderr when the run did not pass.

    `log_file` keeps the audit trail out of the test output, which is right
    for a green run and wrong for every other kind: on an ephemeral CI runner
    nothing ever reads that file. Diagnosing a CI-only hang in this fleet
    cost several rounds of probing that one line of this stream would have
    answered. A hook rather than a fixture finaliser on purpose — fixture
    teardown does not run when the session dies mid-test.
    """
    if exitstatus == 0 or not LOG_FILE.exists():
        return
    text = LOG_FILE.read_text(errors="replace").strip()
    if text:
        print(f"\n--- act stderr ({LOG_FILE}) ---\n{text}")
