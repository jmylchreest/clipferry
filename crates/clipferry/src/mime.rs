//! MIME/target selection. M4 brings the full bidirectional translation table
//! (§7); M1 only needs to pick the best text representation from an offer.

/// Preferred order for reading text from a Wayland offer.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

pub fn pick_text(mime_types: &[String]) -> Option<&'static str> {
    TEXT_MIMES
        .iter()
        .find(|candidate| mime_types.iter().any(|m| m == *candidate))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn prefers_utf8_mime() {
        let mimes = owned(&["image/png", "text/plain", "text/plain;charset=utf-8"]);
        assert_eq!(pick_text(&mimes), Some("text/plain;charset=utf-8"));
    }

    #[test]
    fn falls_back_to_legacy_names() {
        assert_eq!(
            pick_text(&owned(&["UTF8_STRING", "image/png"])),
            Some("UTF8_STRING")
        );
        assert_eq!(pick_text(&owned(&["STRING"])), Some("STRING"));
    }

    #[test]
    fn none_for_non_text_offers() {
        assert_eq!(pick_text(&owned(&["image/png", "text/html"])), None);
        assert_eq!(pick_text(&[]), None);
    }
}
