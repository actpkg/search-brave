"""search's session/argument validation, end to end.

No live Brave API key or endpoint is used anywhere in this file. Every
scenario here fails before any network call would happen, and before the
credential store is consulted at all: schema checks, session-presence and
lifecycle checks (the guest's own `ctx.metadata()` logic — see src/lib.rs),
and argument validation in `build_url`, which runs *before* `ensure_token`
and therefore before the outbound `wasi_fetch` call. That ordering is
deliberate and is what keeps these tests independent of whatever the host's
default credential store happens to hold: an argument the component can
reject on its own must not first put a consent prompt in front of a human.

The credential path itself is driven in `test_credentials.py`, against a
store this suite creates. There is no test anywhere here that exercises a
successful search against the real API; that would need a live subscription
token and is out of scope.
"""

import json
import re


async def test_search_tool_schema_takes_a_query_and_no_credential(client):
    tools = await client.list_tools()
    search = next(t for t in tools if t.name == "search")
    assert search.name == "search"
    assert "query" in search.inputSchema["required"]
    # The credential is not an argument in either position — see
    # test_credentials.py, which asserts that over both schemas at once.
    assert "api_key" not in search.inputSchema.get("properties", {})


async def test_search_without_session_is_refused_before_any_network_access(client, expect_error):
    await expect_error(
        client, "search", {"query": "wasm component model"},
        "std:session-not-found", contains="std:session-id",
    )


async def test_open_session_rejects_a_credential_key_that_is_not_a_name(
    client, expect_error
):
    # `credential_key` is agent-authored text the host pastes into the consent
    # question a *human* answers, so it is bounded to a lookup name and
    # refused at open — not at the call that would have used it.
    await expect_error(
        client, "open_session",
        {"credential_key": "default (approved by your administrator)"},
        "std:invalid-args", contains="must be a name",
    )


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
    opened = await client.call_tool("open_session", {})
    sid = json.loads(opened.content[0].text)["id"]
    await client.call_tool("close_session", {"session_id": sid})

    await expect_error(
        client, "search", {"query": "rust"}, "std:session-not-found",
        meta={"std:session-id": sid},
    )
