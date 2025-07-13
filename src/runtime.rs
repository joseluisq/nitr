use anyhow::anyhow;
use mlua::{FromLua, Function, IntoLua, IntoLuaMulti, Lua, LuaOptions, RegistryKey, StdLib, Table};
use std::path::{Path, PathBuf};

use crate::error::{Context, Result};
use crate::userdata::{db, fetch, json, template, utils, UserData, USERDATA_LIBS};

const MEMORY_LIMIT: usize = 1024 * 1024; // 1MB

/// The Lua runtime that provides an interface to execute Lua scripts and manage Lua state.
/// It allows for registering global functions, configuration scripts, and HTTP handlers.
#[derive(Debug)]
pub struct Runtime {
    lua: Lua,
    cfg: Option<Table>,
    http_fn: Option<Function>,
    http_fn_key: Option<RegistryKey>,
    http_fn_path: Option<PathBuf>,
    opts: RuntimeOpts,
}

/// Options for configuring the Lua runtime.
#[derive(Debug)]
pub struct RuntimeOpts {
    pub libs: mlua::StdLib,
    pub memory_limit: usize,
    pub dynamic_handler: bool,
}

impl Runtime {
    /// It creates a new Lua runtime with default options.
    ///
    /// Such as some **built-in** libraries loaded and a default memory limit.
    pub async fn new() -> Result<Self> {
        // TODO: Make Lua stdlib configurable
        // Lua Std libs to load
        Runtime::new_with(RuntimeOpts {
            libs: StdLib::NONE
            // Default
            | StdLib::MATH
            | StdLib::TABLE
            | StdLib::STRING
            | StdLib::PACKAGE
            // Extra
            | StdLib::IO
            | StdLib::OS
            | StdLib::UTF8
            | StdLib::COROUTINE,
            memory_limit: MEMORY_LIMIT,
            dynamic_handler: true,
        })
        .await
    }

    /// It creates a new Lua runtime with specified options.
    ///
    /// For example, it allows for customizing the Lua standard libraries to load
    /// like `io`, `math`, `os`, etc as well as the memory limits.
    pub async fn new_with(opts: RuntimeOpts) -> Result<Self> {
        let lua = Lua::new_with(opts.libs, LuaOptions::default())
            .with_context(|| "Failed to configure the Lua runtime")?;

        lua.set_memory_limit(opts.memory_limit)
            .with_context(|| "Failed to set the memory limit for Lua runtime")?;

        Ok(Self {
            lua,
            cfg: None,
            http_fn: None,
            http_fn_key: None,
            http_fn_path: None,
            opts,
        })
    }

    /// It sets the Lua global functions for the specified **built-in** libraries
    /// like `Debug`, `Fetch`, `Template`, etc to be accessible in the Lua scripts.
    ///
    /// For setting custom libraries, use the singular [`set_global()`] method.
    pub async fn register_globals(&self, libs: UserData) -> Result {
        if libs.is_none() {
            return Ok(());
        }
        for lib in USERDATA_LIBS {
            if !libs.is_all() && !libs.contains(*lib) {
                continue;
            }
            match *lib {
                UserData::DEBUG => {
                    let value = utils::create_debug_fn(&self.lua)?;
                    self.lua.globals().set(*lib, value)?;
                }
                UserData::FETCH => {
                    let value = fetch::create_fetch_fn(&self.lua)?;
                    self.lua.globals().set(*lib, value)?;
                }
                UserData::TEMPLATE => {
                    let value = template::create_template_fn(&self.lua)?;
                    self.lua.globals().set(*lib, value)?;
                }
                UserData::JSON => {
                    let value = json::create_json_fn(&self.lua)?;
                    self.lua.globals().set(*lib, value)?;
                }
                UserData::DATABASE => {
                    // TODO: Make database path configurable
                    let value = db::create_database_fn(&self.lua, "./scripts/file.db")?;
                    self.lua.globals().set(*lib, value)?;
                }
                _ => continue,
            };
        }
        Ok(())
    }

    /// It sets a custom global Lua variable with the specified key and value.
    ///
    /// For setting **built-in** globals, use the plural [`register_globals()`] method.
    pub fn set_global<V: IntoLua>(&self, key: impl IntoLua, value: V) -> Result {
        self.lua
            .globals()
            .set(key, value)
            .with_context(|| "Failed to set custom global Lua variable")?;
        Ok(())
    }

    /// It sets the Lua configuration function that will be called at server startup.
    ///
    /// It loads the Lua script from the path and evaluates it to allocate the function,
    /// then it's immediately invoked with the provided arguments if any.
    /// The Lua table containing the configuration fields can be accessed later using the [`cfg()`] method.
    pub async fn register_cfg_fn(&mut self, cfg_src: &Path, args: impl IntoLuaMulti) -> Result {
        let data = std::fs::read(cfg_src)
            .with_context(|| "Failed to read the Lua configuration file content.")?;

        // Create config handler and call it
        let key = self
            .lua
            .load(data)
            .eval::<RegistryKey>()
            .with_context(|| "Failed to create Lua Config handler")?;

        let cfg_fn = self
            .lua
            .registry_value::<Function>(&key)
            .with_context(|| "Failed to get Lua Config handler from registry")?;

        let cfg = cfg_fn
            .call_async::<Table>(args)
            .await
            .with_context(|| "Failed to call Lua function with arguments")?;

        self.cfg = Some(cfg);
        Ok(())
    }

    /// It sets the Lua HTTP handler function that will be called on every HTTP request.
    ///
    /// It loads the Lua script from the path and evaluates it to allocate the function,
    /// but it's not invoked immediately. It will be called on every request.
    pub async fn register_http_fn(&mut self, http_src: &Path) -> Result {
        let meta = std::fs::metadata(http_src)
            .with_context(|| "Failed to read HTTP handler file metadata")?;

        if meta.is_file() {
            self.http_fn_path = Some(http_src.to_owned());
        } else {
            return Err(anyhow!(
                "HTTP handler path is not a regular file or does not exist"
            ));
        }

        let data = std::fs::read(http_src)
            .with_context(|| "Failed to read the Lua HTTP handler file content.")?;

        let key = self
            .lua
            .load(data)
            .eval::<RegistryKey>()
            .with_context(|| "Failed to create Lua HTTP handler")?;

        let http_fn = self.lua.registry_value::<Function>(&key)?;
        self.http_fn_key = Some(key);

        self.http_fn = Some(http_fn);
        Ok(())
    }

    /// Get a global Lua variable by key.
    ///
    /// Note that this function can also access a **built-in** global.
    pub fn get_global<V: FromLua>(&mut self, key: impl IntoLua) -> Result<V> {
        let value = self
            .lua
            .globals()
            .get::<V>(key)
            .with_context(|| "Failed to get global Lua variable".to_string())?;
        Ok(value)
    }

    /// The Lua configuration table that is returned after the script handler is invoked.
    pub fn cfg(&self) -> Option<&Table> {
        self.cfg.as_ref()
    }

    /// The Lua HTTP handler function that will be called for each HTTP request.
    pub fn http_fn(&self) -> Option<&Function> {
        self.http_fn.as_ref()
    }

    /// Reloads the Lua HTTP handler function from the file specified in `http_fn_path`.
    pub fn http_fn_reload(&mut self) -> Result<()> {
        // TODO: group all those fields in a struct
        if !self.opts.dynamic_handler
            || self.http_fn.is_none()
            || self.http_fn_key.is_none()
            || self.http_fn_path.is_none()
        {
            return Ok(());
        }

        // TODO: add logging
        println!(
            "Reloading http handler from {}",
            self.http_fn_path.as_ref().unwrap().display()
        );

        let data = std::fs::read(self.http_fn_path.as_ref().unwrap())
            .with_context(|| "Failed to read HTTP handler file content")?;

        let http_fn = self
            .lua
            .load(data)
            .eval::<Function>()
            .with_context(|| "Failed to create Lua HTTP handler")?;
        let mut existing_key = self.http_fn_key.take().unwrap();
        self.lua
            .replace_registry_value(&mut existing_key, http_fn)
            .with_context(|| "Failed to replace Lua HTTP handler in registry")?;
        let http_fn = self
            .lua
            .registry_value::<Function>(&existing_key)
            .with_context(|| "Failed to get Lua HTTP handler from registry")?;
        self.http_fn = Some(http_fn);
        self.http_fn_key = Some(existing_key);

        Ok(())
    }
}
