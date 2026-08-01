GET {{baseurl}}/info
HTTP 200
[Asserts]
jsonpath "$.std.name" == "search-brave"
jsonpath "$.std.version" isString
