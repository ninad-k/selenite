//! Property-style tests for block insert / delete / move / toggle over a corpus of bodies.

use selenite_vault::{
    BlockRef, block_template, delete_block, insert_block, move_block, parse_blocks,
    toggle_task_item,
};

/// Bodies with one blank line between blocks, at most one bullet list (lists of the same kind
/// would merge when moved next to each other) and no indented code.
fn corpus() -> Vec<String> {
    let mut v: Vec<String> = vec![
        "# Title\n\nFirst paragraph.\n\nSecond paragraph.\n".into(),
        "# Title\r\n\r\nFirst paragraph.\r\n\r\nSecond paragraph.\r\n\r\nThird.\r\n".into(),
        "Intro\n\n- a\n- b\n  - nested\n\nOutro\n".into(),
        "Intro\n\n```rust\nfn a() {\n\n    let x = 1;\n\n}\n```\n\nOutro\n".into(),
        "Para\n\n> [!note] Title\n> body line\n>\n> second\n\nAfter callout\n".into(),
        "| a | b |\n| --- | --- |\n| 1 | 2 |\n\nText after table\n\n---\n\nEnd\n".into(),
        "- [ ] one\n- [x] two\n\n1. first\n2. second\n\nTail ^t1\n".into(),
        "One\n\nTwo\n\nThree\n\nFour\n\nFive".into(),
        "> quote one\n\n> quote two\n\n## Heading\n\nLast\n".into(),
        "Para with trailing spaces  \nline two\n\n<div>html</div>\n\nDone\n".into(),
        "A\r\n\r\n```\r\nx\r\n\r\ny\r\n```\r\n\r\n> [!warning]- Fold\r\n> hidden\r\n\r\nZ\r\n"
            .into(),
        "Solo\n".into(),
        "# Only heading\n\n- [ ] todo item ^task\n".into(),
    ];
    v.push("Café ünïcode\n\n日本語の段落\n\n> [!tip] Tip\n> emoji 🎉\n".into());
    v
}

fn texts(body: &str) -> Vec<String> {
    parse_blocks(body).into_iter().map(|b| b.text).collect()
}

#[test]
fn corpus_is_large_enough_and_parses() {
    let c = corpus();
    assert!(c.len() >= 10);
    for body in &c {
        assert!(!parse_blocks(body).is_empty(), "{body:?}");
    }
}

#[test]
fn insert_keeps_every_block_and_adds_one() {
    for body in corpus() {
        let before = texts(&body);
        let crlf = body.contains("\r\n");
        for pos in 0..=before.len() {
            let after = pos.checked_sub(1).map(BlockRef::Index);
            let out = insert_block(&body, after.as_ref(), "Inserted *para*\nline two").unwrap();
            let got = texts(&out);
            assert_eq!(got.len(), before.len() + 1, "{body:?} @ {pos}");
            let mut expect = before.clone();
            let inserted = if crlf {
                "Inserted *para*\r\nline two".to_string()
            } else {
                "Inserted *para*\nline two".to_string()
            };
            expect.insert(pos, inserted);
            assert_eq!(got, expect, "{body:?} @ {pos}");
            if crlf {
                assert!(
                    !out.replace("\r\n", "").contains('\n'),
                    "bare LF in {out:?}"
                );
            }
            // Deleting what was inserted restores the original bytes.
            let back = delete_block(&out, &BlockRef::Index(pos)).unwrap();
            assert_eq!(back, body, "{body:?} @ {pos}");
        }
    }
}

#[test]
fn insert_is_byte_stable_around_the_insertion_point() {
    for body in corpus() {
        let blocks = parse_blocks(&body);
        for (i, b) in blocks.iter().enumerate() {
            let out = insert_block(&body, Some(&BlockRef::Index(i)), "X").unwrap();
            assert!(out.starts_with(&body[..b.end]));
            assert!(out.ends_with(&body[b.end..]));
        }
        let out = insert_block(&body, None, "X").unwrap();
        assert!(out.ends_with(&body[blocks[0].start..]));
        assert!(out.starts_with(&body[..blocks[0].start]));
    }
}

#[test]
fn delete_keeps_every_other_block_byte_identical() {
    for body in corpus() {
        let before = texts(&body);
        for i in 0..before.len() {
            let out = delete_block(&body, &BlockRef::Index(i)).unwrap();
            let mut expect = before.clone();
            expect.remove(i);
            assert_eq!(texts(&out), expect, "{body:?} delete {i}");
            assert!(out.len() < body.len());
        }
    }
}

#[test]
fn delete_by_anchor_and_spacing() {
    let body = "A\n\nB ^b\n\nC\n";
    assert_eq!(
        delete_block(body, &BlockRef::Anchor("b".into())).unwrap(),
        "A\n\nC\n"
    );
    assert_eq!(
        delete_block(body, &BlockRef::Index(0)).unwrap(),
        "B ^b\n\nC\n"
    );
    assert_eq!(
        delete_block(body, &BlockRef::Index(2)).unwrap(),
        "A\n\nB ^b\n"
    );
    assert_eq!(delete_block("Only\n", &BlockRef::Index(0)).unwrap(), "");
    assert!(delete_block(body, &BlockRef::Index(3)).is_err());
    assert!(delete_block(body, &BlockRef::Anchor("zz".into())).is_err());
}

#[test]
fn move_permutes_and_move_back_restores_exactly() {
    for body in corpus() {
        let before = texts(&body);
        let n = before.len();
        for from in 0..n {
            for to in 0..n {
                let moved = move_block(&body, &BlockRef::Index(from), to).unwrap();
                let mut expect = before.clone();
                let item = expect.remove(from);
                expect.insert(to, item);
                assert_eq!(texts(&moved), expect, "{body:?} {from}->{to}");
                let back = move_block(&moved, &BlockRef::Index(to), from).unwrap();
                assert_eq!(back, body, "{body:?} {from}->{to}->{from}");
            }
        }
    }
}

#[test]
fn move_errors_and_noop() {
    let body = "A\n\nB\n\nC\n";
    assert!(move_block(body, &BlockRef::Index(0), 3).is_err());
    assert!(move_block(body, &BlockRef::Index(5), 0).is_err());
    assert_eq!(move_block(body, &BlockRef::Index(1), 1).unwrap(), body);
    assert_eq!(
        move_block(body, &BlockRef::Index(0), 2).unwrap(),
        "B\n\nC\n\nA\n"
    );
}

#[test]
fn toggle_flips_one_character_only() {
    let body = "Intro\n\n- [ ] one\n- [x] two\n  - [X] three\n\n> - [ ] quoted\n";
    let list = BlockRef::Index(1);
    let out = toggle_task_item(body, &list, 0).unwrap();
    assert_eq!(out, body.replacen("- [ ] one", "- [x] one", 1));
    let out2 = toggle_task_item(&out, &list, 0).unwrap();
    assert_eq!(out2, body);
    let out = toggle_task_item(body, &list, 1).unwrap();
    assert_eq!(out, body.replacen("- [x] two", "- [ ] two", 1));
    let out = toggle_task_item(body, &list, 2).unwrap();
    assert_eq!(out, body.replacen("[X] three", "[ ] three", 1));
    let out = toggle_task_item(body, &BlockRef::Index(2), 0).unwrap();
    assert_eq!(out, body.replacen("> - [ ] quoted", "> - [x] quoted", 1));
    // Errors: non-task line, out-of-range line, bad block.
    assert!(toggle_task_item(body, &BlockRef::Index(0), 0).is_err());
    assert!(toggle_task_item(body, &list, 9).is_err());
    assert!(toggle_task_item(body, &BlockRef::Index(9), 0).is_err());
    // Look-alikes are not tasks.
    assert!(toggle_task_item("- [y] no\n", &BlockRef::Index(0), 0).is_err());
    assert!(toggle_task_item("- [x]no\n", &BlockRef::Index(0), 0).is_err());
}

#[test]
fn toggle_crlf_and_untouched_blocks() {
    for body in corpus() {
        let blocks = parse_blocks(&body);
        for b in &blocks {
            for line in 0..b.text.lines().count() {
                let Ok(out) = toggle_task_item(&body, &BlockRef::Index(b.index), line) else {
                    continue;
                };
                assert_eq!(out.len(), body.len());
                assert_eq!(texts(&out).len(), blocks.len());
                for other in blocks.iter().filter(|o| o.index != b.index) {
                    assert_eq!(texts(&out)[other.index], other.text);
                }
                let diff = out
                    .bytes()
                    .zip(body.bytes())
                    .filter(|(a, b)| a != b)
                    .count();
                assert_eq!(diff, 1);
            }
        }
    }
    let crlf = "- [ ] a\r\n- [ ] b\r\n";
    assert_eq!(
        toggle_task_item(crlf, &BlockRef::Index(0), 1).unwrap(),
        "- [ ] a\r\n- [x] b\r\n"
    );
}

#[test]
fn insert_errors_and_crlf() {
    let body = "A\r\n\r\nB\r\n";
    assert!(insert_block(body, None, "  \n").is_err());
    assert!(insert_block(body, Some(&BlockRef::Index(9)), "x").is_err());
    assert_eq!(
        insert_block(body, Some(&BlockRef::Index(0)), "l1\nl2").unwrap(),
        "A\r\n\r\nl1\r\nl2\r\n\r\nB\r\n"
    );
    assert_eq!(insert_block("", None, "x").unwrap(), "x\n");
    assert_eq!(insert_block("\n\n", None, "x").unwrap(), "\n\nx\n");
}

#[test]
fn templates() {
    for (k, v) in [
        ("text", ""),
        ("h1", "# "),
        ("h2", "## "),
        ("h3", "### "),
        ("bullet", "- "),
        ("numbered", "1. "),
        ("todo", "- [ ] "),
        ("toggle", "> [!note]- Toggle\n> "),
        ("quote", "> "),
        ("callout", "> [!note] Note\n> "),
        ("code", "```\n\n```"),
        ("divider", "---"),
        ("image", "![]()"),
        ("file", "[file]()"),
        ("embed", "![[]]"),
        ("table", "| Column | Column |\n| --- | --- |\n|  |  |"),
    ] {
        assert_eq!(block_template(k), Some(v), "{k}");
    }
    assert_eq!(block_template("nope"), None);
}

#[test]
fn templates_insert_as_one_block_each() {
    for k in [
        "h1", "h2", "h3", "bullet", "numbered", "todo", "toggle", "quote", "callout", "code",
        "divider", "image", "file", "embed", "table",
    ] {
        let tpl = block_template(k).unwrap();
        let out = insert_block("Para\n", Some(&BlockRef::Index(0)), tpl).unwrap();
        assert_eq!(parse_blocks(&out).len(), 2, "{k}: {out:?}");
        assert!(out.contains(tpl), "{k}: trailing spaces kept: {out:?}");
    }
}
