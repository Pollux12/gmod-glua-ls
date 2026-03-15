use glua_code_analysis::GmodRealm;

const SHARED_BADGE_URL: &str =
    "https://github.com/user-attachments/assets/a356f942-57d7-4915-a8cc-559870a980fc";
const SERVER_BADGE_URL: &str =
    "https://github.com/user-attachments/assets/d8fbe13a-6305-4e16-8698-5be874721ca1";
const CLIENT_BADGE_URL: &str =
    "https://github.com/user-attachments/assets/a5f6ba64-374d-42f0-b2f4-50e5c964e808";

const SHARED_BADGE_MARKDOWN: &str =
    "![(Shared)](https://github.com/user-attachments/assets/a356f942-57d7-4915-a8cc-559870a980fc)";
const SERVER_BADGE_MARKDOWN: &str =
    "![(Server)](https://github.com/user-attachments/assets/d8fbe13a-6305-4e16-8698-5be874721ca1)";
const CLIENT_BADGE_MARKDOWN: &str =
    "![(Client)](https://github.com/user-attachments/assets/a5f6ba64-374d-42f0-b2f4-50e5c964e808)";

pub(crate) fn badge_markdown(realm: GmodRealm) -> Option<&'static str> {
    match realm {
        GmodRealm::Shared => Some(SHARED_BADGE_MARKDOWN),
        GmodRealm::Server => Some(SERVER_BADGE_MARKDOWN),
        GmodRealm::Client => Some(CLIENT_BADGE_MARKDOWN),
        GmodRealm::Unknown => None,
    }
}

pub(crate) fn badge_label(realm: GmodRealm) -> Option<&'static str> {
    match realm {
        GmodRealm::Shared => Some("SHARED"),
        GmodRealm::Server => Some("SERVER"),
        GmodRealm::Client => Some("CLIENT"),
        GmodRealm::Unknown => None,
    }
}

pub(crate) fn badge_header_markdown(realm: GmodRealm) -> Option<String> {
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
        GmodRealm::Unknown => None,
    }
}
