-- Runs exactly once at startup (side effects such as schema setup belong
-- here); the returned table is available to handlers as nitr.cfg.
function(db)
    db:execute("CREATE TABLE IF NOT EXISTS accounts (name TEXT PRIMARY KEY, balance INTEGER)")
    db:execute("INSERT OR IGNORE INTO accounts (name, balance) VALUES ('alice', 100)")
    db:execute("INSERT OR IGNORE INTO accounts (name, balance) VALUES ('bob', 100)")
    return { app_name = "aggregate-example" }
end
