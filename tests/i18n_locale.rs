//! These tests mutate the process-global locale. Keep them in a separate
//! integration-test binary so they cannot race with string assertions in the
//! library unit tests.

#[macro_use]
extern crate rust_i18n;

use rusker_desktop::{
    engine::EngineError,
    i18n::{self, ENGLISH, SIMPLIFIED_CHINESE},
    settings::LanguagePreference,
};

i18n!("locales", fallback = "en");

#[test]
fn switching_locale_changes_translations_and_errors() {
    let error = EngineError::Timeout {
        action: "Pull".into(),
        seconds: 5,
    };
    i18n::apply(LanguagePreference::English);
    assert_eq!(&*rust_i18n::locale(), ENGLISH);
    assert_eq!(rust_i18n::t!("settings.theme.light"), "Light");
    assert_eq!(error.to_string(), "Pull timed out (5s)");

    i18n::apply(LanguagePreference::SimplifiedChinese);
    assert_eq!(&*rust_i18n::locale(), SIMPLIFIED_CHINESE);
    assert_eq!(rust_i18n::t!("settings.theme.light"), "浅色");
    assert_eq!(error.to_string(), "Pull超时（5 秒）");

    // Unknown keys fall back to English rather than an empty string.
    assert_eq!(rust_i18n::t!("app.settings.language"), "语言");

    i18n::apply(LanguagePreference::English);
    assert_eq!(rust_i18n::t!("app.settings.language"), "Language");
}
