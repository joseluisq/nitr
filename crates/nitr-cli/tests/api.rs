//! Guards for the single-source API description (`nitr-api.toml`):
//!
//! - completeness: every entry registered on the `nitr` namespace must be
//!   described, so adding a builtin without documenting it fails here;
//! - drift: the checked-in generated files (`nitr-types.lua`,
//!   `docs/nitr-api.md`) must match what the description generates.
//!   Regenerate with: NITR_API_REGEN=1 cargo test -p nitr-cli --test api
//!
//! The generator lives in the binary crate; tests reach it through the
//! `nitr types`-shaped internals compiled into the test via include.

use std::collections::BTreeSet;
use std::path::PathBuf;

#[path = "../src/apidef.rs"]
mod apidef;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// Walks the registered `nitr` namespace: top-level entries plus the
/// members of plain Lua tables, recursively. Userdata members are not
/// enumerable, so their methods are covered by declaration only.
fn registered_paths(lua: &mlua::Lua) -> BTreeSet<String> {
    fn walk(prefix: &str, table: &mlua::Table, out: &mut BTreeSet<String>) {
        for pair in table.pairs::<String, mlua::Value>() {
            let Ok((key, value)) = pair else { continue };
            if key.starts_with('_') {
                continue;
            }
            let path = format!("{prefix}.{key}");
            out.insert(path.clone());
            if let mlua::Value::Table(inner) = value {
                walk(&path, &inner, out);
            }
        }
    }
    let nitr: mlua::Table = lua.globals().get("nitr").expect("nitr table");
    let mut out = BTreeSet::new();
    walk("nitr", &nitr, &mut out);
    out
}

#[test]
fn every_registered_entry_is_described() {
    let api = apidef::parse().expect("parse nitr-api.toml");
    let known = api.known_paths();

    let lua = mlua::Lua::new();
    let env = nitr::BuiltinsEnv {
        templates_dir: Some(std::env::temp_dir()),
        database: Some(
            std::env::temp_dir().join(format!("nitr-api-test-{}.db", std::process::id())),
        ),
        ..Default::default()
    };
    nitr::stdlib::register_builtins(&lua, nitr::Builtins::all(), &env)
        .expect("register all builtins");

    let missing: Vec<String> = registered_paths(&lua)
        .into_iter()
        .filter(|path| !known.contains(path))
        .collect();
    assert!(
        missing.is_empty(),
        "registered but not described in nitr-api.toml (document them there): {missing:?}"
    );
}

#[test]
fn generated_files_are_current() {
    let api = apidef::parse().expect("parse nitr-api.toml");
    let outputs = [
        (repo_root().join("nitr-types.lua"), apidef::emit_types(&api)),
        (
            repo_root().join("docs/nitr-api.md"),
            apidef::emit_docs(&api),
        ),
    ];

    if std::env::var_os("NITR_API_REGEN").is_some() {
        for (path, content) in &outputs {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, content).expect("write generated file");
            println!("regenerated {}", path.display());
        }
        return;
    }

    for (path, expected) in &outputs {
        let on_disk = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            &on_disk,
            expected,
            "{} is stale — regenerate with: NITR_API_REGEN=1 cargo test -p nitr-cli --test api",
            path.display()
        );
    }
}
