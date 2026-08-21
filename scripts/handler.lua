-- Development handler: a small tour of the `nitr.*` standard library.
-- Every Nitr API lives on the `nitr` namespace table; the script returns
-- the application built with `nitr.app()`.

-- `require` resolves relative to the scripts directory.
require 'modules.printf'

local app = nitr.app()

-- Global middleware: runs for every matched route.
app:use(function(next)
    return function(req)
        nitr.log.info("request", { method = req.method, path = req.path })
        return next(req)
    end
end)

app:get("/", function(req)
    -- The configuration script's snapshot is available as `nitr.cfg`.
    nitr.dbg(nitr.cfg)

    -- SQLite through `nitr.db` (runs off the async threads).
    nitr.db:execute(
        "CREATE TABLE IF NOT EXISTS person (" ..
        "    id    INTEGER PRIMARY KEY," ..
        "    name  TEXT NOT NULL," ..
        "    data  BLOB" ..
        ")")

    -- JSON codec: `nitr.json:decode` / `nitr.json:encode`.
    local obj = nitr.json:decode('{"current_time":"' .. nitr.time.format(nitr.time.now(), "%d-%m-%YT%H:%M:%S") .. '"}')
    printf("[info] decoded current_time: %s", obj.current_time)

    -- Called as a function, `nitr.json(value)` builds the JSON response.
    return nitr.json({
        message = "Hello from Lua!",
        method = req.method,
        remote_addr = req.remote_addr,
        current_time = obj.current_time,
    })
end)

app:get("/users", function(req)
    -- Data seeded by the configuration script (scripts/config.lua).
    return nitr.json({ users = nitr.cfg.users, seeded_at = nitr.cfg.server_time })
end)

app:get("/hello/:name", function(req)
    -- Template rendering via minijinja (`[templating] dir` in nitr.toml).
    local body = nitr.template:render("response.j2", {
        remote_addr = "req.remote_addr",
        datetime = nitr.time.format(nitr.time.now(), "%d-%m-%YT%H:%M:%S"),
    })
    return nitr.html(body)
end)

app:on_error(function(err, req)
    nitr.log.error("handler failed", { error = err, path = req.path })
    return nitr.error(500, { code = "INTERNAL" })
end)

return app
