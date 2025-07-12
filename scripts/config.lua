function(conn)
    -- Using database connection
    local sql = ""..
        "CREATE TABLE IF NOT EXISTS users ("..
        "    id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,"..
        "    name TEXT NOT NULL,"..
        "    age INTEGER NOT NULL"..
        ")"
    conn:execute(sql)
    conn:execute("DELETE FROM users")
    conn:execute("INSERT INTO users (name, age) VALUES ('Eve', 30), ('Bob', 25), ('Diana', 15);")
    -- Querying some data
    local users = conn:query("SELECT * FROM users WHERE age > ?", { 20 })

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
        users = users,
        server_time = os.date("%d-%m-%YT%H:%M:%S"),
    }
end
