function(conn)
    -- Using database connection
    local sql = ""..
        "CREATE TABLE IF NOT EXISTS person ("..
        "    id    INTEGER PRIMARY KEY,"..
        "    name  TEXT NOT NULL,"..
        "    data  BLOB"..
        ")"
    local affected = conn:execute(sql)

    -- Using Fetch HTTP client
    local headers = {
        ["X-Req-Method"] = "GET",
        ["X-Remote-Addr"] = "78.54.98.17",
    }
    local resp = fetch("get", "https://httpbin.org/ip", headers):send()
    print("Response status: " .. resp.status)

    -- Read Response body in chunks
    print("Response body: ")
    repeat
        local chunk = resp:read()
        if chunk then
            print(chunk)
        end
    until not chunk

    -- Passing custom data to the HTTP handler
    return {
        status = resp.status,
        affected = affected,
        server_time = os.date("%d-%m-%YT%H:%M:%S"),
    }
end
