-- Structured logging and request ids.
--
-- log.debug/info/warn/error(msg, fields?) emit tracing events inside the
-- per-request span, so every line in the server log carries the request
-- id, method, and path automatically -- no manual correlation needed.

local app = nitr.app()

-- Access-log style middleware with structured fields.
app:use(function(next)
    return function(req)
        log.debug("request received", { remote = req.remote_addr })
        local res = next(req)
        log.info("request handled", { status = res.status or 200 })
        return res
    end
end)

app:get("/", function(req)
    -- req.id is the same value the client sees as X-Request-ID.
    log.info("saying hello", { id = req.id, lang = req.headers["accept-language"] })
    return json({ hello = "world", request_id = req.id })
end)

app:post("/echo", function(req)
    -- Bodies above [limits].max_body_bytes never reach this handler (413).
    return text(req:text())
end)

app:on_error(function(err, req)
    log.error("handler failed", { error = err })
    return http.error(500, { code = "INTERNAL" })
end)

return app
