function(cfg, req)
    require 'scripts.modules.printf'

    local uri = req.uri
    local body = ""

    print("Config: ")
    dbg(cfg)

    local sql = ""..
        "CREATE TABLE IF NOT EXISTS person ("..
        "    id    INTEGER PRIMARY KEY,"..
        "    name  TEXT NOT NULL,"..
        "    data  BLOB"..
        ")"
    local affected = conn:execute(sql)
    print("Affected rows: " .. affected)

    -- Access Request data
    print("Server datetime: " .. os.date("%d-%m-%YT%H:%M:%S"))
    print("Request remote_addr: " .. req.remote_addr)
    print("Request method: " .. req.method)

    -- Access Request URI data
    print("Request URI:")
    print(" scheme: " .. uri.scheme)
    print(" authority: " .. uri.authority)
    print(" host: " .. uri.host)
    print(" port: " .. uri.port)
    print(" query: " .. uri.query)
    print(" path: " .. uri.path)

    if uri.path == "/" then
        -- Access Request headers
        print("Request headers:")
        if req.headers then
            for k, v in pairs(req.headers) do
            print(" " .. k .. " = " .. v)
            end
        end

        -- Read the whole Request body at once
        -- local data = req:text()
        -- dbg(data)

        -- Read the whole Request body as JSON
        -- local json = req:json()
        -- dbg(json["origin"])

        -- Read Request body in chunks
        repeat
            local chunk = req:read()
            if chunk then
                dbg(chunk)
            end
        until not chunk

        local headers = {
            ["X-Req-Method"] = req.method,
            ["X-Req-Path"] = req.path,
            ["X-Remote-Addr"] = req.remote_addr,
        }

        -- Using Fetch HTTP client
        local client = fetch("get", "https://httpbin.org/ip", headers)
        local resp = client:send()

        -- Access Response data
        print("Response status: " .. resp.status)

        -- Access Request URL data
        print("Request URL:")
        print(" scheme: " .. resp.url.scheme)
        print(" authority: " .. resp.url.authority)
        print(" host: " .. resp.url.host)
        print(" port: " .. resp.url.port)
        print(" query: " .. resp.url.query)
        print(" path: " .. resp.url.path)

        -- Access Response headers
        print("Response headers:")
        if resp.headers then
            for k, v in pairs(resp.headers) do
                print(" " .. k .. " = " .. v)
            end
        end

        -- Read the whole Response body at once
        -- local data = resp:text()
        -- dbg(data)

        -- Read Response body in chunks
        print("Response body chunks:")
        repeat
            local chunk = resp:read()
            if chunk then
                dbg(chunk)
            end
        until not chunk

        -- Read the whole Response body as JSON
        -- local json = resp:json()
        -- dbg(json["origin"])

        -- Using template rendering
        -- body = template:render("response.j2", {
        --     ["remote_addr"] = req.remote_addr,
        --     ["datetime"] = os.date("%d-%m-%YT%H:%M:%S"),
        -- })

        -- Decode a JSON string into a Lua table
        local s = "{\"current_time\":\"" .. os.date("%d-%m-%YT%H:%M:%S") .. "\"}"
        -- local s = [[{"current_time":"]] .. os.date("%d-%m-%YT%H:%M:%S") .. [["}]]
        local obj = json:decode(s)
        print(obj["current_time"])
        
        -- Encode a Lua table to JSON string
        body = json:encode(obj)
    else
        body = "Hello from Lua! (custom path)\n"
        -- printf('[info] Custom path request: ' .. path)
    end

    return {
        status = 200,
        headers = {
            ["Content-Type"] = "application/json",
            ["X-Req-Method"] = req.method,
            ["X-Req-Path"] = req.path,
            ["X-Remote-Addr"] = req.remote_addr,
        },
        body = body
    }
end
