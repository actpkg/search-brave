"""search's session/argument validation, end to end.

No live Brave API key or endpoint is used anywhere in this file — nor was it
in the old hurl suite. Every scenario here fails before any network call
would happen: schema checks, session-presence/lifecycle checks (which are
the guest's own `ctx.metadata()` logic — see src/lib.rs), and argument
validation in `build_url`, which runs *before* the outbound `wasi_fetch`
call. The "open a real session" tests use `"e2e-placeholder-token"`, a
string that is never sent to Brave or checked against anything — opening a
session only stores it. There is no test anywhere in this suite (old or new)
that exercises a successful search against the real API; that would need a
live subscription token and is out of scope for this migration, not
something it silently dropped.
"""

import json
import re


async def test_search_tool_schema_excludes_api_key(client):
    tools = await client.list_tools()
    search = next(t for t in tools if t.name == "search")
    assert search.name == "search"
    assert "query" in search.inputSchema["required"]
    # The credential is NOT a tool argument — it travels via session args, so
    # that a flow node's params (published, signed) never carry a secret.
    assert "api_key" not in search.inputSchema.get("properties", {})


async def test_open_session_args_schema_requires_api_key(client):
    tools = await client.list_tools()
    open_session = next(t for t in tools if t.name == "open_session")
    schema = open_session.inputSchema
    assert schema["type"] == "object"
    assert "api_key" in schema["properties"]
    assert "api_key" in schema["required"]


async def test_search_without_session_is_refused_before_any_network_access(client, expect_error):
    await expect_error(
        client, "search", {"query": "wasm component model"},
        "std:session-not-found", contains="std:session-id",
    )


async def test_open_session_rejects_empty_api_key(client, expect_error):
    # Rejected at session-open rather than surfacing later as a confusing
    # auth failure from the API.
    await expect_error(client, "open_session", {"api_key": "   "}, "std:invalid-args")


async def test_open_session_returns_an_id(session):
    # hurl's `matches` is an unanchored search, not a full match (verified
    # against hurl 8.0.1) — the original pattern has no ^/$ of its own
    # either, so it is kept exactly as written rather than anchored.
    assert re.search(r"search-brave_\d+", session)


async def test_search_rejects_empty_query(client, session, expect_error):
    # Argument validation happens locally, so this never reaches Brave and
    # needs no real key — the placeholder session is enough.
    await expect_error(
        client, "search", {"query": "   "}, "std:invalid-args",
        contains="must not be empty", meta={"std:session-id": session},
    )


async def test_search_rejects_count_over_20(client, session, expect_error):
    # count is capped at 20 by the upstream API; rejected early with a
    # message that says so.
    await expect_error(
        client, "search", {"query": "rust", "count": 50}, "std:invalid-args",
        contains="between 1 and 20", meta={"std:session-id": session},
    )


async def test_search_rejects_offset_over_9(client, session, expect_error):
    await expect_error(
        client, "search", {"query": "rust", "offset": 42}, "std:invalid-args",
        contains="between 0 and 9", meta={"std:session-id": session},
    )


async def test_search_after_close_session_is_session_not_found(client, expect_error):
    # A dedicated open/close/reuse sequence, not the shared `session`
    # fixture: this is the one scenario that genuinely needs to control the
    # session's lifecycle itself (close it, then prove the id is dead)
    # rather than just needing *a* session, so it manages its own.
    opened = await client.call_tool("open_session", {"api_key": "e2e-placeholder-token"})
    sid = json.loads(opened.content[0].text)["id"]
    await client.call_tool("close_session", {"session_id": sid})

    await expect_error(
        client, "search", {"query": "rust"}, "std:session-not-found",
        meta={"std:session-id": sid},
    )
