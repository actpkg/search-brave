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

A human stores it once, with either of these:

```bash
act login search-brave.wasm                    # prompts for it, no arguments
act secret set search-brave.wasm --key default \
  --field brave:api-key --fields-stdin         # {"brave:api-key": "<token>"}
```

## You never see the token, and you must not ask for one

**Do not ask the user to paste their Brave token, and do not put one in any argument.**
There is nowhere to put it: neither `search` nor `open_session` has a field for a
credential. The component reads it from the host credential store on its first tool call.

```bash
# one-shot
act call search-brave.wasm search \
  --args '{"query":"wasm component model","count":5}' \
  --session-args '{}' \
  --allow wasi:http --allow act:credentials
```

Over MCP, call `open_session` (no arguments) and pass its id as `std:session-id` metadata
on each `search` call, then `close_session` when done.

`open_session` takes one optional argument, `credential_key`, naming which stored
credential to search with; it defaults to `default`. Pass it only when the user has told
you they keep more than one Brave token. It is a lookup name — letters, digits, `-`, `_`
and `.` — not a sentence: a human may be shown it while deciding whether to release the
credential, so anything else is refused.

If a call comes back `std:credential-required`, no usable token is stored (or policy
denies the store). Relay the command in the message to the user and stop — that error is
not something you can work around, and retrying will not change it.

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

## Capabilities

* `wasi:http`, limited to **`api.search.brave.com`** and nothing else. Because the
  endpoint is fixed at build time, that ceiling is a property of the artifact: no
  deployment can widen it, and `act info` shows it before you ever run the component.
* `act:credentials` — read the one stored Brave token. A bare class with no parameters.

A run needs both released: `--allow wasi:http --allow act:credentials`. Without the
second, every search fails with `std:credential-required` however the token was stored.
