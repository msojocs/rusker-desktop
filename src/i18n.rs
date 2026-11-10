//! UI language: resolves the saved preference to a locale and installs it
//! for `t!` (shared with gpui-component's built-in strings).

use crate::settings::LanguagePreference;

pub const ENGLISH: &str = "en";
pub const SIMPLIFIED_CHINESE: &str = "zh-CN";

/// The locale a preference stands for; "follow system" picks Simplified
/// Chinese for any `zh` system locale and English otherwise.
pub fn resolve(preference: LanguagePreference) -> &'static str {
    match preference {
        LanguagePreference::System => system_locale(),
        LanguagePreference::English => ENGLISH,
        LanguagePreference::SimplifiedChinese => SIMPLIFIED_CHINESE,
    }
}

/// Switches all translated text to `preference`. Already rendered text
/// changes on the next frame; callers refresh their windows.
pub fn apply(preference: LanguagePreference) {
    rust_i18n::set_locale(resolve(preference));
}

fn system_locale() -> &'static str {
    match sys_locale::get_locale() {
        Some(locale) if locale.to_ascii_lowercase().starts_with("zh") => SIMPLIFIED_CHINESE,
        _ => ENGLISH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_preferences_ignore_the_system_locale() {
        assert_eq!(resolve(LanguagePreference::English), ENGLISH);
        assert_eq!(
            resolve(LanguagePreference::SimplifiedChinese),
            SIMPLIFIED_CHINESE
        );
    }

    #[test]
    fn engine_errors_default_to_english() {
        let error = crate::engine::EngineError::Timeout {
            action: "Pull".into(),
            seconds: 5,
        };
        assert_eq!(error.to_string(), "Pull timed out (5s)");
    }
}

#[cfg(test)]
mod locale_tests {
    // Separate test binary: these tests mutate the process-global locale, so
    // they must not share a process with the string-assertion tests.
    use crate::settings::LanguagePreference;

    #[test]
    fn switching_locale_changes_translations_and_errors() {
        let error = crate::engine::EngineError::Timeout {
            action: "Pull".into(),
            seconds: 5,
        };
        super::apply(LanguagePreference::English);
        assert_eq!(&*rust_i18n::locale(), super::ENGLISH);
        assert_eq!(rust_i18n::t!("settings.theme.light"), "Light");
        assert_eq!(error.to_string(), "Pull timed out (5s)");

        super::apply(LanguagePreference::SimplifiedChinese);
        assert_eq!(&*rust_i18n::locale(), super::SIMPLIFIED_CHINESE);
        assert_eq!(rust_i18n::t!("settings.theme.light"), "浅色");
        assert_eq!(error.to_string(), "Pull超时（5 秒）");

        // Unknown keys fall back to English rather than an empty string.
        assert_eq!(rust_i18n::t!("app.settings.language"), "语言");

        super::apply(LanguagePreference::English);
        assert_eq!(rust_i18n::t!("app.settings.language"), "Language");
    }
}
