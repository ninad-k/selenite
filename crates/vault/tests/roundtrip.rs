//! Round-trip byte stability for `Document`.

use selenite_vault::Document;

const CASES: &[&str] = &[
    "",
    "no front matter\n",
    "no trailing newline",
    "---\ntitle: A\n---\n",
    "---\ntitle: A\n---",
    "---\n---\n",
    "---\n---",
    "---\r\ntitle: A\r\n---\r\nbody\r\nmore\r\n",
    "\u{feff}---\ntitle: A\n---\nbody\n",
    "\u{feff}plain with bom\n",
    "---\na: 1\n...\nafter dots\n",
    "---   \na: 1\n---  \nbody\n",
    "---\na: [unclosed\n---\nbody\n",
    "---\n# a comment\nkey:   'odd'   # trailing\nother: \"q\\\"uote\"\nmulti: |\n  line1\n  line2\n---\nbody\n",
    "---\ntitle: Ünïcödé 日本語 🙂\n---\nüñí body 🙂\n",
    "intro\n\n---\n\nhorizontal rule is not front matter\n---\n",
    "---\nunterminated front matter\nbody\n",
    "---",
    "---\n",
    "--- not a fence\nx\n---\n",
    "body with trailing spaces   \n\n\n",
    "---\n- a\n- b\n---\nlist front matter\n",
    "\n---\nleading blank line means no front matter\n---\n",
    "---\r\na: 1\n---\nmixed newlines\r\n",
    "\r\n\r\n",
];

#[test]
fn untouched_documents_round_trip_byte_for_byte() {
    for case in CASES {
        let d = Document::parse(case);
        assert_eq!(&d.to_text(), case, "round trip failed for {case:?}");
        assert_eq!(
            &case[d.body_offset()..],
            d.body(),
            "body offset for {case:?}"
        );
    }
}

#[test]
fn reparse_of_output_is_stable() {
    for case in CASES {
        let once = Document::parse(case).to_text();
        assert_eq!(Document::parse(&once).to_text(), once);
    }
}

#[test]
fn set_property_leaves_body_bytes_identical() {
    for case in CASES {
        let mut d = Document::parse(case);
        let body = d.body().to_string();
        d.set_property("added", selenite_vault::PropertyValue::Bool(true));
        assert_eq!(d.body(), body, "body changed for {case:?}");
        assert!(d.to_text().ends_with(&body));
    }
}
