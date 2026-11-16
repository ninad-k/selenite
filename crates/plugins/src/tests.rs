//! Host tests against a real temporary vault.

use std::time::Duration;

use selenite_core::VaultPath;
use selenite_index::Engine;
use selenite_index::audit::AuditLog;
use selenite_storage::WriteMode;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::{
    ENABLED_FILE, Limits, METHODS, Permission, PluginError, PluginHost, discover, load_enabled,
    set_enabled,
};

fn vault() -> (TempDir, Engine) {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::init_local(dir.path()).expect("init vault");
    (dir, engine)
}

fn put(engine: &Engine, path: &str, text: &str) {
    engine
        .storage()
        .write(
            &VaultPath::new(path).expect("path"),
            text.as_bytes(),
            WriteMode::Overwrite,
        )
        .expect("write");
}

fn install(engine: &Engine, id: &str, permissions: &[&str], js: &str) {
    let perms = permissions
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(",");
    put(
        engine,
        &format!(".selenite/plugins/{id}/manifest.json"),
        &format!(
            r#"{{"id":"{id}","name":"Test {id}","version":"1.0.0","description":"d","permissions":[{perms}]}}"#
        ),
    );
    put(engine, &format!(".selenite/plugins/{id}/main.js"), js);
}

fn call(
    host: &mut PluginHost,
    engine: &mut Engine,
    id: &str,
    method: &str,
    args: Value,
) -> Result<Value, PluginError> {
    host.call(engine, id, method, &args)
}

fn audit(engine: &Engine) -> Vec<selenite_index::audit::AuditEntry> {
    AuditLog::new().entries(engine, 100).expect("audit entries")
}

#[test]
fn plugins_are_disabled_by_default_and_enabling_is_explicit() {
    let (_d, mut e) = vault();
    install(&e, "demo", &["read"], "// js");
    let found = discover(&e).expect("discover");
    assert_eq!(found.len(), 1);
    assert!(!found[0].enabled && !found[0].changed && found[0].error.is_none());
    assert_eq!(found[0].permissions, vec![Permission::Read]);

    let mut host = PluginHost::new();
    let err = call(&mut host, &mut e, "demo", "vault.search", json!({"q": "x"}))
        .expect_err("disabled plugin ran");
    assert!(matches!(err, PluginError::Disabled(_)));
    assert!(matches!(
        load_enabled(&e, "demo"),
        Err(PluginError::Disabled(_))
    ));

    set_enabled(&e, "demo", true).expect("enable");
    assert!(discover(&e).expect("discover")[0].enabled);
    call(
        &mut host,
        &mut e,
        "demo",
        "vault.search",
        json!({"q": "welcome"}),
    )
    .expect("search");

    set_enabled(&e, "demo", false).expect("disable");
    assert!(call(&mut host, &mut e, "demo", "vault.search", json!({"q": "x"})).is_err());
}

#[test]
fn changed_files_stop_a_plugin_until_it_is_enabled_again() {
    let (_d, mut e) = vault();
    install(&e, "demo", &["read"], "// v1");
    set_enabled(&e, "demo", true).expect("enable");
    let mut host = PluginHost::new();
    call(
        &mut host,
        &mut e,
        "demo",
        "vault.search",
        json!({"q": "welcome"}),
    )
    .expect("ok");

    put(&e, ".selenite/plugins/demo/main.js", "// v2 (replaced)");
    let err = call(&mut host, &mut e, "demo", "vault.search", json!({"q": "x"}))
        .expect_err("changed plugin ran");
    assert!(matches!(err, PluginError::Changed(_)));
    let info = &discover(&e).expect("discover")[0];
    assert!(!info.enabled && info.changed);

    set_enabled(&e, "demo", true).expect("re-enable");
    call(
        &mut host,
        &mut e,
        "demo",
        "vault.search",
        json!({"q": "welcome"}),
    )
    .expect("ok again");
}

#[test]
fn damaged_enabled_file_enables_nothing() {
    let (_d, e) = vault();
    install(&e, "demo", &["read"], "//");
    set_enabled(&e, "demo", true).expect("enable");
    put(&e, ENABLED_FILE, "{ not json");
    assert!(!discover(&e).expect("discover")[0].enabled);
    assert!(load_enabled(&e, "demo").is_err());
}

#[test]
fn write_without_permission_is_refused_and_audited() {
    let (_d, mut e) = vault();
    install(&e, "reader", &["read"], "//");
    set_enabled(&e, "reader", true).expect("enable");
    let mut host = PluginHost::new();

    let err = call(
        &mut host,
        &mut e,
        "reader",
        "vault.createPage",
        json!({"title": "Sneaky", "markdown": "x"}),
    )
    .expect_err("write without permission");
    match err {
        PluginError::PermissionDenied { needed, .. } => assert_eq!(needed, "write"),
        other => panic!("wrong error: {other}"),
    }
    assert!(
        e.resolve("Sneaky").expect("resolve").is_none(),
        "page was created"
    );
    let err = call(
        &mut host,
        &mut e,
        "reader",
        "vault.appendBlocks",
        json!({"path": "Welcome.md", "markdown": "more"}),
    )
    .expect_err("append without permission");
    assert!(matches!(err, PluginError::PermissionDenied { .. }));
    let (text, _) = e
        .read_page(&VaultPath::new("Welcome.md").expect("path"))
        .expect("read");
    assert!(!text.contains("more"));

    let log = audit(&e);
    assert_eq!(log.len(), 2);
    assert!(
        log.iter()
            .all(|a| !a.ok && a.error.as_deref() == Some("permission_denied"))
    );
    assert!(
        log.iter()
            .all(|a| a.transport.as_deref() == Some("plugin:reader"))
    );
}

#[test]
fn read_permission_does_not_grant_commands_and_vice_versa() {
    let (_d, mut e) = vault();
    install(&e, "cmd", &["commands"], "//");
    set_enabled(&e, "cmd", true).expect("enable");
    let mut host = PluginHost::new();
    let err = call(
        &mut host,
        &mut e,
        "cmd",
        "vault.read",
        json!({"path": "Welcome.md"}),
    )
    .expect_err("read without permission");
    assert!(matches!(
        err,
        PluginError::PermissionDenied { needed: "read", .. }
    ));
    let ok = call(
        &mut host,
        &mut e,
        "cmd",
        "ui.registerCommand",
        json!({"id": "count", "title": " Count words "}),
    )
    .expect("register");
    assert_eq!(ok, json!({"id": "count", "title": "Count words"}));
    // notify needs no permission.
    call(&mut host, &mut e, "cmd", "ui.notify", json!({"text": "hi"})).expect("notify");

    install(&e, "plain", &[], "//");
    set_enabled(&e, "plain", true).expect("enable");
    let err = call(
        &mut host,
        &mut e,
        "plain",
        "ui.registerCommand",
        json!({"id": "x", "title": "X"}),
    )
    .expect_err("commands without permission");
    assert!(matches!(
        err,
        PluginError::PermissionDenied {
            needed: "commands",
            ..
        }
    ));
}

#[test]
fn granted_writes_work_and_are_audited_without_content() {
    let (_d, mut e) = vault();
    install(&e, "writer", &["read", "write"], "//");
    set_enabled(&e, "writer", true).expect("enable");
    let mut host = PluginHost::new();
    let secret = "plutonium-secret-text";

    let created = call(
        &mut host,
        &mut e,
        "writer",
        "vault.createPage",
        json!({"title": "From Plugin", "markdown": format!("# From Plugin\n\n{secret}\n")}),
    )
    .expect("create");
    assert_eq!(created["path"], "From Plugin.md");

    let appended = call(
        &mut host,
        &mut e,
        "writer",
        "vault.appendBlocks",
        json!({"path": "From Plugin", "markdown": "second block"}),
    )
    .expect("append");
    assert_eq!(appended["path"], "From Plugin.md");
    let read = call(
        &mut host,
        &mut e,
        "writer",
        "vault.read",
        json!({"path": "From Plugin.md"}),
    )
    .expect("read");
    assert!(
        read["text"]
            .as_str()
            .expect("text")
            .ends_with("second block\n")
            || read["text"]
                .as_str()
                .expect("text")
                .contains("second block")
    );

    let hits = call(
        &mut host,
        &mut e,
        "writer",
        "vault.search",
        json!({"q": "plutonium"}),
    )
    .expect("search");
    assert_eq!(hits["results"][0]["path"], "From Plugin.md");
    let back = call(
        &mut host,
        &mut e,
        "writer",
        "vault.backlinks",
        json!({"path": "From Plugin.md"}),
    )
    .expect("backlinks");
    assert_eq!(back["backlinks"].as_array().expect("array").len(), 0);

    let log = audit(&e);
    assert_eq!(log.len(), 2, "only writes are audited: {log:?}");
    assert_eq!(log[0].tool, "vault.createPage");
    assert_eq!(log[1].tool, "vault.appendBlocks");
    assert!(log.iter().all(|a| a.ok && a.error.is_none()));
    assert!(
        log.iter()
            .all(|a| a.transport.as_deref() == Some("plugin:writer"))
    );
    assert_eq!(log[0].targets, vec!["From Plugin.md".to_string()]);
    assert!(log[1].revision_before.is_some() && log[1].revision_after.is_some());
    assert_eq!(log[0].args_digest.len(), 64);
    let raw = std::fs::read_to_string(
        std::fs::read_dir(_d.path().join(".selenite/audit"))
            .expect("audit dir")
            .next()
            .expect("file")
            .expect("entry")
            .path(),
    )
    .expect("read log");
    assert!(!raw.contains(secret) && !raw.contains("second block"));
}

#[test]
fn failed_writes_are_audited_too() {
    let (_d, mut e) = vault();
    install(&e, "writer", &["write"], "//");
    set_enabled(&e, "writer", true).expect("enable");
    let mut host = PluginHost::new();
    let err = call(
        &mut host,
        &mut e,
        "writer",
        "vault.appendBlocks",
        json!({"path": "Missing.md", "markdown": "x"}),
    )
    .expect_err("missing page");
    assert!(
        matches!(
            err,
            PluginError::Engine {
                category: "not_found",
                ..
            }
        ),
        "{err}"
    );
    let log = audit(&e);
    assert_eq!(log.len(), 1);
    assert!(!log[0].ok);
    assert_eq!(log[0].error.as_deref(), Some("not_found"));
}

#[test]
fn there_is_no_method_for_delete_rename_settings_or_network() {
    for m in METHODS {
        let n = m.name.to_lowercase();
        for banned in [
            "delete", "remove", "trash", "rename", "move", "setting", "config", "fetch", "http",
            "net", "enable", "archive", "exec", "shell", "fs.",
        ] {
            assert!(!n.contains(banned), "method {} looks dangerous", m.name);
        }
    }
    let names: Vec<&str> = METHODS.iter().map(|m| m.name).collect();
    assert_eq!(
        names,
        vec![
            "vault.search",
            "vault.read",
            "vault.backlinks",
            "vault.createPage",
            "vault.appendBlocks",
            "ui.registerCommand",
            "ui.notify"
        ]
    );

    let (_d, mut e) = vault();
    install(&e, "all", &["read", "write", "commands"], "//");
    set_enabled(&e, "all", true).expect("enable");
    let mut host = PluginHost::new();
    for m in [
        "vault.delete",
        "vault.trash",
        "vault.rename",
        "vault.move",
        "settings.set",
        "plugin.enable",
        "net.fetch",
        "fs.read",
        "vault.search ",
        "VAULT.SEARCH",
        "",
    ] {
        let err = call(
            &mut host,
            &mut e,
            "all",
            m,
            json!({"path": "Welcome.md", "q": "x"}),
        )
        .expect_err("unknown method accepted");
        assert!(matches!(err, PluginError::UnknownMethod(_)), "{m}: {err}");
    }
    // Welcome.md is still there and untouched.
    assert!(e.resolve("Welcome").expect("resolve").is_some());
    assert!(audit(&e).is_empty());
}

#[test]
fn paths_and_arguments_are_validated() {
    let (_d, mut e) = vault();
    install(&e, "all", &["read", "write"], "//");
    set_enabled(&e, "all", true).expect("enable");
    let mut host = PluginHost::new();
    for path in [
        "../outside.md",
        "/etc/passwd",
        ".selenite/config.yaml",
        ".selenite/plugins/all/main.js",
        ".obsidian/app.json",
    ] {
        assert!(
            call(
                &mut host,
                &mut e,
                "all",
                "vault.read",
                json!({"path": path})
            )
            .is_err(),
            "read {path}"
        );
        assert!(
            call(
                &mut host,
                &mut e,
                "all",
                "vault.appendBlocks",
                json!({"path": path, "markdown": "x"})
            )
            .is_err(),
            "append {path}"
        );
    }
    // Unknown fields, wrong types, oversized payloads.
    assert!(matches!(
        call(
            &mut host,
            &mut e,
            "all",
            "vault.read",
            json!({"path": "a.md", "extra": 1})
        ),
        Err(PluginError::InvalidArgs(_))
    ));
    assert!(matches!(
        call(&mut host, &mut e, "all", "vault.search", json!({"q": 5})),
        Err(PluginError::InvalidArgs(_))
    ));
    let big = "x".repeat(300 * 1024);
    assert!(matches!(
        call(
            &mut host,
            &mut e,
            "all",
            "vault.createPage",
            json!({"title": "Big", "markdown": big})
        ),
        Err(PluginError::InvalidArgs(_))
    ));
    // A traversal-looking title is either refused or sanitised into a file inside the vault.
    if let Ok(v) = call(
        &mut host,
        &mut e,
        "all",
        "vault.createPage",
        json!({"title": "../../escape", "markdown": "x"}),
    ) {
        let path = v["path"].as_str().expect("path");
        assert!(!path.contains('/'), "{path}");
    }
    assert!(
        !_d.path()
            .parent()
            .expect("parent")
            .join("escape.md")
            .exists()
    );
    // A plugin id from the caller is validated too.
    assert!(matches!(
        call(
            &mut host,
            &mut e,
            "../all",
            "vault.search",
            json!({"q": "x"})
        ),
        Err(PluginError::NotFound(_))
    ));
}

#[test]
fn calls_are_rate_limited_per_plugin() {
    let (_d, mut e) = vault();
    install(&e, "busy", &["read", "write"], "//");
    install(&e, "calm", &["read"], "//");
    set_enabled(&e, "busy", true).expect("enable");
    set_enabled(&e, "calm", true).expect("enable");
    let mut host = PluginHost::with_limits(Limits {
        calls: 5,
        writes: 2,
        window: Duration::from_secs(3600),
    });
    for i in 0..2 {
        call(
            &mut host,
            &mut e,
            "busy",
            "vault.createPage",
            json!({"title": format!("P{i}"), "markdown": "x"}),
        )
        .expect("write within limit");
    }
    let err = call(
        &mut host,
        &mut e,
        "busy",
        "vault.createPage",
        json!({"title": "P9", "markdown": "x"}),
    )
    .expect_err("third write");
    assert!(matches!(err, PluginError::RateLimited(_)));
    assert!(e.resolve("P9").expect("resolve").is_none());
    // Reads still allowed until the total is reached.
    for _ in 0..3 {
        call(&mut host, &mut e, "busy", "vault.search", json!({"q": "x"})).expect("read");
    }
    assert!(matches!(
        call(&mut host, &mut e, "busy", "vault.search", json!({"q": "x"})),
        Err(PluginError::RateLimited(_))
    ));
    // Another plugin has its own budget.
    call(&mut host, &mut e, "calm", "vault.search", json!({"q": "x"})).expect("separate budget");
    // The rate-limited write was audited as such.
    assert!(
        audit(&e)
            .iter()
            .any(|a| a.error.as_deref() == Some("rate_limited"))
    );
}

#[test]
fn broken_plugins_are_listed_with_an_error_and_cannot_be_enabled() {
    let (_d, e) = vault();
    put(
        &e,
        ".selenite/plugins/bad/manifest.json",
        r#"{"id":"bad","name":"x","version":"1","permissions":["network"]}"#,
    );
    put(&e, ".selenite/plugins/bad/main.js", "//");
    put(
        &e,
        ".selenite/plugins/mismatch/manifest.json",
        r#"{"id":"other","name":"x","version":"1","permissions":[]}"#,
    );
    put(&e, ".selenite/plugins/mismatch/main.js", "//");
    put(
        &e,
        ".selenite/plugins/nomain/manifest.json",
        r#"{"id":"nomain","name":"x","version":"1","permissions":[]}"#,
    );
    install(&e, "good", &[], "//");
    let found = discover(&e).expect("discover");
    let by = |id: &str| found.iter().find(|p| p.id == id).expect("listed");
    assert!(
        by("bad")
            .error
            .as_deref()
            .expect("error")
            .contains("network")
    );
    assert!(by("mismatch").error.is_some());
    assert!(by("nomain").error.is_some());
    assert!(by("good").error.is_none());
    for id in ["bad", "mismatch", "nomain", "missing", "../x"] {
        assert!(set_enabled(&e, id, true).is_err(), "enabled {id}");
    }
}

#[test]
fn the_word_count_example_is_a_valid_plugin_and_works_end_to_end() {
    let manifest = include_str!("../../../examples/plugins/word-count/manifest.json");
    let main_js = include_str!("../../../examples/plugins/word-count/main.js");
    let parsed = crate::Manifest::parse(manifest).expect("example manifest");
    assert_eq!(parsed.id, "word-count");
    assert_eq!(
        parsed.permissions,
        vec![Permission::Read, Permission::Commands]
    );
    assert!(main_js.contains("selenite.vault.read"));

    let (_d, mut e) = vault();
    put(&e, ".selenite/plugins/word-count/manifest.json", manifest);
    put(&e, ".selenite/plugins/word-count/main.js", main_js);
    assert!(!discover(&e).expect("discover")[0].enabled);
    set_enabled(&e, "word-count", true).expect("enable");
    let mut host = PluginHost::new();
    let cmd = call(
        &mut host,
        &mut e,
        "word-count",
        "ui.registerCommand",
        json!({"id": "count", "title": "Count words in this page"}),
    )
    .expect("register");
    assert_eq!(cmd["id"], "count");
    let page = call(
        &mut host,
        &mut e,
        "word-count",
        "vault.read",
        json!({"path": "Welcome.md"}),
    )
    .expect("read");
    assert!(page["text"].as_str().expect("text").contains("Welcome"));
    // It asked for read and commands only: writing is refused.
    assert!(
        call(
            &mut host,
            &mut e,
            "word-count",
            "vault.createPage",
            json!({"title": "X", "markdown": "x"})
        )
        .is_err()
    );
}
