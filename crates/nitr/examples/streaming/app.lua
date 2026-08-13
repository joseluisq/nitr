-- Streaming bodies: the response `body` is a function instead of a string.
--
-- Writer callback: `writer:write(chunk)` sends a chunk to the client and
-- suspends while the client is slower than the producer, so a large export
-- never has to fit in the state's memory limit.

local app = nitr.app()

app:get("/report.csv", function(req)
    return {
        status = 200,
        headers = {
            ["Content-Type"] = "text/csv; charset=utf-8",
            ["Content-Disposition"] = 'attachment; filename="report.csv"',
        },
        body = function(writer)
            writer:write("id,name,score\n")
            -- Imagine rows coming from conn:query(...) here: each row is
            -- written as it is produced, never accumulated.
            for i = 1, 1000 do
                writer:write(string.format("%d,user-%d,%d\n", i, i, i * 7 % 100))
            end
        end,
    }
end)

-- Iterator form: any function returning one chunk per call and nil to
-- finish works as a body; coroutine.wrap is the idiomatic way.
app:get("/chunks", function(req)
    return {
        headers = { ["Content-Type"] = "text/plain; charset=utf-8" },
        body = coroutine.wrap(function()
            for i = 1, 5 do
                coroutine.yield("chunk " .. i .. "\n")
            end
        end),
    }
end)

return app
