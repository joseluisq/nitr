-- A small application showing the nitr.app() programming model:
-- Rust-side routing, Lua middleware, response helpers, and cookies.

local SECRET = "change-me" -- in real apps: read it from nitr.cfg

local app = nitr.app()

-- Global middleware runs for every routed request, in registration order.
app:use(function(next)
    return function(req)
        local res = next(req)
        res.headers = res.headers or {}
        res.headers["X-Powered-By"] = "nitr"
        return res
    end
end)

-- A per-route middleware: short-circuits without calling the handler.
local function auth(next)
    return function(req)
        if req.headers["authorization"] ~= "secret" then
            return http.error(401, "Unauthorized")
        end
        return next(req)
    end
end

app:get("/", function(req)
    return html("<h1>Hello from nitr.app()</h1>")
end)

-- Path parameters are captured by the Rust router.
app:get("/users/:id", function(req)
    return json({ id = req.params.id })
end)

app:post("/users", function(req)
    local body = req:json()
    return json({ created = true, name = body.name }, 201)
end)

app:get("/admin", auth, function(req)
    return text("welcome, admin")
end)

-- Signed cookies: /login sets one, /whoami verifies it.
app:get("/login", function(req)
    local res = redirect("/whoami")
    res.cookies:set_signed("session", "user-42", SECRET, {
        http_only = true,
        same_site = "Lax",
        max_age = 3600,
    })
    return res
end)

app:get("/whoami", function(req)
    local user = req.cookies:verify("session", SECRET)
    if not user then
        return http.error(401, { code = "NO_SESSION" })
    end
    return json({ user = user })
end)

-- Content negotiation: one route serving JSON and HTML clients.
app:get("/data", function(req)
    return negotiate(req, {
        ["application/json"] = function(r)
            return json({ hello = "world" })
        end,
        ["text/html"] = function(r)
            return html("<p>hello world</p>")
        end,
    })
end)

app:on_error(function(err, req)
    dbg(err)
    return http.error(500, { code = "INTERNAL", path = req.path })
end)

return app
