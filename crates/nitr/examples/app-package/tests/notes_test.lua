-- Run with: cargo run -p nitr-cli -- -c crates/nitr/examples/app-package/nitr.toml test

-- Static index is served.
local resp = test.request("GET", "/")
assert(resp.status == 200, "index: " .. resp.status)

-- Validation rejects an empty note.
local resp = test.request("POST", "/api/notes", { body = '{"text":""}' })
assert(resp.status == 422, "empty note: " .. resp.status)

-- Create, then read back.
local resp = test.request("POST", "/api/notes", { body = '{"text":"from the test"}' })
assert(resp.status == 201, "create: " .. resp.status)

local resp = test.request("GET", "/api/notes")
assert(resp.status == 200, "list: " .. resp.status)
local notes = json:decode(resp.body)
assert(#notes >= 1, "expected at least one note")
assert(notes[#notes].text == "from the test", "unexpected last note")
