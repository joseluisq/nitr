-- Runs once at startup; the returned table is snapshotted into every Lua
-- state and reachable from handlers as `nitr.cfg`.
return {
    upload_dir = "crates/nitr/examples/standards/uploads",
}
