use mlua::{IntoLua, LuaSerdeExt};
use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, BitXor, BitXorAssign};

pub(crate) mod db;
pub(crate) mod fetch;
pub(crate) mod json;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod template;
pub(crate) mod utils;

pub const USERDATA_LIBS: &[UserData] = &[
    UserData::NONE,
    UserData::DEBUG,
    UserData::FETCH,
    UserData::TEMPLATE,
    UserData::JSON,
    UserData::DATABASE,
    UserData::ALL,
];

#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UserData(u32);

impl UserData {
    pub const NONE: UserData = UserData(0);
    pub const DEBUG: UserData = UserData(1);
    pub const FETCH: UserData = UserData(1 << 1);
    pub const TEMPLATE: UserData = UserData(1 << 2);
    pub const JSON: UserData = UserData(1 << 3);
    pub const DATABASE: UserData = UserData(1 << 4);
    pub const ALL: UserData = UserData(u32::MAX);

    pub fn contains(self, lib: Self) -> bool {
        (self & lib).0 != 0
    }

    pub fn is_all(self) -> bool {
        self == UserData::ALL
    }

    pub fn is_none(self) -> bool {
        self == UserData::NONE
    }

    pub fn to_str(self) -> &'static str {
        match self {
            UserData::NONE => "none",
            UserData::DEBUG => "dbg",
            UserData::FETCH => "fetch",
            UserData::TEMPLATE => "template",
            UserData::JSON => "json",
            UserData::DATABASE => "conn",
            UserData::ALL => "all",
            _ => "",
        }
    }
}

impl BitAnd for UserData {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self::Output {
        UserData(self.0 & rhs.0)
    }
}

impl BitAndAssign for UserData {
    fn bitand_assign(&mut self, rhs: Self) {
        *self = UserData(self.0 & rhs.0)
    }
}

impl BitOr for UserData {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        UserData(self.0 | rhs.0)
    }
}

impl BitOrAssign for UserData {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = UserData(self.0 | rhs.0)
    }
}

impl BitXor for UserData {
    type Output = Self;
    fn bitxor(self, rhs: Self) -> Self::Output {
        UserData(self.0 ^ rhs.0)
    }
}

impl BitXorAssign for UserData {
    fn bitxor_assign(&mut self, rhs: Self) {
        *self = UserData(self.0 ^ rhs.0)
    }
}

// implement to_str for LuaUserData
impl std::fmt::Display for UserData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut libs = Vec::new();
        if self.contains(UserData::DEBUG) {
            libs.push("DEBUG");
        }
        if self.contains(UserData::FETCH) {
            libs.push("FETCH");
        }
        if self.contains(UserData::TEMPLATE) {
            libs.push("TEMPLATE");
        }
        if self.contains(UserData::JSON) {
            libs.push("JSON");
        }
        if self.contains(UserData::DATABASE) {
            libs.push("database");
        }
        write!(f, "{}", libs.join(", "))
    }
}

impl IntoLua for UserData {
    fn into_lua(self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
        lua.to_value(self.to_str())
    }
}
