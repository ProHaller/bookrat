//! Book language handling. MP4 stores ISO 639-2/T codes (`eng`, `deu`), while
//! tags and locales come as 639-1 (`de`), 639-2/B (`ger`) or plain names.

/// (639-1, 639-2/T, 639-2/B if different, English name)
const LANGUAGES: &[(&str, &str, &str, &str)] = &[
    ("ar", "ara", "", "arabic"),
    ("cs", "ces", "cze", "czech"),
    ("da", "dan", "", "danish"),
    ("de", "deu", "ger", "german"),
    ("el", "ell", "gre", "greek"),
    ("en", "eng", "", "english"),
    ("es", "spa", "", "spanish"),
    ("fa", "fas", "per", "persian"),
    ("fi", "fin", "", "finnish"),
    ("fr", "fra", "fre", "french"),
    ("he", "heb", "", "hebrew"),
    ("hi", "hin", "", "hindi"),
    ("hu", "hun", "", "hungarian"),
    ("is", "isl", "ice", "icelandic"),
    ("it", "ita", "", "italian"),
    ("ja", "jpn", "", "japanese"),
    ("ko", "kor", "", "korean"),
    ("nl", "nld", "dut", "dutch"),
    ("no", "nor", "", "norwegian"),
    ("pl", "pol", "", "polish"),
    ("pt", "por", "", "portuguese"),
    ("ro", "ron", "rum", "romanian"),
    ("ru", "rus", "", "russian"),
    ("sk", "slk", "slo", "slovak"),
    ("sv", "swe", "", "swedish"),
    ("tr", "tur", "", "turkish"),
    ("uk", "ukr", "", "ukrainian"),
    ("zh", "zho", "chi", "chinese"),
];

pub const DEFAULT: &str = "eng";

/// Normalizes a language tag, locale or name to ISO 639-2/T. Unknown but
/// well-formed three-letter codes are passed through.
pub fn normalize(input: &str) -> Option<String> {
    // Locales look like `de_DE.UTF-8` or `en-US`; keep only the language.
    let s = input
        .trim()
        .split(['_', '-', '.', '@'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if let Some(&(_, t, _, _)) = LANGUAGES
        .iter()
        .find(|&&(one, t, b, name)| s == one || s == t || (!b.is_empty() && s == b) || s == name)
    {
        return Some(t.to_string());
    }
    let valid = s.len() == 3 && s.bytes().all(|c| c.is_ascii_lowercase()) && s != "und";
    valid.then_some(s)
}

/// The user's language from the POSIX locale environment, if set.
pub fn from_environment() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .and_then(|v| normalize(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_locales() {
        assert_eq!(normalize("de_DE.UTF-8").as_deref(), Some("deu"));
    }

    #[test]
    fn normalize_maps_bibliographic_codes_to_terminology() {
        assert_eq!(normalize("ger").as_deref(), Some("deu"));
    }

    #[test]
    fn normalize_accepts_english_names() {
        assert_eq!(normalize("English").as_deref(), Some("eng"));
    }

    #[test]
    fn normalize_rejects_undetermined() {
        assert_eq!(normalize("und"), None);
    }
}
