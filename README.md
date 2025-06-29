# Nitr

> A Rust web server embedding [Lua](https://www.lua.org/) for fast, efficient and safe dynamic backends.

**STATUS:** Nitr is in early development stage and not ready for production use. Feel free to try it out and contribute.

## Overview

The server aims to provide a flexible and extensible architecture for building dynamic web server backends using [Lua](https://www.lua.org/) as the scripting language.

An application can define a `scripts/config.lua` script file to perform setup at server startup and process HTTP requests via a `scripts/handler.lua` script file.

The setup script data will be shared with the HTTP handler script, allowing to pass data to be re-used in on every request.

## Features

**Note** This is a work in progress, so it might have features partially working or not at all. Also, remember that those may change over time.

- Built in Rust for low-level performance, efficiency, and safety.
- Lua as scripting language to configure and handling HTTP requests ([`mlua`](https://github.com/mlua-rs/mlua/)).
- Configurable Lua standard library modules.
- Built-in Rust APIs available in Lua scripts like:
  - `Request` and `Response` types
  - HTTP Client (`fetch`)
  - Template Engine (`jinja` syntax)
  - JSON encoder/decoder (`serde_json`)
  - SQLite Driver (`rusqlite`)
  - Utility functions (`debug`, etc)
- Lua configuration handler (script to run before server startup)
- Lua request handler (script to run after server startup on every request)
- Configuration file in TOML format (`nitr.toml`) to define server settings and Lua modules.
- Create your own Lua modules and use them in your scripts via the Nitr crate.

For tracking the progrees of the project, please refer to the [GitHub issues](https://github.com/joseluisq/nitr/issues) page.

## Configuration Script

This Lua script file is executed once at server startup. It can be used to setup an application before it starts processing HTTP requests.

```lua
-- scripts/config.lua
function(conn)
    -- Using the built-in SQLite database connection
    local sql = ""..
        "CREATE TABLE IF NOT EXISTS person ("..
        "    id    INTEGER PRIMARY KEY,"..
        "    name  TEXT NOT NULL,"..
        "    data  BLOB"..
        ")"
    conn:execute(sql)

    -- Passing custom data to the HTTP handler
    return {
        server_time = os.date("%d-%m-%YT%H:%M:%S"),
    }
end
```

## HTTP handler Script

This Lua script file is executed for every HTTP request. It can be used to handle requests and return a responses.
The Lua function will receive the configuration data returned by the setup script and the client HTTP request.

```lua
-- scripts/handler.lua
function(cfg, req)
    -- Define a custom response body
    local body = {
        message = "Hello, Nitr!",
        server_time = cfg.server_time,
        request = {
            method = req.method,
            path = req.path,
            uri = req.uri,
            query = req.query,
            headers = req.headers,
            remote_addr = req.remote_addr,
        },
    }

    -- Or use the built-in `fetch` to make an HTTP request
    local client = fetch("get", "https://httpbin.org/ip", headers)
    local resp = client:send()
    local json = resp:json()

    -- Or use the built-in `template` engine
    body = template:render("my_template.j2", {
        ["client_ip"] = json["origin"],
        ["server_time"] = cfg.server_time,
    })

    -- Return the response specifying the status code, headers, and body
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
```

## Name origins

*Niter* or *nitre* is the mineral form of potassium nitrate, KNO3. It is a soft, white, highly soluble mineral found primarily in arid climates or cave deposits.
> https://en.wikipedia.org/wiki/Niter

## Contributions

Unless you explicitly state otherwise, any contribution you intentionally submitted for inclusion in current work, as defined in the Apache-2.0 license, shall be dual licensed as described below, without any additional terms or conditions.

Feel free to submit a [pull request](https://github.com/joseluisq/nitr/pulls) or file an [issue](https://github.com/joseluisq/nitr/issues).

## Community

[SWS Community on Discord](https://discord.gg/VWvtZeWAA7)

## License

This work is primarily distributed under the terms of both the [MIT license](LICENSE-MIT) and the [Apache License (Version 2.0)](LICENSE-APACHE).

© 2024-present [Jose Quintana](https://joseluisq.net)
