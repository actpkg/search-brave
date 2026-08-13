"""The token is not an argument — not a tool one, not a session one — and the
component reads it from the host credential store instead.

Nothing here uses a live Brave token or reaches the network. Two of the three
assertions inspect published schemas; the third drives the real `get-secret`
path against a store this module creates and deliberately leaves **empty**,
which is the one branch of that path reachable without a subscription: the
refusal an operator actually meets, and the command it hands them.
"""

import asyncio
import json
import subprocess
from contextlib import AsyncExitStack
from pathlib import Path

import pytest
from fastmcp import Client
from fastmcp.client.transports import StdioTransport

from conftest import CONNECT_TIMEOUT, LOG_FILE


@pytest.fixture(scope="module")
def empty_credential_store(act_command: list[str], tmp_path_factory) -> str:
    """A `--credentials-backend` argument naming a store with nothing in it.

    An empty store rather than no store at all: without this the host reads
    the platform's default credential store, and whether *that* holds a
    search-brave token is a property of the machine running the suite, not of
    the component. Empty is the deterministic version of the same question.
    """
    # The only skip: this CLI has no credential store, so the feature under
    # test does not exist here. Everything past this line is a regression.
    probe = subprocess.run(
        [*act_command, "secret", "--help"], capture_output=True, text=True
    )
    if probe.returncode != 0:
        pytest.skip("this `act` has no credential store (`act secret`); nothing to drive")
    return f"file:{tmp_path_factory.mktemp('credentials')}"


@pytest.fixture
async def client(act_command: list[str], wasm_path: Path, empty_credential_store: str):
    """Overrides the suite-wide client for this module only: same two grants,
    plus a store whose contents this suite controls.
    """
    transport = StdioTransport(
        command=act_command[0],
        args=[
            *act_command[1:], "run", str(wasm_path), "--mcp",
            "--allow", "wasi:http", "--allow", "act:credentials",
            "--credentials-backend", empty_credential_store,
        ],
        keep_alive=False,
        log_file=LOG_FILE,
    )
    async with AsyncExitStack() as stack:
        try:
            async with asyncio.timeout(CONNECT_TIMEOUT):
                connected = await stack.enter_async_context(Client(transport))
        except TimeoutError:
            pytest.fail(f"MCP client did not connect within {CONNECT_TIMEOUT}s")
        yield connected


async def test_no_schema_offers_a_place_to_put_the_token(client):
    """The property this whole design exists to protect, asserted over what an
    agent can actually see: both schemas at once, since a credential removed
    from one and left in the other has not gone anywhere.
    """
    tools = {t.name: t.inputSchema for t in await client.list_tools()}
    for name in ("search", "open_session"):
        props = tools[name].get("properties", {})
        for forbidden in ("api_key", "token", "secret", "password", "brave:api-key"):
            assert forbidden not in props, f"{forbidden} must never be a {name} argument"


async def test_open_session_publishes_the_credential_key_and_nothing_else(client):
    tools = {t.name: t.inputSchema for t in await client.list_tools()}
    schema = tools["open_session"]
    assert schema["type"] == "object"
    assert list(schema["properties"]) == ["credential_key"]
    # Optional: a deployment with one credential opens a session with `{}`.
    assert "credential_key" not in schema.get("required", [])


async def test_search_without_a_stored_token_names_the_command_that_stores_one(
    client, expect_error
):
    """The real `get-secret` path, all the way to the store and back.

    `not-found` and `denied` are one error on purpose (ACT-AUTH §1.1.7), so
    this asserts the kind and the fix rather than the cause — and the fix has
    to be runnable as printed, which is the part that rots silently.
    """
    opened = await client.call_tool("open_session", {})
    sid = json.loads(opened.content[0].text)["id"]
    await expect_error(
        client, "search", {"query": "wasm component model"},
        "std:credential-required",
        contains="act secret set <component-ref> --key default "
                 "--field brave:api-key --fields-stdin",
        meta={"std:session-id": sid},
    )
