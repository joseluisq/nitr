//! Crypto and auth primitives for Lua handlers: `nitr.crypto` (hashing,
//! HMAC, random bytes, constant-time comparison, argon2id passwords) and
//! `nitr.auth` (Basic/Bearer `Authorization` header parsing).
//!
//! Primitives, not a framework: everything is implemented in Rust
//! (RustCrypto), and scripts compose them into their own auth flows.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac as _};
use mlua::{Lua, LuaString, ObjectLike as _, Table, Value};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

/// Upper bound for `nitr.crypto.random_bytes(n)`: large enough for any
/// key/nonce/token, small enough that a script cannot use it as an
/// allocation amplifier.
const MAX_RANDOM_BYTES: usize = 64 * 1024;

/// The RNG/password-hash error types have no `std::error::Error` impl
/// here, so their `Display` is carried over manually.
fn rng_err(err: getrandom::Error) -> mlua::Error {
    mlua::Error::RuntimeError(format!("failed to read OS entropy: {err}"))
}

fn pw_err(err: argon2::password_hash::Error) -> mlua::Error {
    mlua::Error::RuntimeError(format!("password hashing failed: {err}"))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Builds the `nitr.crypto` table.
pub(crate) fn create_crypto_table(lua: &Lua) -> mlua::Result<Table> {
    let crypto = lua.create_table()?;

    // Digest and MAC results are lowercase hex strings: printable, easy to
    // compare and log, and what most wire formats expect.
    crypto.set(
        "sha256",
        lua.create_function(|_, data: LuaString| Ok(hex(&Sha256::digest(data.as_bytes()))))?,
    )?;

    crypto.set(
        "hmac_sha256",
        lua.create_function(|_, (key, data): (LuaString, LuaString)| {
            // Never panics: HMAC-SHA256 accepts keys of any length.
            let mut mac = Hmac::<Sha256>::new_from_slice(&key.as_bytes())
                .expect("HMAC accepts any key length");
            mac.update(&data.as_bytes());
            Ok(hex(&mac.finalize().into_bytes()))
        })?,
    )?;

    // Raw bytes (a binary Lua string) from the OS entropy source.
    crypto.set(
        "random_bytes",
        lua.create_function(|lua, n: usize| {
            if n == 0 || n > MAX_RANDOM_BYTES {
                return Err(mlua::Error::RuntimeError(format!(
                    "random_bytes(n) requires 1 <= n <= {MAX_RANDOM_BYTES}, got {n}"
                )));
            }
            let mut buf = vec![0u8; n];
            getrandom::getrandom(&mut buf).map_err(rng_err)?;
            lua.create_string(&buf)
        })?,
    )?;

    // The comparison Lua apps always get wrong: `==` on secrets leaks
    // timing. Length differences still return early — hide lengths by
    // comparing digests when they may vary.
    crypto.set(
        "constant_time_eq",
        lua.create_function(|_, (a, b): (LuaString, LuaString)| {
            let (a, b) = (a.as_bytes(), b.as_bytes());
            Ok(a.len() == b.len() && bool::from(a.ct_eq(&b)))
        })?,
    )?;

    crypto.set(
        "password_hash",
        lua.create_function(|_, password: LuaString| {
            let mut salt = [0u8; 16];
            getrandom::getrandom(&mut salt).map_err(rng_err)?;
            let salt = SaltString::encode_b64(&salt).map_err(pw_err)?;
            let hash = Argon2::default()
                .hash_password(&password.as_bytes(), &salt)
                .map_err(pw_err)?;
            Ok(hash.to_string())
        })?,
    )?;

    crypto.set(
        "password_verify",
        lua.create_function(|_, (password, hash): (LuaString, String)| {
            let Ok(parsed) = PasswordHash::new(&hash) else {
                return Ok(false);
            };
            Ok(Argon2::default()
                .verify_password(&password.as_bytes(), &parsed)
                .is_ok())
        })?,
    )?;

    Ok(crypto)
}

/// Builds the `nitr.auth` table: `basic(req)` returns `user, pass` (or
/// `nil`) and `bearer(req)` returns the token (or `nil`). Both accept the
/// request object or the raw `Authorization` header value.
pub(crate) fn create_auth_table(lua: &Lua) -> mlua::Result<Table> {
    let auth = lua.create_table()?;

    auth.set(
        "basic",
        lua.create_function(|lua, source: Value| {
            let header = authorization(&source)?;
            let Some(encoded) = header.as_deref().and_then(|h| scheme_value(h, "basic")) else {
                return Ok(mlua::MultiValue::new());
            };
            let Some((user, pass)) = B64
                .decode(encoded)
                .ok()
                .and_then(|raw| String::from_utf8(raw).ok())
                .and_then(|creds| {
                    creds
                        .split_once(':')
                        .map(|(u, p)| (u.to_string(), p.to_string()))
                })
            else {
                return Ok(mlua::MultiValue::new());
            };
            let mut out = mlua::MultiValue::new();
            out.push_back(Value::String(lua.create_string(&user)?));
            out.push_back(Value::String(lua.create_string(&pass)?));
            Ok(out)
        })?,
    )?;

    auth.set(
        "bearer",
        lua.create_function(|lua, source: Value| {
            let header = authorization(&source)?;
            match header
                .as_deref()
                .and_then(|h| scheme_value(h, "bearer"))
                .filter(|t| !t.is_empty())
            {
                Some(token) => Ok(Value::String(lua.create_string(token)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    Ok(auth)
}

/// Extracts the `Authorization` header from a request-like value (userdata
/// or table with a `headers` field) or accepts the header string directly.
fn authorization(source: &Value) -> mlua::Result<Option<String>> {
    let headers: Option<Table> = match source {
        Value::String(s) => return Ok(Some(s.to_string_lossy().to_string())),
        Value::UserData(ud) => ud.get("headers").ok(),
        Value::Table(t) => t.get("headers").ok(),
        _ => None,
    };
    Ok(headers.and_then(|h| h.get::<Option<String>>("authorization").ok().flatten()))
}

/// Returns the value part of an `Authorization` header when its scheme
/// matches (case-insensitively), e.g. `Bearer <value>`.
fn scheme_value<'a>(header: &'a str, scheme: &str) -> Option<&'a str> {
    let (found, value) = header.trim().split_once(' ')?;
    found
        .eq_ignore_ascii_case(scheme)
        .then(|| value.trim())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_parsing_is_case_insensitive_and_strict() {
        assert_eq!(scheme_value("Bearer abc", "bearer"), Some("abc"));
        assert_eq!(scheme_value("bearer abc", "bearer"), Some("abc"));
        assert_eq!(
            scheme_value("  Basic dXNlcg==  ", "basic"),
            Some("dXNlcg==")
        );
        assert_eq!(scheme_value("Bearer", "bearer"), None);
        assert_eq!(scheme_value("Bearer ", "bearer"), None);
        assert_eq!(scheme_value("Basic abc", "bearer"), None);
    }

    #[test]
    fn passwords_hash_and_verify() {
        let lua = Lua::new();
        let crypto = create_crypto_table(&lua).expect("crypto table");
        let hash: String = crypto
            .get::<mlua::Function>("password_hash")
            .expect("fn")
            .call("hunter2")
            .expect("hash");
        assert!(hash.starts_with("$argon2id$"), "got: {hash}");

        let verify: mlua::Function = crypto.get("password_verify").expect("fn");
        assert!(verify.call::<bool>(("hunter2", hash.clone())).expect("ok"));
        assert!(!verify.call::<bool>(("wrong", hash)).expect("ok"));
        assert!(
            !verify
                .call::<bool>(("hunter2", "not-a-hash".to_string()))
                .expect("ok")
        );
    }

    #[test]
    fn digests_are_hex_and_deterministic() {
        let lua = Lua::new();
        let crypto = create_crypto_table(&lua).expect("crypto table");
        let sha256: mlua::Function = crypto.get("sha256").expect("fn");
        assert_eq!(
            sha256.call::<String>("abc").expect("digest"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let eq: mlua::Function = crypto.get("constant_time_eq").expect("fn");
        assert!(eq.call::<bool>(("same", "same")).expect("eq"));
        assert!(!eq.call::<bool>(("same", "diff")).expect("eq"));
        assert!(!eq.call::<bool>(("same", "longer-value")).expect("eq"));
    }
}
