-- Handler for the `hello` example: builds the app once, routes run per request.
-- `nitr.ext.hello` is the custom Rust module registered via ServerBuilder::module().
local app = nitr.app()

app:get("/", function(req)
    return {
        status = 200,
        headers = { ["Content-Type"] = "application/json" },
        body = nitr.json:encode({
            greeting = nitr.ext.hello.greet(req.query.name or "world"),
            method = req.method,
            path = req.path,
        }),
    }
end)

return app
