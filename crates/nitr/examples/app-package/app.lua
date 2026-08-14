local app = nitr.app()

app:get("/api/notes", function(req)
    return json(conn:query("SELECT id, text FROM notes ORDER BY id"))
end)

app:post("/api/notes", function(req)
    local body = req:json()
    if type(body.text) ~= "string" or #body.text == 0 then
        return http.error(422, { code = "TEXT_REQUIRED" })
    end
    conn:execute("INSERT INTO notes (text) VALUES (?)", { body.text })
    return json({ ok = true }, 201)
end)

app:on_error(function(err, req)
    log.error("handler failed", { error = err })
    return http.error(500, { code = "INTERNAL" })
end)

return app
