//! Languages. English is the source language; translations live in
//! `translations/<language>/LC_MESSAGES/vibefuse.po`. Slint compiles them
//! into the window, and this module reads the same files for the texts the
//! Rust code makes, so every translation is in one place.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::LazyLock;
#[cfg(not(test))]
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// The language the user picked in the settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    System,
    English,
    Russian,
}

impl Language {
    /// In the order of the settings menu.
    pub const ALL: [Language; 3] = [Language::System, Language::English, Language::Russian];

    /// Language code of the translation to use.
    pub fn code(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::Russian => "ru",
            Language::System => {
                let russian = sys_locale::get_locale()
                    .is_some_and(|locale| locale.to_lowercase().starts_with("ru"));
                if russian { "ru" } else { "en" }
            }
        }
    }

    /// The name in the settings menu: each language in its own.
    pub fn name(self) -> String {
        match self {
            Language::System => tr("System"),
            Language::English => "English".to_owned(),
            Language::Russian => "Русский".to_owned(),
        }
    }
}

#[cfg(not(test))]
static RUSSIAN: AtomicBool = AtomicBool::new(false);

// Tests run in parallel threads: each keeps its own language.
#[cfg(test)]
thread_local! {
    static TEST_RUSSIAN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

static CATALOG: LazyLock<HashMap<String, String>> =
    LazyLock::new(|| parse_po(include_str!("../translations/ru/LC_MESSAGES/vibefuse.po")));

/// Switches the texts made in Rust; the window follows through Slint.
pub fn set(language: Language) {
    let code = language.code();
    #[cfg(test)]
    TEST_RUSSIAN.with(|russian| russian.set(code == "ru"));
    #[cfg(not(test))]
    RUSSIAN.store(code == "ru", Ordering::Relaxed);
    // Fails only before the first component exists, when Slint is about to
    // pick the system language by itself.
    let _ = slint::select_bundled_translation(code);
}

pub fn is_russian() -> bool {
    #[cfg(test)]
    return TEST_RUSSIAN.with(std::cell::Cell::get);
    #[cfg(not(test))]
    RUSSIAN.load(Ordering::Relaxed)
}

/// The translation of an English text.
pub fn tr(text: &str) -> String {
    if is_russian()
        && let Some(translated) = CATALOG.get(text).filter(|t| !t.is_empty())
    {
        return translated.clone();
    }
    text.to_owned()
}

/// Puts `args` into the `{}` of a translated text, in order.
pub fn fill(template: String, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut args = args.iter();
    let mut rest = template.as_str();
    while let Some(at) = rest.find("{}") {
        out.push_str(&rest[..at]);
        match args.next() {
            Some(arg) => out.push_str(&arg.to_string()),
            None => out.push_str("{}"),
        }
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    out
}

/// A decimal number in the current language: `44.1` or `44,1`.
pub fn decimal(text: String) -> String {
    if is_russian() {
        text.replace('.', ",")
    } else {
        text
    }
}

/// `tr!("text")`, or `tr!("text with {} and {}", a, b)`.
#[macro_export]
macro_rules! tr {
    ($text:literal) => {
        $crate::i18n::tr($text)
    };
    ($text:literal, $($arg:expr),+ $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::tr($text),
            &[$(&$arg as &dyn ::std::fmt::Display),+],
        )
    };
}

/// Reads the `msgid`/`msgstr` pairs of a gettext catalog.
fn parse_po(text: &str) -> HashMap<String, String> {
    fn unquote(line: &str) -> Option<String> {
        let inner = line.trim().strip_prefix('"')?.strip_suffix('"')?;
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        }
        Some(out)
    }

    let mut catalog = HashMap::new();
    let (mut id, mut value) = (None::<String>, None::<String>);
    let mut field = 0; // 1 msgid, 2 msgstr
    let mut finish = |id: &mut Option<String>, value: &mut Option<String>| {
        if let (Some(id), Some(value)) = (id.take(), value.take())
            && !id.is_empty()
        {
            catalog.insert(id, value);
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("msgid ") {
            finish(&mut id, &mut value);
            id = unquote(rest);
            field = 1;
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            value = unquote(rest);
            field = 2;
        } else if line.starts_with('"') {
            let target = if field == 1 { &mut id } else { &mut value };
            if let (Some(target), Some(more)) = (target.as_mut(), unquote(line)) {
                target.push_str(&more);
            }
        }
    }
    finish(&mut id, &mut value);
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_are_read_and_filled() {
        let catalog = parse_po(
            "msgid \"\"\nmsgstr \"Content-Type: text/plain\"\n\n\
             msgid \"Hello {}\"\nmsgstr \"Привет, \"\n\"{}\"\n\n\
             msgid \"Say \\\"hi\\\"\"\nmsgstr \"Скажи «привет»\"\n",
        );
        assert_eq!(catalog["Hello {}"], "Привет, {}");
        assert_eq!(catalog["Say \"hi\""], "Скажи «привет»");
        assert!(!catalog.contains_key(""));
        assert_eq!(fill("{} of {}".into(), &[&1, &"two"]), "1 of two");
        assert_eq!(fill("{} and {}".into(), &[&1]), "1 and {}");
    }

    #[test]
    fn every_translation_keeps_its_placeholders() {
        for (id, text) in CATALOG.iter() {
            assert!(!text.is_empty(), "untranslated: {id}");
            assert_eq!(id.matches("{}").count(), text.matches("{}").count(), "{id}");
        }
    }
}
