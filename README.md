# search-brave

Search the web with the [Brave Search API](https://api-dashboard.search.brave.com/) —
an independent index, not a Bing or Google reseller.

```bash
act call search-brave.wasm search \
  --args '{"query":"wasm component model","count":5}' \
  --session-args '{"api_key":"YOUR_TOKEN"}' \
  --allow wasi:http
```

Get a **Web Search** token at
[api-dashboard.search.brave.com](https://api-dashboard.search.brave.com/); the free tier
allows one request per second.

## The token travels in session args

Not as a tool argument. A credential passed as an argument would end up in a dataflow
graph's node parameters, and those live in the flow document, which is published and
signed. Session args come from the run context, where they can be marked secret and
redacted from run traces.

## Capability

Declares `wasi:http` limited to **`api.search.brave.com`** and nothing else. The endpoint
is fixed at build time, so that ceiling is a property of the artifact rather than of the
deployment — no grant can widen it, and `act info` shows it before the component ever
runs.

## Result shape

Identical to the other `search-*` components, so they are interchangeable in a graph.
`score` is always null and `unresponsive_engines` always empty because Brave exposes
neither; they exist to keep the shape uniform.

## Usage

```bash
just init   # first time: fetch WIT deps
just build  # build wasm component
just test   # run e2e tests
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
