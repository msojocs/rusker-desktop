#[macro_use]
extern crate rust_i18n;

// Translations for the whole app (`locales/*.yml`); the `ui` crate's
// gpui-component strings share the same global locale.
i18n!("locales", fallback = "en");

pub mod container;
pub mod diagnostics;
pub mod docker_context;
pub mod engine;
pub mod format;
pub mod i18n;
pub mod image;
pub mod model;
pub mod runtime_host;
pub mod settings;
pub mod tray;
pub mod update;
pub mod volume;
