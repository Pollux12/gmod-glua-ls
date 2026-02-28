use glua_code_analysis::get_locale_code;
use log::info;
use lsp_types::InitializeParams;

pub fn set_ls_locale(params: &InitializeParams) -> Option<()> {
    let locale = params.locale.as_ref()?;
    let locale = get_locale_code(locale);
    info!("set locale: {}", locale);
    glua_parser::set_locale(&locale);
    glua_code_analysis::set_locale(&locale);
    rust_i18n::set_locale(&locale);
    Some(())
}
