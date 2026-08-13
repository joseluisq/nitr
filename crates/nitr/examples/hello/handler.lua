-- Handler for the `hello` example: runs once per request.
-- `greet` is the custom Rust function registered via ServerBuilder::setup().
function(cfg, req)
    return {
        status = 200,
        headers = { ["Content-Type"] = "application/json" },
        body = json:encode({
            greeting = greet(req.query.name or "world"),
            method = req.method,
            path = req.path,
        }),
    }
end
