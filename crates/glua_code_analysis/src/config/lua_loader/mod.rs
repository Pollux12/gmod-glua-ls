use luars::{
    LuaResult, LuaVM, LuaValue,
    lua_vm::{LuaState, SafeOption},
};
use serde_json::Value;

pub fn load_lua_config(content: &str) -> Result<Value, String> {
    let mut safe_option = SafeOption::default();
    safe_option.max_call_depth = 64;
    safe_option.base_call_depth = 64;
    safe_option.max_stack_size = 256;
    safe_option.max_memory_limit = 100 * 1024 * 1024; // 100 MB
    let mut lua = LuaVM::new(safe_option);

    // SECURITY: never open `Stdlib::Os` for workspace-provided config files.
    // It exposes APIs such as `os.execute`/`os.remove`/`os.rename`, which would
    // allow arbitrary shell and filesystem operations when `.emmyrc.lua` loads.
    //
    // NOTE: `luars` currently does not provide a restricted/sandboxed `Basic`
    // subset. We keep `Basic` to preserve existing `.emmyrc.lua` compatibility.
    let _ = lua.open_stdlibs(&[
        luars::Stdlib::Package,
        luars::Stdlib::Basic,
        luars::Stdlib::Table,
        luars::Stdlib::String,
        luars::Stdlib::Math,
        luars::Stdlib::Utf8,
    ]);

    let _ = lua.set_global("print", LuaValue::cfunction(ls_println));

    // SECURITY: `.emmyrc.lua` is expected to return a config table, not load or
    // execute arbitrary external code. Remove dynamic code-loading and module-
    // loading globals exposed by Basic/Package to reduce sandbox escape surface.
    let _ = lua.set_global("load", LuaValue::nil());
    let _ = lua.set_global("loadfile", LuaValue::nil());
    let _ = lua.set_global("dofile", LuaValue::nil());
    let _ = lua.set_global("loadstring", LuaValue::nil());
    let _ = lua.set_global("require", LuaValue::nil());
    let _ = lua.set_global("package", LuaValue::nil());

    let values = match lua.execute(content) {
        Ok(v) => v,
        Err(e) => {
            let err_msg = lua.main_state().get_error_msg(e);
            return Err(format!("Failed to execute lua config: {:?}", err_msg));
        }
    };

    if values.is_empty() {
        return Err("Lua config did not return any value".to_string());
    }

    let value = values[0];
    lua.serialize_to_json(&value)
}

fn ls_println(l: &mut LuaState) -> LuaResult<usize> {
    let args = l.get_args();
    let mut output = String::new();
    for (index, arg) in args.iter().enumerate() {
        let s = l.to_string(arg)?;
        output.push_str(&s);
        if index < args.len() - 1 {
            output.push('\t');
        }
    }
    log::info!("{}", output);
    Ok(0)
}
