//! Human-facing messages only; protocol and machine-readable fields stay stable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Language {
    English,
    Spanish,
}

pub fn choose<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Language {
    if let Some(value) = values.into_iter().flatten().find(|v| !v.is_empty()) {
        let language = value.split(['_', '.', '@', '-']).next().unwrap_or("");
        return if language.eq_ignore_ascii_case("es") {
            Language::Spanish
        } else {
            Language::English
        };
    }
    Language::English
}

pub fn language() -> Language {
    let all = std::env::var("LC_ALL").ok();
    let messages = std::env::var("LC_MESSAGES").ok();
    let lang = std::env::var("LANG").ok();
    choose([all.as_deref(), messages.as_deref(), lang.as_deref()])
}

pub fn text(english: &'static str, spanish: &'static str) -> &'static str {
    match language() {
        Language::Spanish => spanish,
        Language::English => english,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locale_priority_and_fallback() {
        assert_eq!(
            choose([Some(""), Some("es_ES.UTF-8"), Some("en_US")]),
            Language::Spanish
        );
        assert_eq!(choose([Some("C"), Some("es_ES")]), Language::English);
        assert_eq!(choose([Some("fr_FR"), Some("es_ES")]), Language::English);
        assert_eq!(choose([None, None, None]), Language::English);
        assert_eq!(choose([Some("es-MX")]), Language::Spanish);
    }
}
