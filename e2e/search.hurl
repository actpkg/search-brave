# The search tool is listed with its argument schema.
POST {{baseurl}}/tools
HTTP 200
[Asserts]
jsonpath "$.tools[0].name" == "search"
jsonpath "$.tools[0].parameters_schema.required" contains "query"
# The credential is NOT a tool argument — it travels via session args, so that a flow
# node's params (which live in a published, signed flow document) never carry a secret.
jsonpath "$.tools[0].parameters_schema.properties.api_key" not exists

# The session args schema advertises the credential.
POST {{baseurl}}/sessions/open-args-schema
{}
HTTP 200
[Asserts]
jsonpath "$.type" == "object"
jsonpath "$.properties.api_key" exists
jsonpath "$.required" contains "api_key"

# Calling without a session is refused before any network access.
POST {{baseurl}}/tools/search
{"arguments": {"query": "wasm component model"}}
HTTP *
[Asserts]
jsonpath "$.error.kind" == "std:session-not-found"
jsonpath "$.error.message" contains "std:session-id"

# An empty api_key is rejected at session-open rather than surfacing later as a
# confusing auth failure from the API.
POST {{baseurl}}/sessions
{"arguments": {"api_key": "   "}}
HTTP *
[Asserts]
jsonpath "$.error.kind" == "std:invalid-args"

# Open a real session; everything below reuses its id.
POST {{baseurl}}/sessions
{"arguments": {"api_key": "e2e-placeholder-token"}}
HTTP 201
[Asserts]
jsonpath "$.id" matches "search-brave_\\d+"
[Captures]
session_id: jsonpath "$.id"

# Argument validation happens locally, so these never reach Brave and need no key.
POST {{baseurl}}/tools/search
{"arguments": {"query": "   "}, "metadata": {"std:session-id": "{{session_id}}"}}
HTTP 422
[Asserts]
jsonpath "$.error.kind" == "std:invalid-args"
jsonpath "$.error.message" contains "must not be empty"

# count is capped at 20 by the upstream API; reject early with a message that says so.
POST {{baseurl}}/tools/search
{"arguments": {"query": "rust", "count": 50}, "metadata": {"std:session-id": "{{session_id}}"}}
HTTP 422
[Asserts]
jsonpath "$.error.kind" == "std:invalid-args"
jsonpath "$.error.message" contains "between 1 and 20"

# offset is capped at 9.
POST {{baseurl}}/tools/search
{"arguments": {"query": "rust", "offset": 42}, "metadata": {"std:session-id": "{{session_id}}"}}
HTTP 422
[Asserts]
jsonpath "$.error.kind" == "std:invalid-args"
jsonpath "$.error.message" contains "between 0 and 9"

# Closing the session invalidates the id.
DELETE {{baseurl}}/sessions/{{session_id}}
HTTP *

POST {{baseurl}}/tools/search
{"arguments": {"query": "rust"}, "metadata": {"std:session-id": "{{session_id}}"}}
HTTP *
[Asserts]
jsonpath "$.error.kind" == "std:session-not-found"
