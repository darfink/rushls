use isolang::Language;

/// Canonicalizes the ISO 639 primary subtag while retaining its RFC 5646
/// qualifiers.
///
/// The source already allocated this string while copying container metadata,
/// so rewriting it in place keeps discovery from allocating a second string
/// for the ordinary case.
pub(super) fn canonical_language_tag(mut tag: String) -> Option<String> {
    let primary_end = tag.find('-').unwrap_or(tag.len());

    // This deliberately checks only the lexical shape of suffix subtags. Full
    // RFC 5646 parsing, including grandfathered and private-use-only tags, is
    // outside the metadata contract; a recognized ISO 639 primary is required.
    if tag.split('-').skip(1).any(|subtag| {
        subtag.is_empty()
            || subtag.len() > 8
            || !subtag.bytes().all(|byte| byte.is_ascii_alphanumeric())
    }) {
        return None;
    }

    tag[..primary_end].make_ascii_lowercase();
    let language = iso_language(&tag[..primary_end])?;
    let canonical = language.to_639_1().unwrap_or_else(|| language.to_639_3());
    tag.replace_range(..primary_end, canonical);
    Some(tag)
}

fn iso_language(primary: &str) -> Option<Language> {
    // ISO 639-3 uses the terminology code for the twenty languages whose
    // legacy ISO 639-2 bibliographic code differs.
    let primary = match primary {
        "alb" => "sqi",
        "arm" => "hye",
        "baq" => "eus",
        "bur" => "mya",
        "chi" => "zho",
        "cze" => "ces",
        "dut" => "nld",
        "fre" => "fra",
        "geo" => "kat",
        "ger" => "deu",
        "gre" => "ell",
        "ice" => "isl",
        "mac" => "mkd",
        "mao" => "mri",
        "may" => "msa",
        "per" => "fas",
        "rum" => "ron",
        "slo" => "slk",
        "tib" => "bod",
        "wel" => "cym",
        primary => primary,
    };

    Language::from_639_1(primary).or_else(|| Language::from_639_3(primary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_iso_639_primaries_and_preserves_qualifiers() {
        for (input, expected) in [
            ("en", Some("en")),
            ("EN", Some("en")),
            ("eng", Some("en")),
            ("HAW", Some("haw")),
            ("und", Some("und")),
            ("fra-CA", Some("fr-CA")),
            ("ZHO-Hant-TW", Some("zh-Hant-TW")),
            ("", None),
            ("zzz", None),
            ("i-klingon", None),
            ("x-private", None),
            ("en-", None),
            ("en--US", None),
            ("en-US!", None),
            ("en-123456789", None),
            ("en_US", None),
        ] {
            assert_eq!(
                canonical_language_tag(input.into()).as_deref(),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn canonicalizes_every_iso_639_2_bibliographic_alias() {
        for (input, expected) in [
            ("alb", "sq"),
            ("arm", "hy"),
            ("baq", "eu"),
            ("bur", "my"),
            ("chi", "zh"),
            ("cze", "cs"),
            ("dut", "nl"),
            ("fre", "fr"),
            ("geo", "ka"),
            ("ger", "de"),
            ("gre", "el"),
            ("ice", "is"),
            ("mac", "mk"),
            ("mao", "mi"),
            ("may", "ms"),
            ("per", "fa"),
            ("rum", "ro"),
            ("slo", "sk"),
            ("tib", "bo"),
            ("wel", "cy"),
        ] {
            assert_eq!(
                canonical_language_tag(input.into()).as_deref(),
                Some(expected),
                "{input}"
            );
        }
    }
}
