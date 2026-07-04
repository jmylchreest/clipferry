//! MIME/target selection and the §7 translation table.

pub const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";
pub const URI_LIST: &str = "text/uri-list";
pub const QT_IMAGE: &str = "application/x-qt-image";
pub const KDE_PASSWORD_HINT: &str = "x-kde-passwordManagerHint";

/// Preferred order for reading text from a Wayland offer.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// MIME types we advertise on the Wayland side when proxying an X11 text
/// owner (X→W direction).
pub const X2W_TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "TEXT",
    "STRING",
];

/// X11 protocol machinery atoms — never forwarded as content types (§7).
pub const PROTOCOL_TARGETS: &[&str] = &[
    "TARGETS",
    "TIMESTAMP",
    "MULTIPLE",
    "SAVE_TARGETS",
    "DELETE",
    "INCR",
];

/// Plain-text targets/MIMEs that collapse into the standard text set when
/// translating. (`text/html` etc. are NOT plain text — they pass verbatim.)
pub fn is_plain_text(name: &str) -> bool {
    matches!(
        name,
        "UTF8_STRING"
            | "STRING"
            | "TEXT"
            | "text/plain"
            | "text/plain;charset=utf-8"
            | "text/plain;charset=UTF-8"
    )
}

pub fn pick_text(mime_types: &[String]) -> Option<&'static str> {
    TEXT_MIMES
        .iter()
        .find(|candidate| mime_types.iter().any(|m| m == *candidate))
        .copied()
}

/// The sensitive-content marker (§8).
///
/// Treat any offer carrying the KDE password-manager hint as sensitive.
/// Reading the hint's *value* would itself be a transfer; presence is the
/// practical signal — Klipper and friends only attach it to secrets.
pub fn is_sensitive(types: &[String]) -> bool {
    types.iter().any(|t| t == KDE_PASSWORD_HINT)
}

/// Streamable content transform for the §7 translation rows. Both variants
/// only touch the head of the stream, so they are chunk-safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transform {
    #[default]
    None,
    /// `x-special/gnome-copied-files` → `text/uri-list`: drop the leading
    /// `copy`/`cut` action line.
    StripCopyHeader,
    /// `text/uri-list` → `x-special/gnome-copied-files`: prepend `copy\n`.
    PrependCopyHeader,
}

/// W→X (§7): extra X11 targets to synthesize for a Wayland offer.
/// Returns (x11 target name, source MIME to read, transform).
pub fn synthesized_x11_targets(offered: &[String]) -> Vec<(String, String, Transform)> {
    let mut extra = Vec::new();
    if offered.iter().any(|m| m == URI_LIST) && !offered.iter().any(|m| m == GNOME_COPIED_FILES) {
        extra.push((
            GNOME_COPIED_FILES.to_owned(),
            URI_LIST.to_owned(),
            Transform::PrependCopyHeader,
        ));
    }
    extra
}

/// X→W (§7): translate an X11 owner's target names.
///
/// Returns (advertised Wayland MIMEs, plans) where each plan is
/// (advertised MIME, x11 target to read, transform).
pub fn x2w_translate(targets: &[String]) -> (Vec<String>, Vec<(String, String, Transform)>) {
    let has_uri_list = targets.iter().any(|t| t == URI_LIST);
    let mut advertised: Vec<String> = Vec::new();
    let mut plans: Vec<(String, String, Transform)> = Vec::new();
    let mut has_text = false;

    for t in targets {
        if PROTOCOL_TARGETS.contains(&t.as_str()) {
            continue;
        }
        if is_plain_text(t) {
            has_text = true;
            continue;
        }
        // WeChat/Wine quirk: prefer the uri-list over the Qt image blob.
        if t == QT_IMAGE && has_uri_list {
            continue;
        }
        if t == GNOME_COPIED_FILES && !has_uri_list {
            // Synthesize text/uri-list from the GNOME format; also pass the
            // GNOME type through verbatim for Wayland file managers.
            advertised.push(URI_LIST.to_owned());
            plans.push((
                URI_LIST.to_owned(),
                GNOME_COPIED_FILES.to_owned(),
                Transform::StripCopyHeader,
            ));
        }
        if !advertised.contains(t) {
            advertised.push(t.clone());
        }
    }

    let mut result = Vec::new();
    if has_text {
        result.extend(X2W_TEXT_MIMES.iter().map(|s| (*s).to_owned()));
    }
    for m in advertised {
        if !result.contains(&m) {
            result.push(m);
        }
    }
    (result, plans)
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

    #[test]
    fn uri_list_synthesizes_gnome_target() {
        let extra = synthesized_x11_targets(&owned(&["text/uri-list", "text/plain"]));
        assert_eq!(
            extra,
            vec![(
                GNOME_COPIED_FILES.to_owned(),
                URI_LIST.to_owned(),
                Transform::PrependCopyHeader
            )]
        );
        // Already offered → nothing to synthesize.
        assert!(synthesized_x11_targets(&owned(&["text/uri-list", GNOME_COPIED_FILES])).is_empty());
        assert!(synthesized_x11_targets(&owned(&["image/png"])).is_empty());
    }

    #[test]
    fn x2w_collapses_text_and_drops_protocol_atoms() {
        let (adv, plans) =
            x2w_translate(&owned(&["TARGETS", "TIMESTAMP", "UTF8_STRING", "STRING"]));
        assert_eq!(adv, owned(X2W_TEXT_MIMES));
        assert!(plans.is_empty());
    }

    #[test]
    fn x2w_gnome_synthesizes_uri_list() {
        let (adv, plans) = x2w_translate(&owned(&[GNOME_COPIED_FILES]));
        assert_eq!(adv, owned(&[URI_LIST, GNOME_COPIED_FILES]));
        assert_eq!(
            plans,
            vec![(
                URI_LIST.to_owned(),
                GNOME_COPIED_FILES.to_owned(),
                Transform::StripCopyHeader
            )]
        );
        // Owner already offers uri-list: no synthesis needed.
        let (adv, plans) = x2w_translate(&owned(&[GNOME_COPIED_FILES, URI_LIST]));
        assert!(plans.is_empty());
        assert!(adv.contains(&URI_LIST.to_owned()));
        assert!(adv.contains(&GNOME_COPIED_FILES.to_owned()));
    }

    #[test]
    fn x2w_qt_image_quirk_prefers_uri_list() {
        let (adv, _) = x2w_translate(&owned(&[QT_IMAGE, URI_LIST]));
        assert!(!adv.contains(&QT_IMAGE.to_owned()));
        assert!(adv.contains(&URI_LIST.to_owned()));
        // Without a uri-list the Qt image passes through verbatim.
        let (adv, _) = x2w_translate(&owned(&[QT_IMAGE]));
        assert!(adv.contains(&QT_IMAGE.to_owned()));
    }

    #[test]
    fn sensitive_detection() {
        assert!(is_sensitive(&owned(&["text/plain", KDE_PASSWORD_HINT])));
        assert!(!is_sensitive(&owned(&["text/plain"])));
    }
}
