fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    build_emmyluacodestyle();
}

fn build_emmyluacodestyle() {
    let mut builder = cc::Build::new();
    builder.cpp(true);
    builder
        .include("3rd/EmmyLuaCodeStyle/Util/include")
        .include("3rd/EmmyLuaCodeStyle/CodeFormatCLib/include")
        .include("3rd/EmmyLuaCodeStyle/CodeFormatCore/include")
        .include("3rd/EmmyLuaCodeStyle/LuaParser/include")
        .include("3rd/EmmyLuaCodeStyle/3rd/wildcards/include")
        .include("3rd/lua");

    let source_patterns = vec![
        "3rd/EmmyLuaCodeStyle/CodeFormatCLib/src/*.cpp",
        "3rd/EmmyLuaCodeStyle/LuaParser/src/**/*.cpp",
        "3rd/EmmyLuaCodeStyle/Util/src/StringUtil.cpp",
        "3rd/EmmyLuaCodeStyle/Util/src/Utf8.cpp",
        "3rd/EmmyLuaCodeStyle/Util/src/SymSpell/*.cpp",
        "3rd/EmmyLuaCodeStyle/Util/src/InfoTree/*.cpp",
        "3rd/EmmyLuaCodeStyle/CodeFormatCore/src/**/*.cpp",
    ];

    let watch_patterns = vec![
        "3rd/EmmyLuaCodeStyle/CodeFormatCLib/include/**/*.h",
        "3rd/EmmyLuaCodeStyle/LuaParser/include/**/*.h",
        "3rd/EmmyLuaCodeStyle/Util/include/**/*.h",
        "3rd/EmmyLuaCodeStyle/CodeFormatCore/include/**/*.h",
        "3rd/EmmyLuaCodeStyle/3rd/wildcards/include/**/*.h",
        "3rd/lua/**/*.h",
    ];

    for pattern in source_patterns {
        if pattern.contains("*") {
            let files = glob::glob(pattern)
                .unwrap()
                .filter_map(|path| path.ok())
                .collect::<Vec<_>>();
            for path in &files {
                println!("cargo:rerun-if-changed={}", path.display());
            }
            builder.files(files);
        } else {
            println!("cargo:rerun-if-changed={pattern}");
            builder.file(pattern);
        }
    }

    for pattern in watch_patterns {
        for path in glob::glob(pattern).unwrap().filter_map(|path| path.ok()) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    if cfg!(windows) {
        let compiler = builder.get_compiler();
        if compiler.is_like_msvc() {
            builder.flag("/utf-8");
            builder.flag("/std:c++17");
        } else {
            // Assuming mingw on Windows
            builder.flag("-std=c++17");
        }
    } else {
        builder.flag("-std=c++17");
    }

    builder.compile("EmmyLuaCodeStyle");
}
