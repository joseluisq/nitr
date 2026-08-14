-- A tour of the `nitr.*` standard library: the namespace table is the one
-- and only surface Nitr exposes to Lua.

local app = nitr.app()

app:get("/", function(req)
    -- `nitr.json(value)` is the JSON response helper; the same userdata
    -- also carries the codec (`nitr.json:encode` / `nitr.json:decode`).
    return nitr.json({
        namespace = "nitr.*",
        helpers = { "text", "html", "json", "redirect", "status", "negotiate", "sse", "error" },
        library = { "log", "crypto", "auth", "fetch", "await_all", "db", "template", "dbg" },
    })
end)

-- Crypto primitives: hashing, HMAC, and OS randomness.
app:get("/token", function(req)
    local token = nitr.crypto.sha256(nitr.crypto.random_bytes(32))
    local mac = nitr.crypto.hmac_sha256("server-secret", token)
    return nitr.json({ token = token, mac = mac })
end)

-- Password storage the right way: argon2id hashing and verification are
-- implemented in Rust; Lua only composes them.
app:post("/password", function(req)
    local password = req:text()
    if password == "" then
        return nitr.error(400, { code = "EMPTY_PASSWORD" })
    end
    local hash = nitr.crypto.password_hash(password)
    return nitr.json({
        hash = hash,
        verified = nitr.crypto.password_verify(password, hash),
        rejected = not nitr.crypto.password_verify("wrong-" .. password, hash),
    })
end)

-- Bearer-token middleware built from the `nitr.auth` primitives. The token
-- comparison is constant-time: `==` on secrets leaks timing.
local function require_bearer(next)
    return function(req)
        local token = nitr.auth.bearer(req)
        if not token or not nitr.crypto.constant_time_eq(token, "s3cret") then
            nitr.log.warn("unauthorized", { path = req.path })
            return nitr.error(401, { code = "UNAUTHORIZED" })
        end
        return next(req)
    end
end

app:get("/secure", require_bearer, function(req)
    return nitr.json({ secret = "the pool is warm" })
end)

-- Basic credentials: `nitr.auth.basic(req)` returns `user, pass` or nil.
app:get("/whoami", function(req)
    local user, pass = nitr.auth.basic(req)
    if not user then
        return nitr.error(401, "who are you?")
    end
    return nitr.json({ user = user, password_length = #pass })
end)

return app
