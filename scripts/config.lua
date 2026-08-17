-- Development configuration script: runs exactly once at startup; its
-- returned table (plain data) is snapshotted into every Lua state and
-- exposed to handlers as `nitr.cfg`. The database connection arrives as
-- the script's vararg.
local db = ...

-- Seed some data through the SQLite connection.
db:execute("" ..
    "CREATE TABLE IF NOT EXISTS users (" ..
    "    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT," ..
    "    name TEXT NOT NULL," ..
    "    age INTEGER NOT NULL" ..
    ")")
db:execute("DELETE FROM users")
db:execute("INSERT INTO users (name, age) VALUES ('Eve', 30), ('Bob', 25), ('Diana', 15);")
local users = db:query("SELECT * FROM users WHERE age > ?", { 20 })

-- Outbound HTTP via `nitr.fetch` (guarded: startup must not depend on
-- the network being reachable in development).
local status = 0
local ok, resp = pcall(function()
    return nitr.fetch("get", "https://httpbin.org/ip", {
        headers = { ["X-Req-Method"] = "GET" },
        timeout = 5,
    }):send()
end)
if ok then
    status = resp.status
    print("Response status: " .. resp.status)
else
    -- `nitr.errinfo` classifies whatever pcall caught (Rust or Lua
    -- error): concise `kind: message (source:line)` instead of the raw
    -- traceback dump. `.pretty` adds ANSI color on a terminal; plain
    -- concatenation (`.. err`) stays uncolored for logs/bodies.
    print("fetch skipped: " .. nitr.errinfo(resp).pretty)
end

-- Passing custom data to the HTTP handlers (`nitr.cfg`).
return {
    status = status,
    users = users,
    server_time = nitr.time.format(nitr.time.now(), "%d-%m-%YT%H:%M:%S"),
}
