local app = nitr.app()

app:get("/api/notes", function(req)
    return nitr.json(nitr.db:query("SELECT id, text FROM notes ORDER BY id"))
end)

app:post("/api/notes", function(req)
    local body = req:json()
    if type(body.text) ~= "string" or #body.text == 0 then
        return nitr.error(422, { code = "TEXT_REQUIRED" })
    end
    nitr.db:execute("INSERT INTO notes (text) VALUES (?)", { body.text })
    return nitr.json({ ok = true }, 201)
end)

app:on_error(function(err, req)
    nitr.log.error("handler failed", { error = err })
    return nitr.error(500, { code = "INTERNAL" })
end)

return app
