mod test;

use std::ffi::{CStr, CString};

#[repr(C)]
struct CLibRangeFormatResult {
    pub start_line: i32,
    pub start_col: i32,
    pub end_line: i32,
    pub end_col: i32,
    pub text: *mut libc::c_char,
}

#[derive(Debug)]
pub struct RangeFormatResult {
    pub start_line: i32,
    pub start_col: i32,
    pub end_line: i32,
    pub end_col: i32,
    pub text: String,
}

#[derive(Debug)]
pub struct CodeStyleDiagnostic {
    pub start_line: i32,
    pub start_col: i32,
    pub end_line: i32,
    pub end_col: i32,
    pub message: String,
}

#[derive(Debug)]
#[repr(C)]
pub struct FormattingOptions {
    pub indent_size: u32,
    pub use_tabs: bool,
    pub insert_final_newline: bool,
    pub non_standard_symbol: bool,
}

impl Default for FormattingOptions {
    fn default() -> Self {
        FormattingOptions {
            indent_size: 4,
            use_tabs: false,
            insert_final_newline: true,
            non_standard_symbol: false,
        }
    }
}

unsafe extern "C" {
    fn ReformatLuaCode(
        code: *const libc::c_char,
        uri: *const libc::c_char,
        options: FormattingOptions,
    ) -> *mut libc::c_char;
    fn RangeFormatLuaCode(
        code: *const libc::c_char,
        uri: *const libc::c_char,
        startLine: i32,
        startCol: i32,
        endLine: i32,
        endCol: i32,
        options: FormattingOptions,
    ) -> CLibRangeFormatResult;
    fn FreeReformatResult(ptr: *mut libc::c_char);
    fn UpdateCodeStyle(workspaceUri: *const libc::c_char, configPath: *const libc::c_char);
    fn RemoveCodeStyle(workspaceUri: *const libc::c_char);
    fn CheckCodeStyle(code: *const libc::c_char, uri: *const libc::c_char) -> *mut libc::c_char;
}

pub fn reformat_code(code: &str, uri: &str, options: FormattingOptions) -> String {
    let c_code = CString::new(code).unwrap();
    let c_uri = CString::new(uri).unwrap();
    let c_result = unsafe { ReformatLuaCode(c_code.as_ptr(), c_uri.as_ptr(), options) };
    if c_result.is_null() {
        return code.to_string();
    }
    let result = unsafe { CStr::from_ptr(c_result).to_string_lossy().into_owned() };
    unsafe { FreeReformatResult(c_result) };
    result
}

pub fn range_format_code(
    code: &str,
    uri: &str,
    start_line: i32,
    start_col: i32,
    end_line: i32,
    end_col: i32,
    options: FormattingOptions,
) -> Option<RangeFormatResult> {
    let c_code = CString::new(code).unwrap();
    let c_uri = CString::new(uri).unwrap();
    let c_result = unsafe {
        RangeFormatLuaCode(
            c_code.as_ptr(),
            c_uri.as_ptr(),
            start_line,
            start_col,
            end_line,
            end_col,
            options,
        )
    };

    if c_result.text.is_null() {
        return None;
    }

    let result = RangeFormatResult {
        start_line: c_result.start_line,
        start_col: c_result.start_col,
        end_line: c_result.end_line,
        end_col: c_result.end_col,
        text: unsafe { CStr::from_ptr(c_result.text).to_string_lossy().into_owned() },
    };
    unsafe { FreeReformatResult(c_result.text) };

    Some(result)
}

pub fn update_code_style(workspace_uri: &str, config_path: &str) {
    let c_workspace_uri = CString::new(workspace_uri).unwrap();
    let c_config_path = CString::new(config_path).unwrap();
    unsafe { UpdateCodeStyle(c_workspace_uri.as_ptr(), c_config_path.as_ptr()) };
}

pub fn remove_code_style(workspace_uri: &str) {
    let c_workspace_uri = CString::new(workspace_uri).unwrap();
    unsafe { RemoveCodeStyle(c_workspace_uri.as_ptr()) };
}

pub fn check_code_style(uri: &str, code: &str) -> Vec<CodeStyleDiagnostic> {
    let c_uri = CString::new(uri).unwrap();
    let c_code = CString::new(code).unwrap();
    let c_result = unsafe { CheckCodeStyle(c_code.as_ptr(), c_uri.as_ptr()) };
    if c_result.is_null() {
        return vec![];
    }
    let result = unsafe { CStr::from_ptr(c_result).to_string_lossy().into_owned() };
    unsafe { FreeReformatResult(c_result) };
    // the format is start_line|start_col|end_line|end_col|message \n
    let result = result
        .split("\n")
        .map(|line| {
            let parts: Vec<&str> = line.split("|").collect();
            if parts.len() == 5 {
                Some(CodeStyleDiagnostic {
                    start_line: parts[0].parse().unwrap(),
                    start_col: parts[1].parse().unwrap(),
                    end_line: parts[2].parse().unwrap(),
                    end_col: parts[3].parse().unwrap(),
                    message: parts[4].to_string(),
                })
            } else {
                None
            }
        })
        .filter_map(|x| x)
        .collect::<Vec<CodeStyleDiagnostic>>();

    result
}
