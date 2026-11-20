//! The hidden-oracle command for a CVE outside Go: the test files the
//! upstream fix added or changed, run with the project's own test runner.
//!
//! Go keeps its per-test oracle (`go test -run`, see [`crate::grader`]).
//! Every other language gets one shell command that must fail on the
//! vulnerable code and pass with the fix.

use std::path::Path;

use crate::profiles::Profile;
use crate::sandbox::shell_quote;

fn ext(path: &str) -> &str {
    path.rsplit_once('.').map_or("", |(_, e)| e)
}

fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.split_once('.').map_or(name, |(s, _)| s)
}

fn quoted(files: &[&String]) -> String {
    files.iter().map(|f| shell_quote(f)).collect::<Vec<_>>().join(" ")
}

/// The JS test runner a package uses, from `package.json`.
pub fn node_runner(root: &Path) -> &'static str {
    let pkg: serde_json::Value = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let has = |name: &str| {
        ["dependencies", "devDependencies"].iter().any(|k| pkg[*k].get(name).is_some())
            || pkg["scripts"]["test"].as_str().is_some_and(|t| t.split(|c: char| !c.is_alphanumeric()).any(|w| w == name))
    };
    for runner in ["jest", "vitest", "mocha", "ava", "tap"] {
        if has(runner) {
            return runner;
        }
    }
    "node"
}

/// `[[test]]` targets in `Cargo.toml`: `(name, path)`.
fn cargo_test_targets(root: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default();
    let field = |block: &str, key: &str| {
        block.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"').to_owned())
        })
    };
    text.split("[[test]]")
        .skip(1)
        .filter_map(|block| {
            let block = block.split("\n[").next().unwrap_or(block);
            let name = field(block, "name")?;
            let path = field(block, "path").unwrap_or_else(|| format!("tests/{name}.rs"));
            Some((name, path))
        })
        .collect()
}

/// The command that runs only `tests` (repository-relative paths, as they
/// are after the fix) under `profile`, or why there is none.
pub fn command(profile: &Profile, root: &Path, tests: &[String]) -> Result<String, String> {
    let pick = |pred: &dyn Fn(&str) -> bool| tests.iter().filter(|t| pred(t)).collect::<Vec<_>>();
    let none = |what: &str| Err(format!("the fix's test files contain no runnable {what} test"));
    match profile.lang.as_str() {
        "python" => {
            let files = pick(&|t| ext(t) == "py" && !t.ends_with("conftest.py") && !t.ends_with("__init__.py"));
            if files.is_empty() {
                return none("Python");
            }
            Ok(format!("python -m pytest -q -p no:cacheprovider {}", quoted(&files)))
        }
        "node" => {
            let files = pick(&|t| matches!(ext(t), "js" | "mjs" | "cjs" | "ts" | "mts" | "cts" | "jsx" | "tsx")
                && !t.contains("/fixtures/") && !t.contains("/__fixtures__/"));
            if files.is_empty() {
                return none("JavaScript/TypeScript");
            }
            let files = quoted(&files);
            Ok(match node_runner(root) {
                "jest" => format!("npx --no-install jest --ci {files}"),
                "vitest" => format!("npx --no-install vitest run {files}"),
                "mocha" => format!("npx --no-install mocha {files}"),
                "ava" => format!("npx --no-install ava {files}"),
                "tap" => format!("npx --no-install tap {files}"),
                _ => format!("node --test {files}"),
            })
        }
        "rust" => {
            let targets = cargo_test_targets(root);
            let names: Vec<String> = tests
                .iter()
                .filter_map(|t| {
                    targets.iter().find(|(_, path)| path == t).map(|(name, _)| name.clone()).or_else(|| {
                        (t.starts_with("tests/") && ext(t) == "rs" && t.matches('/').count() == 1).then(|| stem(t).to_owned())
                    })
                })
                .collect();
            if names.is_empty() {
                return Err("the fix's tests are not integration tests under tests/ (inline #[cfg(test)] modules are part of the source)".into());
            }
            Ok(names.iter().map(|n| format!("cargo test --offline --test {}", shell_quote(n))).collect::<Vec<_>>().join(" && "))
        }
        "maven" | "gradle" => {
            let classes: Vec<&str> = tests.iter().filter(|t| matches!(ext(t), "java" | "kt" | "scala" | "groovy")).map(|t| stem(t)).collect();
            if classes.is_empty() {
                return none("JVM");
            }
            if profile.lang == "maven" {
                let mvn = if root.join("mvnw").is_file() { "./mvnw" } else { "mvn" };
                Ok(format!(
                    "{mvn} -o -B -q test -Dtest={} -DfailIfNoTests=false -Dsurefire.failIfNoSpecifiedTests=false",
                    shell_quote(&classes.join(","))
                ))
            } else {
                let gr = if root.join("gradlew").is_file() { "./gradlew" } else { "gradle" };
                let selectors: String = classes.iter().map(|c| format!(" --tests {}", shell_quote(c))).collect();
                Ok(format!("{gr} --no-daemon --offline -q test{selectors}"))
            }
        }
        "ruby" => {
            let bundled = profile.deps.contains("bundle install");
            let exec = if bundled { "bundle exec " } else { "" };
            let specs = pick(&|t| t.ends_with("_spec.rb"));
            if !specs.is_empty() {
                return Ok(format!("{exec}rspec {}", quoted(&specs)));
            }
            let files = pick(&|t| {
                let n = stem(t);
                ext(t) == "rb" && (["test_", "tc_", "spec_"].iter().any(|p| n.starts_with(p)) || n.ends_with("_test"))
            });
            if files.is_empty() {
                return none("Ruby");
            }
            Ok(format!(
                "{exec}ruby -Itest -Ilib -e 'ARGV.each {{ |f| require File.expand_path(f) }}' {}",
                quoted(&files)
            ))
        }
        "php" => {
            let files = pick(&|t| ext(t) == "php" && stem(t).ends_with("Test"));
            if files.is_empty() {
                return none("PHP");
            }
            Ok(format!("for f in {}; do vendor/bin/phpunit \"$f\" || exit 1; done", quoted(&files)))
        }
        other => Err(format!("no test runner for {other} projects, so the fix's tests cannot be the oracle")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (n, b) in files {
            let p = dir.path().join(n);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b).unwrap();
        }
        dir
    }

    fn cmd(files: &[(&str, &str)], tests: &[&str]) -> Result<String, String> {
        let d = tree(files);
        let tests: Vec<String> = tests.iter().map(|s| (*s).to_owned()).collect();
        command(&profiles::detect(d.path()), d.path(), &tests)
    }

    #[test]
    fn per_language_commands() {
        assert_eq!(
            cmd(&[("setup.py", "")], &["tests/test_x.py", "tests/conftest.py"]).unwrap(),
            "python -m pytest -q -p no:cacheprovider 'tests/test_x.py'"
        );
        assert_eq!(
            cmd(&[("package.json", r#"{"devDependencies":{"mocha":"1"}}"#)], &["test/a.js", "test/fixtures/b.js"]).unwrap(),
            "npx --no-install mocha 'test/a.js'"
        );
        assert_eq!(cmd(&[("package.json", r#"{"scripts":{"test":"jest"}}"#)], &["a.test.ts"]).unwrap(), "npx --no-install jest --ci 'a.test.ts'");
        assert_eq!(cmd(&[("package.json", "{}")], &["test/x.test.js"]).unwrap(), "node --test 'test/x.test.js'");
        assert_eq!(cmd(&[("Cargo.toml", "")], &["tests/overflow.rs"]).unwrap(), "cargo test --offline --test 'overflow'");
        assert!(cmd(&[("Cargo.toml", "")], &["tests/common/mod.rs"]).is_err());
        let toml = "[package]\nname = \"regex\"\n\n[[test]]\npath = \"tests/test_default.rs\"\nname = \"default\"\n\n[[test]]\nname = \"crates\"\n";
        assert_eq!(cmd(&[("Cargo.toml", toml)], &["tests/test_default.rs"]).unwrap(), "cargo test --offline --test 'default'");
        assert!(cmd(&[("Gemfile", "")], &["test/tc_timezone.rb", "test/assets/payload.rb"]).unwrap().ends_with("'test/tc_timezone.rb'"));
        assert_eq!(
            cmd(&[("pom.xml", "")], &["src/test/java/org/x/CleanerTest.java"]).unwrap(),
            "mvn -o -B -q test -Dtest='CleanerTest' -DfailIfNoTests=false -Dsurefire.failIfNoSpecifiedTests=false"
        );
        assert_eq!(cmd(&[("build.gradle", "")], &["src/test/kotlin/AT.kt"]).unwrap(), "gradle --no-daemon --offline -q test --tests 'AT'");
        assert_eq!(cmd(&[("Gemfile", "")], &["spec/a_spec.rb"]).unwrap(), "bundle exec rspec 'spec/a_spec.rb'");
        assert!(cmd(&[("Gemfile", "")], &["test/spec_utils.rb"]).unwrap().starts_with("bundle exec ruby"));
        assert!(cmd(&[("x.gemspec", "")], &["test/tc_a.rb"]).unwrap().starts_with("bundle exec ruby"));
        assert!(cmd(&[("lib/a.rb", "")], &["test/tc_a.rb"]).unwrap().starts_with("ruby -Itest"));
        assert_eq!(
            cmd(&[("composer.json", "{}")], &["tests/FooTest.php"]).unwrap(),
            "for f in 'tests/FooTest.php'; do vendor/bin/phpunit \"$f\" || exit 1; done"
        );
        assert!(cmd(&[("Makefile", "")], &["tests/t.c"]).is_err());
    }
}
