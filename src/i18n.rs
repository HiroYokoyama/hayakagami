//! Interface language. English is the default.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Ja,
}

impl Lang {
    /// Reads a saved code. Anything unknown means English.
    pub fn from_code(code: &str) -> Self {
        if code.eq_ignore_ascii_case("ja") {
            Lang::Ja
        } else {
            Lang::En
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ja => "ja",
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set_lang(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == Lang::Ja as u8 {
        Lang::Ja
    } else {
        Lang::En
    }
}

/// Picks the text for the current language.
pub fn tr(en: &'static str, ja: &'static str) -> &'static str {
    match lang() {
        Lang::En => en,
        Lang::Ja => ja,
    }
}

/// `format!` with an English and a Japanese template.
#[macro_export]
macro_rules! trf {
    ($en:literal, $ja:literal $(, $($args:tt)*)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::En => format!($en $(, $($args)*)?),
            $crate::i18n::Lang::Ja => format!($ja $(, $($args)*)?),
        }
    };
}
