use mlua::{
    AnyUserData, ExternalResult, Lua, LuaSerdeExt, LuaString, MetaMethod, Table, UserData,
    UserDataMethods, Value,
};
use serde_json::Value as SerdeValue;

#[derive(Default)]
pub(crate) struct LuaJson;

impl UserData for LuaJson {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("encode", |lua, _, input: Value| {
            let s = serde_json::to_string(&input).into_lua_err()?;
            lua.to_value(&s)
        });

        methods.add_method_mut("decode", |lua, _, input: LuaString| {
            let v = serde_json::from_slice::<SerdeValue>(&input.as_bytes()).into_lua_err()?;
            lua.to_value(&v)
        });

        // Calling the userdata itself — `json({ ... })` — is the JSON
        // response helper: it returns a `{status, headers, body}` table.
        methods.add_meta_method(
            MetaMethod::Call,
            |lua, _, (value, status): (Value, Option<u16>)| {
                let body = serde_json::to_string(&value).into_lua_err()?;
                let table = crate::http::response_table(lua, status.unwrap_or(200))?;
                table
                    .get::<Table>("headers")?
                    .set("Content-Type", "application/json")?;
                table.set("body", body)?;
                Ok(table)
            },
        );
    }
}

/// JSON encode function via Serde.
pub(crate) fn create_json_fn(lua: &Lua) -> mlua::Result<AnyUserData> {
    lua.create_userdata(LuaJson)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::ObjectLike as _;

    #[test]
    fn encodes_decodes_and_builds_responses() {
        let lua = Lua::new();
        let json = create_json_fn(&lua).expect("json");
        let decoded: Table = json
            .call_method("decode", r#"{"a": 1, "b": [true, "x"]}"#)
            .expect("decode");
        assert_eq!(decoded.get::<i64>("a").expect("a"), 1);
        let encoded: String = json.call_method("encode", decoded).expect("encode");
        let value: SerdeValue = serde_json::from_str(&encoded).expect("parse");
        assert_eq!(value["b"][1], "x");

        // `json(value, status?)` is the response helper.
        let resp: Table = json
            .call((lua.create_table().expect("t"), 201))
            .expect("call");
        assert_eq!(resp.get::<u16>("status").expect("status"), 201);
    }

    /// A JSON tree without the shapes Lua cannot represent faithfully:
    /// no nulls (nil erases table entries) and no empty containers (an
    /// empty Lua table is ambiguous between `{}` and `[]`).
    fn json_value() -> impl proptest::prelude::Strategy<Value = SerdeValue> {
        use proptest::prelude::*;
        let leaf = prop_oneof![
            any::<bool>().prop_map(SerdeValue::from),
            any::<i32>().prop_map(SerdeValue::from),
            "[ -~]{0,20}".prop_map(SerdeValue::from),
        ];
        leaf.prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 1..4).prop_map(SerdeValue::from),
                proptest::collection::btree_map("[a-z]{1,6}", inner, 1..4)
                    .prop_map(|m| SerdeValue::Object(m.into_iter().collect())),
            ]
        })
    }

    proptest::proptest! {
        /// Property: decode(encode(decode(json))) is a fixed point — a
        /// JSON document survives the trip through Lua values.
        #[test]
        fn prop_json_round_trips_through_lua(tree in json_value()) {
            let lua = Lua::new();
            let json = create_json_fn(&lua).expect("json");
            let text = serde_json::to_string(&tree).expect("serialize");
            let decoded: Value = json.call_method("decode", text).expect("decode");
            let encoded: String = json.call_method("encode", decoded).expect("encode");
            let back: SerdeValue = serde_json::from_str(&encoded).expect("parse");
            proptest::prop_assert_eq!(back, tree);
        }
    }
}
