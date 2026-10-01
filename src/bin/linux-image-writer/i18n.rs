// Translations. The GUI's own words are written in English and looked up
// with gettext in the "linux-image-writer" domain (po/<language>.po,
// installed as share/locale/<language>/LC_MESSAGES/linux-image-writer.mo).
// Without a catalog for the user's language -- or with LANG=C -- the
// English text is shown as it is.
//
// Placeholders are named (`{size}`) and filled with `fill`, so a translation
// can put them in any order. `tr`, `ntr` and `n_` are the keywords the
// catalog is extracted with (see po/POTFILES.in).

/// The gettext domain: the program's name.
pub const DOMAIN: &str = "linux-image-writer";

// Binds the domain to the catalogs installed next to the program:
// `<prefix>/share/locale` for a program in `<prefix>/bin`
// (/app/share/locale in the Flatpak). The locale itself is set from the
// environment by GTK when the application starts (`gtk_init`), before any
// text is looked up. A failure leaves the text in English.
pub fn init() {
    use gettextrs::{bind_textdomain_codeset, bindtextdomain, textdomain};

    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|program| Some(program.parent()?.parent()?.join("share").join("locale")))
    {
        let _ = bindtextdomain(DOMAIN, dir);
    }
    let _ = bind_textdomain_codeset(DOMAIN, "UTF-8");
    let _ = textdomain(DOMAIN);
}

/// `msgid` in the user's language.
#[cfg(not(test))]
pub fn tr(msgid: &str) -> String {
    gettextrs::gettext(msgid)
}

/// `singular` or `plural`, as the user's language counts `n`.
#[cfg(not(test))]
pub fn ntr(singular: &str, plural: &str, n: u64) -> String {
    gettextrs::ngettext(singular, plural, u32::try_from(n).unwrap_or(u32::MAX))
}

/// Marks a constant for the catalog; it is translated where it is shown.
pub const fn n_(msgid: &'static str) -> &'static str {
    msgid
}

/// `text` with each `{name}` replaced by its value.
pub fn fill(text: String, values: &[(&str, &str)]) -> String {
    values.iter().fold(text, |text, (name, value)| {
        text.replace(&format!("{{{name}}}"), value)
    })
}

// ---- Tests ----
//
// Tests never touch the process's locale. Their text is English unless a
// test asks for Japanese (`ja`), which is then looked up in po/ja.po itself:
// so the Japanese the program shows is tested as it is, and a missing
// translation fails the test.

#[cfg(test)]
pub use test_catalog::{ja, ntr, tr};

#[cfg(test)]
mod test_catalog {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::OnceLock;

    thread_local! {
        static JAPANESE: Cell<bool> = const { Cell::new(false) };
    }

    /// Runs `f` with the Japanese catalog (this thread only).
    pub fn ja<T>(f: impl FnOnce() -> T) -> T {
        let before = JAPANESE.with(|japanese| japanese.replace(true));
        let value = f();
        JAPANESE.with(|japanese| japanese.set(before));
        value
    }

    pub fn tr(msgid: &str) -> String {
        lookup(msgid).unwrap_or_else(|| msgid.to_string())
    }

    pub fn ntr(singular: &str, plural: &str, n: u64) -> String {
        // Japanese has one form; English two.
        lookup(singular).unwrap_or_else(|| if n == 1 { singular } else { plural }.to_string())
    }

    fn lookup(msgid: &str) -> Option<String> {
        if !JAPANESE.with(Cell::get) {
            return None;
        }
        let translation = catalog().get(msgid);
        assert!(
            translation.is_some(),
            "no Japanese translation for {msgid:?}"
        );
        translation.cloned()
    }

    pub(super) fn catalog() -> &'static HashMap<String, String> {
        static CATALOG: OnceLock<HashMap<String, String>> = OnceLock::new();
        CATALOG.get_or_init(|| parse(include_str!("../../../po/ja.po")))
    }

    // The entries of a .po file: msgid -> msgstr (msgstr[0] for plurals).
    // Only what po/ja.po uses: no msgctxt, no obsolete entries, and a
    // msgid_plural on one line.
    fn parse(po: &str) -> HashMap<String, String> {
        let mut entries = HashMap::new();
        let mut msgid: Option<String> = None;
        let mut msgstr: Option<String> = None;
        for line in po.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("msgid ") {
                if let (Some(id), Some(string)) = (msgid.take(), msgstr.take())
                    && !id.is_empty()
                {
                    entries.insert(id, string);
                }
                msgid = Some(unquote(rest));
            } else if let Some(rest) = line
                .strip_prefix("msgstr ")
                .or_else(|| line.strip_prefix("msgstr[0] "))
            {
                msgstr = Some(unquote(rest));
            } else if line.starts_with('"') {
                // A continuation of the last field.
                match (&mut msgstr, &mut msgid) {
                    (Some(string), _) => string.push_str(&unquote(line)),
                    (None, Some(id)) => id.push_str(&unquote(line)),
                    _ => {}
                }
            }
        }
        if let (Some(id), Some(string)) = (msgid, msgstr)
            && !id.is_empty()
        {
            entries.insert(id, string);
        }
        entries
    }

    fn unquote(quoted: &str) -> String {
        let inner = quoted
            .trim()
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or("");
        let mut text = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                text.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => text.push('\n'),
                Some('t') => text.push('\t'),
                Some(other) => text.push(other),
                None => {}
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The string literals passed to `tr`, `ntr` (both forms) and `n_` in a
    // source file -- what the catalog is extracted from.
    fn msgids(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        for keyword in ["tr(", "n_(", "ntr("] {
            let mut rest = source;
            while let Some(at) = rest.find(keyword) {
                let before = rest[..at].chars().next_back();
                rest = &rest[at + keyword.len()..];
                // A whole word: not `str(`, `ntr(` inside `tr(` and so on.
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let mut args = rest;
                for _ in 0..if keyword == "ntr(" { 2 } else { 1 } {
                    let Some(open) = args.trim_start().strip_prefix('"') else {
                        break;
                    };
                    let mut text = String::new();
                    let mut chars = open.char_indices();
                    let mut end = open.len();
                    while let Some((i, c)) = chars.next() {
                        match c {
                            '"' => {
                                end = i + 1;
                                break;
                            }
                            '\\' => match chars.next().map(|(_, c)| c) {
                                Some('n') => text.push('\n'),
                                Some(other) => text.push(other),
                                None => {}
                            },
                            c => text.push(c),
                        }
                    }
                    found.push(text);
                    args = open[end..].trim_start().trim_start_matches(',');
                }
            }
        }
        found
    }

    #[test]
    fn every_text_has_a_japanese_translation() {
        let catalog = test_catalog::catalog();
        let mut count = 0;
        for source in [include_str!("text.rs"), include_str!("window.rs")] {
            let code = source.split("#[cfg(test)]").next().unwrap();
            for msgid in msgids(code) {
                count += 1;
                // The plural form of an `ntr` is the catalog's msgid_plural.
                if msgid.ends_with("bytes") && catalog.contains_key(msgid.trim_end_matches('s')) {
                    continue;
                }
                assert!(catalog.contains_key(&msgid), "not in po/ja.po: {msgid:?}");
            }
        }
        // Every text of both files, not a parsing accident.
        assert!(count > 200, "{count}");
    }

    #[test]
    fn placeholders_are_filled_by_name_in_any_order() {
        assert_eq!(
            fill(
                "{b} then {a}".to_string(),
                &[("a", "first"), ("b", "second")]
            ),
            "second then first"
        );
        // Japanese keeps the placeholders of the English text.
        for (msgid, msgstr) in test_catalog::catalog() {
            let names = |text: &str| {
                let mut names: Vec<String> = text
                    .split('{')
                    .skip(1)
                    .filter_map(|rest| rest.split_once('}').map(|(name, _)| name.to_string()))
                    .collect();
                names.sort();
                names
            };
            assert_eq!(names(msgid), names(msgstr), "{msgid:?}");
        }
    }
}
