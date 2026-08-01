---
name: search-brave
description: Search the web with the Brave Search API — independent index, ranked results
metadata:
  act: {}
---

# Brave Search Component

Query the [Brave Search API](https://api-dashboard.search.brave.com/) and get back ranked
web results. Brave runs its own index rather than reselling Bing or Google.

## Getting a key

Create a **Web Search** subscription token at
[api-dashboard.search.brave.com](https://api-dashboard.search.brave.com/). The free tier
allows one request per second.

## The key goes in session args, not tool arguments

```bash
# one-shot
act call search-brave.wasm search \
  --args '{"query":"wasm component model","count":5}' \
  --session-args '{"api_key":"YOUR_TOKEN"}' \
  --allow wasi:http
```

Over MCP or HTTP, open a session first and pass `std:session-id` on each call.

This is deliberate. A credential passed as a tool argument would end up in a dataflow
graph's node parameters, and those live in the flow document — which gets published and
signed. Session args come from the run context instead, where they can be marked secret
and redacted from traces.

## Tools

### search

| Argument | Required | Meaning |
|---|---|---|
| `query` | yes | What to search for. |
| `count` | no | Results to return, 1–20 (the API's maximum). |
| `offset` | no | Page offset, 0–9. |
| `country` | no | Two-letter code, e.g. `us`, `de`. |
| `search_lang` | no | Result language, e.g. `en`. |
| `safesearch` | no | `off`, `moderate` or `strict`. |
| `freshness` | no | `pd` day, `pw` week, `pm` month, `py` year, or `YYYY-MM-DDtoYYYY-MM-DD`. |
| `timeout_ms` | no | Request timeout, default 15000. |

Returns:

```json
{
  "query": "wasm component model",
  "results": [
    {
      "title": "…", "url": "https://…", "snippet": "…",
      "published": "2026-01-15T00:00:00", "score": null,
      "engine": "brave", "category": "general"
    }
  ],
  "answers": ["…"],
  "suggestions": ["corrected query"],
  "unresponsive_engines": []
}
```

`score` is always null and `unresponsive_engines` always empty — Brave exposes neither.
Both fields exist so the shape matches the other `search-*` components and they stay
interchangeable in a graph. `suggestions` carries Brave's spelling correction when it
rewrote your query.

News results are included alongside web results, tagged `category: "news"`.

## Fetching the pages

This component returns links and snippets, not page content. Pass the URLs to
`http-client` and convert with an HTML→Markdown component. Keeping the steps separate
means each declares its own, narrower network ceiling.

## Capability

Declares `wasi:http` limited to **`api.search.brave.com`** and nothing else. Because the
endpoint is fixed at build time, that ceiling is a property of the artifact: no
deployment can widen it, and `act info` shows it before you ever run the component.
