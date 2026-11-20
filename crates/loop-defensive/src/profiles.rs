//! Language profiles for `/patch <url|folder>`: which toolchain image to build,
//! how to install dependencies (at build time, network on), and the build and
//! test commands the grader runs (no network).
//!
//! Detection looks at the manifest files at the repository root first, then at
//! the most common source extension. Anything unrecognised gets the `generic`
//! profile (Debian with compilers): the agent supplies its own reproduction
//! command, and grading skips what it cannot run.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use regex::Regex;

/// One language's sandbox and grading setup.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Profile {
    /// `go`, `node`, `python`, `rust`, `maven`, `gradle`, `ruby`, `php`, `c` or `generic`.
    pub lang: String,
    /// Human-readable name.
    pub label: String,
    /// Base image.
    pub image: String,
    /// Shell run in the source directory at build time (network on).
    pub deps: String,
    /// Grader step C (`""` skips it).
    pub build: String,
    /// The existing test suite, grader step E (`""` skips it).
    pub test: String,
    /// Extra Debian packages.
    pub apt: String,
    /// Runtime environment (offline settings).
    pub env: BTreeMap<String, String>,
    /// Extra Containerfile lines before the dependency step.
    pub pre: String,
    /// Shell run at build time to capture analyzer output, best effort.
    pub analyzers: String,
    /// Example reproduction command, for the agent.
    pub example: String,
}

impl Profile {
    fn new(lang: &str, label: &str, image: &str) -> Self {
        Self {
            lang: lang.into(),
            label: label.into(),
            image: image.into(),
            ..Self::default()
        }
    }
}

fn read(root: &Path, name: &str) -> String {
    let p = root.join(name);
    match fs::symlink_metadata(&p) {
        Ok(m) if m.file_type().is_file() => {
            String::from_utf8_lossy(&fs::read(&p).unwrap_or_default()).into_owned()
        }
        _ => String::new(),
    }
}

fn version(text: &str, default: &str, rx: &str) -> String {
    Regex::new(rx)
        .ok()
        .and_then(|re| re.captures(text).map(|c| c[1].to_owned()))
        .unwrap_or_else(|| default.to_owned())
}

/// The most common language among source files under `root` (`unknown` when none).
pub fn detect_language(root: &Path) -> String {
    let exts = [
        ("go", "Go"), ("py", "Python"), ("js", "JavaScript"), ("ts", "TypeScript"),
        ("rs", "Rust"), ("java", "Java"), ("rb", "Ruby"), ("c", "C"), ("cc", "C++"),
        ("cpp", "C++"), ("php", "PHP"),
    ];
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for e in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .flatten()
    {
        if !e.file_type().is_file() {
            continue;
        }
        let ext = e.path().extension().and_then(|x| x.to_str()).unwrap_or("");
        if let Some((_, lang)) = exts.iter().find(|(x, _)| *x == ext) {
            match counts.iter_mut().find(|(l, _)| l == lang) {
                Some((_, n)) => *n += 1,
                None => counts.push((lang, 1)),
            }
        }
    }
    // Python's max() keeps the first of equal counts.
    let mut best: Option<(&str, usize)> = None;
    for (l, n) in counts {
        if best.is_none_or(|(_, b)| n > b) {
            best = Some((l, n));
        }
    }
    best.map_or_else(|| "unknown".to_owned(), |(l, _)| l.to_owned())
}

/// The Go version to build with: `go` in go.mod, at least 1.21.
pub fn go_version(gomod: &str) -> String {
    let re = Regex::new(r"(?m)^go\s+(\d+)\.(\d+)").unwrap();
    let (major, minor) = re
        .captures(gomod)
        .map(|c| (c[1].parse().unwrap_or(1), c[2].parse().unwrap_or(21)))
        .unwrap_or((1u32, 21u32));
    let minor = if major == 1 { minor.max(21) } else { minor };
    format!("{major}.{minor}")
}

/// Pick the profile for the source tree at `root`.
pub fn detect(root: &Path) -> Profile {
    let has = |n: &str| root.join(n).is_file();
    if has("go.mod") {
        return Profile::new("go", "Go", "");
    }
    if has("Cargo.toml") {
        let mut p = Profile::new("rust", "Rust (cargo)", "docker.io/library/rust:1-bookworm");
        p.deps = "cargo fetch && (cargo build --tests || true)".into();
        p.build = "cargo build --offline".into();
        p.test = "cargo test --offline".into();
        p.env.insert("CARGO_NET_OFFLINE".into(), "true".into());
        p.example = "cargo test --offline --test repro".into();
        return p;
    }
    if has("package.json") {
        return node(root);
    }
    if ["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt", "Pipfile"].iter().any(|n| has(n)) {
        return python(root);
    }
    if has("pom.xml") {
        let mvn = if has("mvnw") { "./mvnw" } else { "mvn" };
        let jdk = jdk_for(&read(root, "pom.xml"));
        let mut p = Profile::new("maven", "Java (Maven)", &format!("docker.io/library/maven:3-eclipse-temurin-{jdk}"));
        p.deps = format!(
            // Run one existing test class: surefire fetches its test provider
            // (JUnit 4 or 5) only when it actually runs a test.
            "chmod +x mvnw 2>/dev/null; {mvn} -B -q dependency:go-offline; \
             t=$(find . -path '*/src/test/*' -name '*Test.java' | head -1 | xargs -r basename | sed 's/\\.java$//'); \
             ({mvn} -B -q test -Dtest=\"${{t:-DefensiveWarmUp}}\" -DfailIfNoTests=false -Dsurefire.failIfNoSpecifiedTests=false) || true"
        );
        p.build = format!("{mvn} -o -B -q -DskipTests compile");
        p.test = format!("{mvn} -o -B -q test");
        p.example = format!("{mvn} -o -B -q test -Dtest=ReproTest");
        return p;
    }
    if ["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"].iter().any(|n| has(n)) {
        let gr = if has("gradlew") { "./gradlew" } else { "gradle" };
        let build = ["build.gradle", "build.gradle.kts"].iter().map(|n| read(root, n)).collect::<String>();
        let jdk = jdk_for(&build);
        let mut p = Profile::new("gradle", "Java/Kotlin (Gradle)", &format!("docker.io/library/gradle:8-jdk{jdk}"));
        p.deps = format!("chmod +x gradlew 2>/dev/null; {gr} --no-daemon -q testClasses || true");
        p.build = format!("{gr} --no-daemon --offline -q classes");
        p.test = format!("{gr} --no-daemon --offline -q test");
        p.example = format!("{gr} --no-daemon --offline test --tests ReproTest");
        return p;
    }
    if has("Gemfile") {
        let ver = version(&read(root, ".ruby-version"), "3.3", r"(\d+\.\d+)");
        let mut p = Profile::new("ruby", "Ruby (bundler)", &format!("docker.io/library/ruby:{ver}-bookworm"));
        p.deps = "bundle install --jobs 4".into();
        p.test = if root.join("spec").is_dir() {
            "bundle exec rspec".into()
        } else if has("Rakefile") {
            "bundle exec rake test".into()
        } else {
            String::new()
        };
        p.example = "bundle exec rspec spec/repro_spec.rb".into();
        return p;
    }
    let gemspec = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".gemspec"));
    if gemspec {
        // A gem without a Gemfile: install its gemspec's dependencies through one
        // generated in the image only.
        let ver = version(&read(root, ".ruby-version"), "3.3", r"(\d+\.\d+)");
        let mut p = Profile::new("ruby", "Ruby (gemspec)", &format!("docker.io/library/ruby:{ver}-bookworm"));
        p.deps = "printf \"source 'https://rubygems.org'\\ngemspec\\ngem 'rake'\\ngem 'minitest'\\ngem 'test-unit'\\n\" > Gemfile && bundle install --jobs 4".into();
        p.test = if has("Rakefile") { "bundle exec rake test".into() } else { String::new() };
        p.example = "bundle exec ruby -Itest -Ilib test/repro_test.rb".into();
        return p;
    }
    if has("composer.json") {
        let mut p = Profile::new("php", "PHP (composer)", "docker.io/library/php:8.3-cli-bookworm");
        p.apt = "unzip".into();
        p.pre = "COPY --from=docker.io/library/composer:2 /usr/bin/composer /usr/bin/composer".into();
        p.deps = "composer install --no-interaction --no-progress".into();
        if has("phpunit.xml") || has("phpunit.xml.dist") {
            p.test = "vendor/bin/phpunit".into();
        }
        p.env.insert("COMPOSER_DISABLE_NETWORK".into(), "1".into());
        p.example = "vendor/bin/phpunit tests/ReproTest.php".into();
        return p;
    }
    if has("CMakeLists.txt") || has("Makefile") || has("configure") {
        return c(root);
    }
    let lang = detect_language(root);
    match lang.as_str() {
        "JavaScript" | "TypeScript" => return node(root),
        "Python" => return python(root),
        "C" | "C++" => return c(root),
        "Ruby" => {
            let mut p = Profile::new("ruby", "Ruby", "docker.io/library/ruby:3.3-bookworm");
            p.example = "ruby -Itest test/repro_test.rb".into();
            return p;
        }
        "PHP" => {
            let mut p = Profile::new("php", "PHP", "docker.io/library/php:8.3-cli-bookworm");
            p.example = "php tests/repro_test.php".into();
            return p;
        }
        _ => {}
    }
    // Static sites (HTML/CSS/JS without package.json) usually test best under node.
    let web = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .flatten()
        .any(|e| {
            e.file_type().is_file()
                && matches!(
                    e.path().extension().and_then(|x| x.to_str()),
                    Some("html" | "htm" | "css" | "vue" | "svelte")
                )
        });
    if web {
        return node(root);
    }
    let mut p = Profile::new(
        "generic",
        &format!("generic ({lang})"),
        "docker.io/library/buildpack-deps:bookworm",
    );
    p.apt = "build-essential".into();
    p.example = "bash tests/repro.sh".into();
    p
}

/// The JDK image (8, 11, 17 or 21) for the Java level a build file declares;
/// 21 when it declares none. Old source levels do not compile on new JDKs.
pub fn jdk_for(build: &str) -> u32 {
    let re = Regex::new(
        r"(?x)
        <(?:maven\.compiler\.(?:source|target|release)|source|target|release|java\.version|jdk\.version)>\s*(?:1\.)?(\d+)
        | (?:sourceCompatibility|targetCompatibility)\s*=\s*['\x22]?(?:JavaVersion\.VERSION_)?(?:1[._])?(\d+)
        | languageVersion\.set\(\s*JavaLanguageVersion\.of\(\s*(\d+)",
    )
    .unwrap();
    let level = re
        .captures_iter(build)
        .filter_map(|c| c.iter().skip(1).flatten().next().and_then(|m| m.as_str().parse::<u32>().ok()))
        .filter(|v| (1..=30).contains(v))
        .min();
    match level {
        None => 21,
        Some(v) if v <= 8 => 8,
        Some(v) if v <= 11 => 11,
        Some(v) if v <= 17 => 17,
        Some(_) => 21,
    }
}

fn node(root: &Path) -> Profile {
    let pkg: serde_json::Value = serde_json::from_str(&read(root, "package.json")).unwrap_or_default();
    let engines = pkg
        .get("engines")
        .and_then(|e| e.get("node"))
        .map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))
        .unwrap_or_default();
    let nvmrc = read(root, ".nvmrc");
    let ver = version(if nvmrc.is_empty() { &engines } else { &nvmrc }, "22", r"(\d+)");
    let ver = if ver.parse::<u32>().unwrap_or(0) >= 18 { ver } else { "22".into() };
    let deps = if root.join("pnpm-lock.yaml").is_file() {
        "corepack enable && pnpm install --frozen-lockfile --ignore-scripts"
    } else if root.join("yarn.lock").is_file() {
        "corepack enable && (yarn install --frozen-lockfile --ignore-scripts || yarn install --ignore-scripts)"
    } else if root.join("package-lock.json").is_file() {
        "npm ci --no-audit --no-fund --ignore-scripts || npm install --no-audit --no-fund --ignore-scripts"
    } else if root.join("package.json").is_file() {
        "npm install --no-audit --no-fund --ignore-scripts"
    } else {
        ""
    };
    let script = pkg
        .get("scripts")
        .and_then(|s| s.get("test"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    let mut p = Profile::new(
        "node",
        "JavaScript/TypeScript (node)",
        &format!("docker.io/library/node:{ver}-bookworm"),
    );
    p.deps = deps.into();
    if !script.is_empty() && !script.contains("no test specified") {
        p.test = "npm test".into();
    }
    p.env.insert("npm_config_offline".into(), "true".into());
    p.env.insert("CI".into(), "true".into());
    p.analyzers =
        "(test -f package-lock.json && npm audit --json > /opt/defensive/npm-audit.json) || true".into();
    p.example = "node --test test/repro.test.js".into();
    p
}

fn python(root: &Path) -> Profile {
    let ver = version(&read(root, ".python-version"), "3.12", r"(3\.\d+)");
    let mut p = Profile::new("python", "Python", &format!("docker.io/library/python:{ver}-bookworm"));
    p.deps = "pip install -q --no-cache-dir pytest bandit; \
              for f in requirements*.txt requirements/*.txt; do [ -f \"$f\" ] && \
              pip install -q --no-cache-dir -r \"$f\"; done; \
              if [ -f pyproject.toml ] || [ -f setup.py ]; then pip install -q --no-cache-dir \".[test,tests,testing,dev,crypto]\" || pip install -q --no-cache-dir . || true; fi"
        .into();
    p.build = "python -m compileall -q .".into();
    p.test = "python -m pytest -q -p no:cacheprovider".into();
    p.env.insert("PIP_NO_INDEX".into(), "1".into());
    p.example = "python -m pytest -q tests/test_repro.py".into();
    p
}

fn c(root: &Path) -> Profile {
    let (build, test) = if root.join("CMakeLists.txt").is_file() {
        (
            "cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug >/dev/null && cmake --build build -j4".to_owned(),
            "ctest --test-dir build --output-on-failure".to_owned(),
        )
    } else if root.join("Makefile").is_file() {
        let has_test = Regex::new(r"(?m)^(test|check)\s*:").unwrap().is_match(&read(root, "Makefile"));
        ("make -j4".to_owned(), if has_test { "make test".to_owned() } else { String::new() })
    } else {
        ("./configure && make -j4".to_owned(), String::new())
    };
    let mut p = Profile::new("c", "C/C++", "docker.io/library/gcc:14-bookworm");
    p.apt = "cmake".into();
    p.build = build;
    p.test = test;
    p.example = "bash tests/repro.sh".into();
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = dir.path().join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        dir
    }

    #[test]
    fn node_project_with_lockfile_matches_the_recorded_run() {
        let d = tree(&[("package.json", r#"{"scripts":{"test":"echo \"Error: no test specified\""}}"#), ("package-lock.json", "{}")]);
        let p = detect(d.path());
        assert_eq!(p.lang, "node");
        assert_eq!(p.label, "JavaScript/TypeScript (node)");
        assert_eq!(p.image, "docker.io/library/node:22-bookworm");
        assert_eq!(
            p.deps,
            "npm ci --no-audit --no-fund --ignore-scripts || npm install --no-audit --no-fund --ignore-scripts"
        );
        assert_eq!(p.test, "");
        assert_eq!(p.build, "");
    }

    #[test]
    fn manifests_win_over_extensions() {
        assert_eq!(detect(tree(&[("go.mod", "module x\n")]).path()).lang, "go");
        assert_eq!(detect(tree(&[("Cargo.toml", "")]).path()).lang, "rust");
        assert_eq!(detect(tree(&[("requirements.txt", "")]).path()).lang, "python");
        assert_eq!(detect(tree(&[("Makefile", "test:\n\techo\n")]).path()).test, "make test");
        assert_eq!(detect(tree(&[("x.py", "")]).path()).lang, "python");
        assert_eq!(detect(tree(&[("index.html", "")]).path()).lang, "node");
        assert_eq!(detect(tree(&[("README", "")]).path()).lang, "generic");
        let gem = detect(tree(&[("tzinfo.gemspec", ""), ("Rakefile", ""), ("lib/a.rb", "")]).path());
        assert_eq!((gem.lang.as_str(), gem.test.as_str()), ("ruby", "bundle exec rake test"));
        assert!(gem.deps.contains("gemspec"));
    }

    #[test]
    fn jdk_from_build_files() {
        assert_eq!(jdk_for("<properties><maven.compiler.source>1.6</maven.compiler.source></properties>"), 8);
        assert_eq!(jdk_for("<source>11</source><target>11</target>"), 11);
        assert_eq!(jdk_for("<release>17</release>"), 17);
        assert_eq!(jdk_for("sourceCompatibility = JavaVersion.VERSION_1_8"), 8);
        assert_eq!(jdk_for("java { toolchain { languageVersion.set(JavaLanguageVersion.of(21)) } }"), 21);
        assert_eq!(jdk_for("<project></project>"), 21);
        let d = tree(&[("pom.xml", "<maven.compiler.target>1.7</maven.compiler.target>")]);
        assert_eq!(detect(d.path()).image, "docker.io/library/maven:3-eclipse-temurin-8");
    }

    #[test]
    fn go_version_floor() {
        assert_eq!(go_version("module x\n\ngo 1.18\n"), "1.21");
        assert_eq!(go_version("go 1.22.3\n"), "1.22");
        assert_eq!(go_version(""), "1.21");
    }

    #[test]
    fn node_version_from_nvmrc() {
        let d = tree(&[("package.json", "{}"), (".nvmrc", "v20.1\n")]);
        assert_eq!(detect(d.path()).image, "docker.io/library/node:20-bookworm");
    }
}
