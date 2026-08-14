-- Static mounts are declared by the app and served entirely in Rust;
-- only /api/* requests below ever reach Lua.

local ROOT = "crates/nitr/examples/static-site/public"

local app = nitr.app()

-- The whole site at /, plus a long-cache mount for fingerprinted assets.
app:static("/", ROOT)
app:static("/assets", ROOT .. "/assets", {
    cache_control = "public, max-age=31536000, immutable",
})

app:get("/api/time", function(req)
    return nitr.json({ now = os and os.time and os.time() or "os disabled" })
end)

return app
