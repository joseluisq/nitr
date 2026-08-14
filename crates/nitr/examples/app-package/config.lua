-- Runs exactly once at startup; the returned table becomes nitr.cfg.
function(db)
    db:execute("CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, text TEXT)")
    return { app_name = "notes" }
end
