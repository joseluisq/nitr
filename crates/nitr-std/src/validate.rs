//! Declarative request validation: `nitr.validate.schema({...})` compiles
//! a schema once at load time; `schema:check(value)` then validates
//! untrusted input in Rust, per request, and reports every failing field.
//!
//! Deliberately not JSON Schema: a small declarative subset covers what a
//! small API needs, and validating input is exactly the "predictable,
//! fast, secure" work that belongs on the Rust side of the boundary.

use mlua::{Lua, Table, UserData, UserDataMethods, Value};

/// The value types a rule can require.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    String,
    Number,
    Integer,
    Boolean,
    Array,
    Table,
}

impl Kind {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "string" => Self::String,
            "number" => Self::Number,
            "integer" => Self::Integer,
            "boolean" => Self::Boolean,
            "array" => Self::Array,
            "table" => Self::Table,
            _ => None?,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Table => "table",
        }
    }
}

/// String formats with one careful, dependency-free Rust implementation
/// each: syntactic sanity checks, not full RFC validation.
#[derive(Debug, Clone, Copy)]
enum Format {
    Email,
    Uuid,
    Url,
    Ip,
    Ipv4,
    Ipv6,
    Hostname,
    Date,
    Datetime,
    Hex,
    Base64,
    Alphanumeric,
    Slug,
}

/// Every recognized format, for the compile-time error message.
const FORMATS: &[(&str, Format)] = &[
    ("email", Format::Email),
    ("uuid", Format::Uuid),
    ("url", Format::Url),
    ("ip", Format::Ip),
    ("ipv4", Format::Ipv4),
    ("ipv6", Format::Ipv6),
    ("hostname", Format::Hostname),
    ("date", Format::Date),
    ("datetime", Format::Datetime),
    ("hex", Format::Hex),
    ("base64", Format::Base64),
    ("alphanumeric", Format::Alphanumeric),
    ("slug", Format::Slug),
];

/// One DNS label: 1–63 chars, alphanumeric plus inner hyphens.
fn is_hostname_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

fn is_hostname(value: &str) -> bool {
    !value.is_empty() && value.len() <= 253 && value.split('.').all(is_hostname_label)
}

impl Format {
    fn parse(name: &str) -> Option<Self> {
        FORMATS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, format)| *format)
    }

    fn check(self, value: &str) -> bool {
        match self {
            Self::Email => {
                let Some((local, domain)) = value.split_once('@') else {
                    return false;
                };
                !local.is_empty()
                    && local.len() <= 64
                    && domain.contains('.')
                    && is_hostname(domain)
                    && !local.contains(|c: char| c.is_whitespace() || c.is_control())
            }
            Self::Uuid => {
                let groups: Vec<&str> = value.split('-').collect();
                groups.len() == 5
                    && groups
                        .iter()
                        .zip([8usize, 4, 4, 4, 12])
                        .all(|(g, len)| g.len() == len && g.chars().all(|c| c.is_ascii_hexdigit()))
            }
            Self::Url => {
                let rest = value
                    .strip_prefix("http://")
                    .or_else(|| value.strip_prefix("https://"));
                matches!(rest, Some(rest) if !rest.is_empty() && !rest.starts_with('/'))
                    && !value.contains(|c: char| c.is_whitespace() || c.is_control())
            }
            Self::Ip => value.parse::<std::net::IpAddr>().is_ok(),
            Self::Ipv4 => value.parse::<std::net::Ipv4Addr>().is_ok(),
            Self::Ipv6 => value.parse::<std::net::Ipv6Addr>().is_ok(),
            Self::Hostname => is_hostname(value),
            Self::Date => chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok(),
            Self::Datetime => chrono::DateTime::parse_from_rfc3339(value).is_ok(),
            Self::Hex => !value.is_empty() && value.chars().all(|c| c.is_ascii_hexdigit()),
            Self::Base64 => {
                use base64::Engine as _;
                !value.is_empty()
                    && base64::engine::general_purpose::STANDARD
                        .decode(value)
                        .is_ok()
            }
            Self::Alphanumeric => {
                !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric())
            }
            Self::Slug => {
                !value.is_empty()
                    && value
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !value.starts_with('-')
                    && !value.ends_with('-')
            }
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Email => "an email address",
            Self::Uuid => "a UUID",
            Self::Url => "an http(s) URL",
            Self::Ip => "an IP address",
            Self::Ipv4 => "an IPv4 address",
            Self::Ipv6 => "an IPv6 address",
            Self::Hostname => "a hostname",
            Self::Date => "a date (YYYY-MM-DD)",
            Self::Datetime => "an RFC 3339 datetime",
            Self::Hex => "a hex string",
            Self::Base64 => "a base64 string",
            Self::Alphanumeric => "letters and digits only",
            Self::Slug => "a slug (lowercase letters, digits, hyphens)",
        }
    }
}

/// A literal a `one_of` list can hold.
#[derive(Debug, Clone, PartialEq)]
enum Literal {
    String(String),
    Number(f64),
}

impl std::fmt::Display for Literal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String(s) => write!(f, "\"{s}\""),
            Self::Number(n) => write!(f, "{n}"),
        }
    }
}

/// One compiled field rule.
#[derive(Debug)]
struct Rule {
    kind: Kind,
    required: bool,
    // Numbers.
    min: Option<f64>,
    max: Option<f64>,
    // Strings.
    min_len: Option<usize>,
    max_len: Option<usize>,
    format: Option<Format>,
    // Strings and numbers.
    one_of: Option<Vec<Literal>>,
    // Arrays.
    items: Option<Box<Rule>>,
    min_items: Option<usize>,
    max_items: Option<usize>,
    // Nested tables.
    fields: Option<Vec<(String, Rule)>>,
}

/// Raises a schema-compilation error naming the field it is about.
fn bad_schema(path: &str, msg: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::RuntimeError(format!("invalid schema for `{path}`: {msg}"))
}

/// Which rule keys apply to which type — anything else in a rule table is
/// an error, so a typo (`requird`, `maxlen`) fails at load time instead of
/// silently validating nothing.
fn allowed_keys(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::String => &["type", "required", "min_len", "max_len", "format", "one_of"],
        Kind::Number | Kind::Integer => &["type", "required", "min", "max", "one_of"],
        Kind::Boolean => &["type", "required"],
        Kind::Array => &["type", "required", "items", "min_items", "max_items"],
        Kind::Table => &["type", "required", "fields"],
    }
}

fn compile_rule(rule: &Table, path: &str) -> mlua::Result<Rule> {
    let type_name: String = rule
        .get::<Option<String>>("type")?
        .ok_or_else(|| bad_schema(path, "missing `type`"))?;
    let kind = Kind::parse(&type_name).ok_or_else(|| {
        bad_schema(
            path,
            format!(
                "unknown type `{type_name}` (expected string, number, integer, boolean, array or table)"
            ),
        )
    })?;

    for pair in rule.pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::String(key) = key else {
            return Err(bad_schema(path, "rule keys must be strings"));
        };
        let key = key.to_string_lossy();
        if !allowed_keys(kind).contains(&key.as_ref()) {
            return Err(bad_schema(
                path,
                format!(
                    "unknown rule `{key}` for type `{}` (allowed: {})",
                    kind.name(),
                    allowed_keys(kind).join(", ")
                ),
            ));
        }
    }

    let format = match rule.get::<Option<String>>("format")? {
        Some(name) => Some(Format::parse(&name).ok_or_else(|| {
            let known: Vec<&str> = FORMATS.iter().map(|(n, _)| *n).collect();
            bad_schema(
                path,
                format!(
                    "unknown format `{name}` (expected one of: {})",
                    known.join(", ")
                ),
            )
        })?),
        None => None,
    };

    let one_of = match rule.get::<Option<Table>>("one_of")? {
        Some(list) => {
            let mut literals = Vec::new();
            for value in list.sequence_values::<Value>() {
                literals.push(match value? {
                    Value::String(s) => Literal::String(s.to_string_lossy().to_string()),
                    Value::Integer(n) => Literal::Number(n as f64),
                    Value::Number(n) => Literal::Number(n),
                    other => {
                        return Err(bad_schema(
                            path,
                            format!(
                                "`one_of` entries must be strings or numbers, got {}",
                                other.type_name()
                            ),
                        ));
                    }
                });
            }
            if literals.is_empty() {
                return Err(bad_schema(path, "`one_of` must not be empty"));
            }
            Some(literals)
        }
        None => None,
    };

    let items = match rule.get::<Option<Table>>("items")? {
        Some(items) => Some(Box::new(compile_rule(&items, &format!("{path}[]"))?)),
        None if kind == Kind::Array => {
            return Err(bad_schema(path, "type `array` requires `items`"));
        }
        None => None,
    };

    let fields = match rule.get::<Option<Table>>("fields")? {
        Some(fields) => Some(compile_fields(&fields, path)?),
        None if kind == Kind::Table => {
            return Err(bad_schema(path, "type `table` requires `fields`"));
        }
        None => None,
    };

    Ok(Rule {
        kind,
        required: rule.get::<Option<bool>>("required")?.unwrap_or(false),
        min: rule.get("min")?,
        max: rule.get("max")?,
        min_len: rule.get("min_len")?,
        max_len: rule.get("max_len")?,
        format,
        one_of,
        items,
        min_items: rule.get("min_items")?,
        max_items: rule.get("max_items")?,
        fields,
    })
}

/// Compiles a `{ name = rule, ... }` map, sorted so error output and
/// validation order are deterministic.
fn compile_fields(fields: &Table, path: &str) -> mlua::Result<Vec<(String, Rule)>> {
    let mut compiled = Vec::new();
    for pair in fields.pairs::<Value, Value>() {
        let (name, rule) = pair?;
        let Value::String(name) = name else {
            return Err(bad_schema(path, "field names must be strings"));
        };
        let name = name.to_string_lossy().to_string();
        let field_path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}.{name}")
        };
        let Value::Table(rule) = rule else {
            return Err(bad_schema(&field_path, "the rule must be a table"));
        };
        compiled.push((name, compile_rule(&rule, &field_path)?));
    }
    compiled.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(compiled)
}

/// Validates one value against a rule. On success returns the value to
/// place in the output (tables are rebuilt with only declared fields); on
/// failure records a message under `path` and returns `None`.
fn check_value(
    lua: &Lua,
    rule: &Rule,
    value: Value,
    path: &str,
    errors: &Table,
) -> mlua::Result<Option<Value>> {
    let fail = |msg: String| -> mlua::Result<Option<Value>> {
        errors.set(path, msg)?;
        Ok(None)
    };

    match rule.kind {
        Kind::String => {
            let Value::String(s) = &value else {
                return fail(format!("must be a string, got {}", value.type_name()));
            };
            let s = s.to_string_lossy().to_string();
            let len = s.chars().count();
            if let Some(min) = rule.min_len
                && len < min
            {
                return fail(format!("must be at least {min} characters"));
            }
            if let Some(max) = rule.max_len
                && len > max
            {
                return fail(format!("must be at most {max} characters"));
            }
            if let Some(format) = rule.format
                && !format.check(&s)
            {
                return fail(format!("must be {}", format.describe()));
            }
            if let Some(one_of) = &rule.one_of
                && !one_of.contains(&Literal::String(s.clone()))
            {
                return fail(format!("must be one of: {}", literals(one_of)));
            }
            Ok(Some(value))
        }
        Kind::Number | Kind::Integer => {
            let n = match &value {
                Value::Integer(n) => *n as f64,
                Value::Number(n) => *n,
                other => {
                    return fail(format!("must be a number, got {}", other.type_name()));
                }
            };
            if rule.kind == Kind::Integer && n.fract() != 0.0 {
                return fail("must be an integer".into());
            }
            if let Some(min) = rule.min
                && n < min
            {
                return fail(format!("must be >= {min}"));
            }
            if let Some(max) = rule.max
                && n > max
            {
                return fail(format!("must be <= {max}"));
            }
            if let Some(one_of) = &rule.one_of
                && !one_of.contains(&Literal::Number(n))
            {
                return fail(format!("must be one of: {}", literals(one_of)));
            }
            Ok(Some(value))
        }
        Kind::Boolean => match value {
            Value::Boolean(_) => Ok(Some(value)),
            other => fail(format!("must be a boolean, got {}", other.type_name())),
        },
        Kind::Array => {
            let Value::Table(t) = &value else {
                return fail(format!("must be an array, got {}", value.type_name()));
            };
            let len = t.raw_len();
            if let Some(min) = rule.min_items
                && len < min
            {
                return fail(format!("must have at least {min} items"));
            }
            if let Some(max) = rule.max_items
                && len > max
            {
                return fail(format!("must have at most {max} items"));
            }
            let items = rule.items.as_ref().expect("array rules carry `items`");
            let out = lua.create_table_with_capacity(len, 0)?;
            let mut ok = true;
            for i in 1..=len {
                let item: Value = t.raw_get(i)?;
                match check_value(lua, items, item, &format!("{path}[{i}]"), errors)? {
                    Some(item) => out.raw_set(i, item)?,
                    None => ok = false,
                }
            }
            Ok(ok.then_some(Value::Table(out)))
        }
        Kind::Table => {
            let Value::Table(t) = &value else {
                return fail(format!("must be a table, got {}", value.type_name()));
            };
            let fields = rule.fields.as_ref().expect("table rules carry `fields`");
            check_fields(lua, fields, t, path, errors).map(|v| v.map(Value::Table))
        }
    }
}

/// Validates a table against a field map, building the output table with
/// only the declared fields — undeclared input never passes through, so a
/// handler cannot be mass-assigned a field the schema never mentioned.
fn check_fields(
    lua: &Lua,
    fields: &[(String, Rule)],
    input: &Table,
    path: &str,
    errors: &Table,
) -> mlua::Result<Option<Table>> {
    let out = lua.create_table()?;
    let mut ok = true;
    for (name, rule) in fields {
        let field_path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}.{name}")
        };
        let value: Value = input.get(name.as_str())?;
        if value.is_nil() {
            if rule.required {
                errors.set(field_path, "is required")?;
                ok = false;
            }
            continue;
        }
        match check_value(lua, rule, value, &field_path, errors)? {
            Some(value) => out.set(name.as_str(), value)?,
            None => ok = false,
        }
    }
    Ok(ok.then_some(out))
}

fn literals(list: &[Literal]) -> String {
    list.iter()
        .map(Literal::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A compiled schema: the field rules live in Rust, so `check` walks the
/// input once with no per-request compilation.
struct LuaSchema {
    fields: Vec<(String, Rule)>,
}

impl UserData for LuaSchema {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // schema:check(value) -> data, nil | nil, { message, fields }
        //
        // `data` contains only the declared fields; `fields` maps each
        // failing field path (`email`, `address.city`, `tags[2]`) to its
        // message, ready to serialize into a 422 body.
        methods.add_method("check", |lua, this, value: Value| {
            let errors = lua.create_table()?;
            let checked = match &value {
                Value::Table(input) => check_fields(lua, &this.fields, input, "", &errors)?,
                other => {
                    errors.set("$", format!("must be a table, got {}", other.type_name()))?;
                    None
                }
            };
            match checked {
                Some(data) => Ok((Value::Table(data), Value::Nil)),
                None => {
                    let err = lua.create_table()?;
                    err.set("message", "validation failed")?;
                    err.set("fields", errors)?;
                    Ok((Value::Nil, Value::Table(err)))
                }
            }
        });
    }
}

/// Builds the `nitr.validate` table.
pub(crate) fn create_validate_table(lua: &Lua) -> mlua::Result<Table> {
    let validate = lua.create_table()?;
    validate.set(
        "schema",
        lua.create_function(|_, fields: Table| {
            Ok(LuaSchema {
                fields: compile_fields(&fields, "")?,
            })
        })?,
    )?;
    Ok(validate)
}

#[cfg(test)]
mod tests {
    use mlua::ObjectLike as _;

    use super::*;

    fn schema(lua: &Lua, def: &str) -> mlua::AnyUserData {
        let validate = create_validate_table(lua).expect("table");
        let fields: Table = lua.load(def).eval().expect("schema table");
        validate
            .get::<mlua::Function>("schema")
            .expect("fn")
            .call(fields)
            .expect("compile")
    }

    fn check(lua: &Lua, schema: &mlua::AnyUserData, input: &str) -> (Value, Value) {
        let value: Value = lua.load(input).eval().expect("input");
        let f: mlua::Function = schema.get("check").expect("method");
        f.call((schema, value)).expect("check")
    }

    #[test]
    fn valid_input_passes_and_is_stripped_to_declared_fields() {
        let lua = Lua::new();
        let s = schema(
            &lua,
            r#"{
                email = { type = "string", format = "email", required = true },
                age = { type = "integer", min = 0, max = 150 },
                tags = { type = "array", items = { type = "string" }, max_items = 3 },
            }"#,
        );
        let (data, err) = check(
            &lua,
            &s,
            r#"{ email = "ada@example.com", age = 36, tags = {"math"}, role = "admin" }"#,
        );
        assert!(err.is_nil(), "unexpected error: {err:?}");
        let Value::Table(data) = data else {
            panic!("expected data table");
        };
        assert_eq!(data.get::<String>("email").unwrap(), "ada@example.com");
        assert_eq!(data.get::<i64>("age").unwrap(), 36);
        // Undeclared fields never pass through.
        assert!(data.get::<Value>("role").unwrap().is_nil());
    }

    #[test]
    fn failures_report_every_field_with_its_path() {
        let lua = Lua::new();
        let s = schema(
            &lua,
            r#"{
                email = { type = "string", format = "email", required = true },
                age = { type = "integer", min = 0 },
                tags = { type = "array", items = { type = "string", max_len = 4 } },
                home = { type = "table", fields = { city = { type = "string", required = true } } },
            }"#,
        );
        let (data, err) = check(
            &lua,
            &s,
            r#"{ age = -3, tags = {"ok", "toolong"}, home = {} }"#,
        );
        assert!(data.is_nil());
        let Value::Table(err) = err else {
            panic!("expected error table");
        };
        assert_eq!(err.get::<String>("message").unwrap(), "validation failed");
        let fields: Table = err.get("fields").expect("fields");
        assert_eq!(fields.get::<String>("email").unwrap(), "is required");
        assert_eq!(fields.get::<String>("age").unwrap(), "must be >= 0");
        assert_eq!(
            fields.get::<String>("tags[2]").unwrap(),
            "must be at most 4 characters"
        );
        assert_eq!(fields.get::<String>("home.city").unwrap(), "is required");
    }

    #[test]
    fn schema_typos_fail_at_compile_time() {
        let lua = Lua::new();
        let validate = create_validate_table(&lua).expect("table");
        let compile: mlua::Function = validate.get("schema").expect("fn");

        for (def, needle) in [
            (
                r#"{ a = { type = "string", requird = true } }"#,
                "unknown rule `requird`",
            ),
            (r#"{ a = { required = true } }"#, "missing `type`"),
            (r#"{ a = { type = "text" } }"#, "unknown type `text`"),
            (r#"{ a = { type = "array" } }"#, "requires `items`"),
            (
                r#"{ a = { type = "string", format = "phone" } }"#,
                "unknown format `phone`",
            ),
            (
                r#"{ a = { type = "number", min_len = 2 } }"#,
                "unknown rule `min_len`",
            ),
        ] {
            let fields: Table = lua.load(def).eval().expect("def");
            let err = compile.call::<Value>(fields).expect_err(def).to_string();
            assert!(err.contains(needle), "`{def}` -> {err}");
        }
    }

    #[test]
    fn one_of_booleans_and_numeric_kinds() {
        let lua = Lua::new();
        let s = schema(
            &lua,
            r#"{
                role = { type = "string", one_of = { "admin", "user" } },
                level = { type = "integer", one_of = { 1, 2, 3 } },
                score = { type = "number", min = 0 },
                active = { type = "boolean" },
            }"#,
        );

        let (data, err) = check(
            &lua,
            &s,
            r#"{ role = "admin", level = 2, score = 7.5, active = false }"#,
        );
        assert!(err.is_nil(), "unexpected error: {err:?}");
        let Value::Table(data) = data else {
            panic!("expected data table");
        };
        // A false boolean survives (the `nil`-vs-`false` classic).
        assert!(!data.get::<bool>("active").unwrap());
        assert_eq!(data.get::<f64>("score").unwrap(), 7.5);

        let (data, err) = check(
            &lua,
            &s,
            r#"{ role = "root", level = 2.5, score = "high", active = 1 }"#,
        );
        assert!(data.is_nil());
        let Value::Table(err) = err else {
            panic!("expected error table");
        };
        let fields: Table = err.get("fields").expect("fields");
        assert_eq!(
            fields.get::<String>("role").unwrap(),
            r#"must be one of: "admin", "user""#
        );
        assert_eq!(fields.get::<String>("level").unwrap(), "must be an integer");
        assert_eq!(
            fields.get::<String>("score").unwrap(),
            "must be a number, got string"
        );
        assert_eq!(
            fields.get::<String>("active").unwrap(),
            "must be a boolean, got integer"
        );
    }

    #[test]
    fn arrays_of_tables_nest_and_non_table_input_is_reported() {
        let lua = Lua::new();
        let s = schema(
            &lua,
            r#"{
                points = {
                    type = "array",
                    items = { type = "table", fields = {
                        x = { type = "number", required = true },
                        y = { type = "number", required = true },
                    } },
                },
            }"#,
        );

        let (data, err) = check(&lua, &s, r#"{ points = { { x = 1, y = 2 }, { x = 3 } } }"#);
        assert!(data.is_nil());
        let Value::Table(err) = err else {
            panic!("expected error table");
        };
        let fields: Table = err.get("fields").expect("fields");
        assert_eq!(fields.get::<String>("points[2].y").unwrap(), "is required");

        // Non-table input fails with the `$` root marker instead of a
        // Lua error.
        let (data, err) = check(&lua, &s, r#""not a table""#);
        assert!(data.is_nil());
        let Value::Table(err) = err else {
            panic!("expected error table");
        };
        let fields: Table = err.get("fields").expect("fields");
        assert_eq!(
            fields.get::<String>("$").unwrap(),
            "must be a table, got string"
        );
    }

    #[test]
    fn optional_fields_are_simply_absent() {
        let lua = Lua::new();
        let s = schema(&lua, r#"{ nick = { type = "string", min_len = 2 } }"#);
        let (data, err) = check(&lua, &s, "{}");
        assert!(err.is_nil());
        let Value::Table(data) = data else {
            panic!("expected data table");
        };
        assert!(data.get::<Value>("nick").unwrap().is_nil());
        // …but when present, the rules still apply.
        let (data, _) = check(&lua, &s, r#"{ nick = "a" }"#);
        assert!(data.is_nil());
    }

    #[test]
    fn formats_accept_and_reject_sensibly() {
        for (ok, value) in [
            (true, "ada@example.com"),
            (false, "ada@nodot"),
            (false, "@example.com"),
            (false, "two words@example.com"),
        ] {
            assert_eq!(Format::Email.check(value), ok, "email {value}");
        }
        assert!(Format::Uuid.check("0198c5b6-1f6a-7abc-9def-0123456789ab"));
        assert!(!Format::Uuid.check("not-a-uuid"));
        assert!(Format::Url.check("https://example.com/x"));
        assert!(!Format::Url.check("ftp://example.com"));
        assert!(!Format::Url.check("https:///nohost"));
        assert!(Format::Ip.check("192.168.1.1"));
        assert!(Format::Ip.check("::1"));
        assert!(Format::Ipv4.check("10.0.0.1") && !Format::Ipv4.check("::1"));
        assert!(Format::Ipv6.check("2001:db8::1") && !Format::Ipv6.check("10.0.0.1"));
        assert!(Format::Hostname.check("api.example.com"));
        assert!(!Format::Hostname.check("-bad.example.com"));
        assert!(Format::Date.check("2026-08-17") && !Format::Date.check("2026-13-01"));
        assert!(Format::Datetime.check("2026-08-17T10:00:00Z"));
        assert!(!Format::Datetime.check("2026-08-17"));
        assert!(Format::Hex.check("deadBEEF42") && !Format::Hex.check("xyz"));
        assert!(Format::Base64.check("aGVsbG8=") && !Format::Base64.check("!!!"));
        assert!(Format::Alphanumeric.check("abc123") && !Format::Alphanumeric.check("a b"));
        assert!(Format::Slug.check("my-post-42"));
        assert!(!Format::Slug.check("My Post") && !Format::Slug.check("-lead"));
    }
}
