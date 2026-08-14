# Nitr application package

The conventional layout the `nitr` CLI works with:

```
app-package/
├── nitr.toml       server + app configuration
├── app.lua         routes and middleware (returns nitr.app())
├── config.lua      runs once at startup (schema setup); result → nitr.cfg
├── public/         static files, served by Rust
└── tests/          *.lua files for `nitr test`
```

From the repository root:

```sh
cargo run -p nitr-cli -- -c crates/nitr/examples/app-package/nitr.toml check
cargo run -p nitr-cli -- -c crates/nitr/examples/app-package/nitr.toml test
cargo run -p nitr-cli -- -c crates/nitr/examples/app-package/nitr.toml run
```

In your own project you would just run `nitr check` / `nitr test` /
`nitr dev` next to `nitr.toml` (scaffold one with `nitr init`).
Send `SIGHUP` to a running server for a zero-downtime reload.
