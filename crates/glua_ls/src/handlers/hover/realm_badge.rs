use glua_code_analysis::GmodRealm;

const SHARED_BADGE_URL: &str = "https://raw.githubusercontent.com/Pollux12/gmod-glua-ls/main/docs/mintlify/images/realms/shared.png";
const SERVER_BADGE_URL: &str = "https://raw.githubusercontent.com/Pollux12/gmod-glua-ls/main/docs/mintlify/images/realms/server.png";
const CLIENT_BADGE_URL: &str = "https://raw.githubusercontent.com/Pollux12/gmod-glua-ls/main/docs/mintlify/images/realms/client.png";

const SHARED_BADGE_MARKDOWN: &str = "![(Shared)](https://raw.githubusercontent.com/Pollux12/gmod-glua-ls/main/docs/mintlify/images/realms/shared.png)";
const SERVER_BADGE_MARKDOWN: &str = "![(Server)](https://raw.githubusercontent.com/Pollux12/gmod-glua-ls/main/docs/mintlify/images/realms/server.png)";
const CLIENT_BADGE_MARKDOWN: &str = "![(Client)](https://raw.githubusercontent.com/Pollux12/gmod-glua-ls/main/docs/mintlify/images/realms/client.png)";
const MENU_BADGE_MARKDOWN: &str = "`MENU`";

pub(crate) fn badge_markdown(realm: GmodRealm) -> Option<&'static str> {
    match realm {
        GmodRealm::Shared => Some(SHARED_BADGE_MARKDOWN),
        GmodRealm::Server => Some(SERVER_BADGE_MARKDOWN),
        GmodRealm::Client => Some(CLIENT_BADGE_MARKDOWN),
        GmodRealm::Menu => Some(MENU_BADGE_MARKDOWN),
        GmodRealm::Unknown => None,
    }
}

pub(crate) fn badge_label(realm: GmodRealm) -> Option<&'static str> {
    match realm {
        GmodRealm::Shared => Some("SHARED"),
        GmodRealm::Server => Some("SERVER"),
        GmodRealm::Client => Some("CLIENT"),
        GmodRealm::Menu => Some("MENU"),
        GmodRealm::Unknown => None,
    }
}

pub(crate) fn badge_header_markdown(realm: GmodRealm) -> Option<String> {
    if realm == GmodRealm::Menu {
        return Some(MENU_BADGE_MARKDOWN.to_string());
    }

    Some(format!(
        "{} **{}**",
        badge_markdown(realm)?,
        badge_label(realm)?
    ))
}

#[allow(dead_code)]
pub(crate) fn badge_url(realm: GmodRealm) -> Option<&'static str> {
    match realm {
        GmodRealm::Shared => Some(SHARED_BADGE_URL),
        GmodRealm::Server => Some(SERVER_BADGE_URL),
        GmodRealm::Client => Some(CLIENT_BADGE_URL),
        GmodRealm::Menu | GmodRealm::Unknown => None,
    }
}
