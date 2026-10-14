//! End-to-end tests of the `pour` binary against a small local registry.
//! No network: the index is built from JSON in the Homebrew API shape.

use std::path::PathBuf;
use std::process::Command;

fn pour(prefix: &PathBuf, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pour"))
        .args(args)
        .env("POUR_PREFIX", prefix)
        .env("NO_COLOR", "1")
        .env("POUR_INDEX_URL", "http://127.0.0.1:9/never")
        .output()
        .expect("run pour");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn registry_json(dir: &PathBuf) -> (PathBuf, PathBuf) {
    let formulae = serde_json::json!([
        {"name": "jq", "full_name": "jq", "tap": "homebrew/core", "desc": "Lightweight and flexible command-line JSON processor", "homepage": "https://jqlang.github.io/jq/", "license": "MIT",
         "versions": {"stable": "1.8.2"}, "revision": 0, "bottle": {"stable": {"rebuild": 1, "root_url": "https://ghcr.io/v2/homebrew/core", "files": {
            "arm64_tahoe": {"cellar": ":any", "sha256": "ca67c64d0aaf1e5472790ec2cc081ff7972316f27095d8a8aab81b3321247036"},
            "x86_64_linux": {"cellar": ":any", "sha256": "631787d9b19df5e5557cc83f972080e541f543f8df2d1f212ca4290ff23e9764"}}}},
         "dependencies": ["oniguruma"], "build_dependencies": [], "uses_from_macos": [], "aliases": [], "oldnames": []},
        {"name": "oniguruma", "full_name": "oniguruma", "tap": "homebrew/core", "desc": "Regular expressions library", "homepage": "https://github.com/kkos/oniguruma", "license": "BSD-2-Clause",
         "versions": {"stable": "6.9.10"}, "revision": 0, "bottle": {"stable": {"rebuild": 0, "root_url": "https://ghcr.io/v2/homebrew/core", "files": {"all": {"cellar": ":any_skip_relocation", "sha256": "49eb885ab9a80ce30edc0aaa5f531fc0079a17e5cf41c2b6c34be0e53e763993"}}}},
         "dependencies": [], "build_dependencies": [], "uses_from_macos": [], "aliases": ["onig"], "oldnames": []},
        {"name": "python@3.14", "full_name": "python@3.14", "tap": "homebrew/core", "desc": "Interpreted, interactive, object-oriented programming language", "homepage": "https://www.python.org/",
         "versions": {"stable": "3.14.0"}, "revision": 1, "bottle": {"stable": {"rebuild": 0, "root_url": "https://ghcr.io/v2/homebrew/core", "files": {}}},
         "dependencies": ["sqlite"], "build_dependencies": [], "uses_from_macos": [], "aliases": ["python3", "python"], "oldnames": []}
    ]);
    let casks = serde_json::json!([
        {"token": "firefox", "full_token": "firefox", "tap": "homebrew/cask", "name": ["Mozilla Firefox"], "desc": "Web browser", "homepage": "https://www.mozilla.org/firefox/", "version": "157.0.1",
         "sha256": "40a0a649120635460256dae9ad63e40377f77326cbc9c449a5d0a1d7f5dd7982", "url": "https://example.com/Firefox.dmg", "artifacts": [{"app": ["Firefox.app"]}], "auto_updates": true, "depends_on": {"macos": {">=": ["12"]}}}
    ]);
    let f = dir.join("formula.json");
    let c = dir.join("cask.json");
    std::fs::write(&f, serde_json::to_vec(&formulae).unwrap()).unwrap();
    std::fs::write(&c, serde_json::to_vec(&casks).unwrap()).unwrap();
    (f, c)
}

fn setup() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("pour");
    std::fs::create_dir_all(&prefix).unwrap();
    let (f, c) = registry_json(&dir.path().to_path_buf());
    let out = dir.path().join("index.pour.gz");
    let (ok, stdout, stderr) = pour(
        &prefix,
        &[
            "registry",
            "build",
            "--formulae",
            f.to_str().unwrap(),
            "--casks",
            c.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--meta",
            "source=test",
        ],
    );
    assert!(ok, "build failed: {stdout} {stderr}");
    assert!(stdout.contains("3 formulae, 1 casks"), "{stdout}");
    let (ok, stdout, stderr) = pour(&prefix, &["registry", "import", out.to_str().unwrap()]);
    assert!(ok, "import failed: {stdout} {stderr}");
    (dir, prefix)
}

#[test]
fn search_info_and_json() {
    let (_d, prefix) = setup();
    let (ok, out, _) = pour(&prefix, &["search", "json"]);
    assert!(ok);
    assert!(out.contains("jq"), "{out}");
    assert!(
        out.contains("Lightweight and flexible"),
        "search keeps the real description: {out}"
    );
    let (ok, out, _) = pour(&prefix, &["search", "--cask", "browser"]);
    assert!(ok && out.contains("firefox"), "{out}");
    let (ok, out, _) = pour(&prefix, &["info", "onig"]);
    assert!(
        ok && out.contains("oniguruma") && out.contains("6.9.10"),
        "alias lookup: {out}"
    );
    let (ok, out, _) = pour(&prefix, &["info", "--json", "python3"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["name"], "python@3.14");
    assert_eq!(v[0]["pkg_version"], "3.14.0_1");
    let (ok, _, err) = pour(&prefix, &["info", "nothing-like-this"]);
    assert!(!ok && err.contains("No formula or cask"), "{err}");
    let (ok, _, err) = pour(&prefix, &["install", "--dry-run", "jqq"]);
    assert!(!ok && err.contains("Did you mean"), "{err}");
}

#[test]
fn list_env_stats_and_dry_run() {
    let (_d, prefix) = setup();
    let (ok, out, _) = pour(&prefix, &["env", "--json"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["formulae"], 3);
    assert_eq!(v["installed_formulae"], 0);
    let (ok, out, _) = pour(&prefix, &["list", "--json"]);
    assert!(ok && out.contains("\"formulae\": []"), "{out}");
    let (ok, out, _) = pour(&prefix, &["registry", "stats"]);
    assert!(ok && out.contains("\"casks\": 1"), "{out}");
    let (ok, out, _) = pour(&prefix, &["deps", "jq"]);
    assert!(ok && out.trim() == "oniguruma", "{out}");
    let (ok, out, _) = pour(&prefix, &["--prefix"]);
    assert!(ok && out.trim() == prefix.to_string_lossy(), "{out}");
    let (ok, out, _) = pour(&prefix, &["shellenv", "zsh"]);
    assert!(
        ok && out.contains("POUR_PREFIX") && out.contains("/bin:"),
        "{out}"
    );
    let (ok, out, _) = pour(&prefix, &["completions", "bash"]);
    assert!(ok && out.contains("_pour"), "{out}");
    let (ok, out, _) = pour(&prefix, &["init", "--dry-run"]);
    assert!(ok && out.contains("pour init"), "{out}");
    let (ok, out, _) = pour(&prefix, &["doctor"]);
    assert!(ok, "{out}");
    // dry-run install plans but pours nothing; on systems without bottles it says so
    let (_ok, out, err) = pour(&prefix, &["install", "--dry-run", "jq"]);
    assert!(
        out.contains("oniguruma") || err.contains("cannot run on") || err.contains("no bottle"),
        "{out} {err}"
    );
    assert!(!prefix.join("Cellar/jq").exists());
}

#[test]
fn brewfile_parsing() {
    let text = "# comment\ntap \"jks-sys/pour-apps\", \"https://x/index.json\"\nbrew \"jq\"\nbrew 'wget', args: ['x']\ncask \"firefox\"\nmas \"Xcode\", id: 497799835\n";
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("pour");
    std::fs::create_dir_all(&prefix).unwrap();
    let bf = dir.path().join("Brewfile");
    std::fs::write(&bf, text).unwrap();
    let (f, c) = registry_json(&dir.path().to_path_buf());
    let out = dir.path().join("i.pour");
    pour(
        &prefix,
        &[
            "registry",
            "build",
            "--formulae",
            f.to_str().unwrap(),
            "--casks",
            c.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    );
    pour(&prefix, &["registry", "import", out.to_str().unwrap()]);
    let (_ok, stdout, stderr) = pour(
        &prefix,
        &[
            "bundle",
            "install",
            "--file",
            bf.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(stdout.contains("2 formulae, 1 casks"), "{stdout} {stderr}");
}
