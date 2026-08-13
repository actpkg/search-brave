# search-brave

Search the web with the [Brave Search API](https://api-dashboard.search.brave.com/) —
an independent index, not a Bing or Google reseller.

Get a **Web Search** token at
[api-dashboard.search.brave.com](https://api-dashboard.search.brave.com/) — the free tier
allows one request per second — and store it once:

```bash
act login search-brave.wasm          # prompts for the token, no arguments needed
```

Then search. The token is never mentioned again:

```bash
act call search-brave.wasm search \
  --args '{"query":"wasm component model","count":5}' \
  --session-args '{}' \
  --allow wasi:http --allow act:credentials
```

## The token is not an argument, in either position

It used to be a session argument (`--session-args '{"api_key":"…"}'`). Since **0.2.0** it
is not, and there is nowhere left to put it: a session names a credential *key*, and the
token itself is read from the host credential store through
[`act:credentials/store`](https://github.com/actcore/act-spec/blob/main/spec/ACT-AUTH.md).

That closes a real hole rather than tidying one. Everything sent to `open-session` is
plaintext to whatever opened it: it lands in the agent's context and its transcript, in
the host's session record, and — in a dataflow graph — in the node parameters of a flow
document that gets published and signed. Under the store, the secret goes from the
operator to the host and from the host to the component, and never through the agent at
all.

If `act login` is not how your deployment provisions credentials, the explicit form is:

```bash
act secret set search-brave.wasm --key default \
  --field brave:api-key --fields-stdin
{"brave:api-key": "<Web Search subscription token>"}
```

`default` is the key a session uses when its args do not name one. Keep several tokens
under different keys and pick one per session with
`--session-args '{"credential_key":"prod"}'`.

The fetch happens on the **first tool call**, not at `open-session`: `get-secret` needs a
session the host has already marked live, and the host marks it live only once
`open-session` has returned. The token is then cached for that session's lifetime, and if
Brave refuses it the session is poisoned rather than re-fetched — otherwise a bad token
would ask the store, and possibly a human, on every single call.

## Capabilities

* `wasi:http`, limited to **`api.search.brave.com`** and nothing else. The endpoint is
  fixed at build time, so that ceiling is a property of the artifact rather than of the
  deployment — no grant can widen it, and `act info` shows it before the component ever
  runs.
* `act:credentials`, a bare class with no parameters: read the one credential above. An
  undeclared class is denied outright, so this line is what makes the store reachable at
  all — and `--allow act:credentials` is what releases it at run time.

## Result shape

Identical to the other `search-*` components, so they are interchangeable in a graph.
`score` is always null and `unresponsive_engines` always empty because Brave exposes
neither; they exist to keep the shape uniform.

## Usage

```bash
just init       # first time: fetch WIT deps
just build      # build + pack the wasm component
just test-unit  # host-target unit tests (credential parsing, schema, errors)
just test       # unit tests, then the MCP-driven e2e suite
```

## Publishing

Pushing to `main` publishes a signed component to
`actpkg.dev/<owner>/search-brave` (owner derived from the git remote;
override the full path with the `OCI_REGISTRY` env var). CI signs the image
keylessly with [cosign](https://docs.sigstore.dev/) via GitHub OIDC.

One-time setup: create a Personal Access Token at
[actpkg.dev](https://actpkg.dev) and add it as a repository secret named
**`ACTPKG_TOKEN`** (Settings → Secrets and variables → Actions).

```bash
just publish   # local publish (unsigned); CI signs on push to main
```

## License

MIT OR Apache-2.0
