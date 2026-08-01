//! Theme loading and color resolution (pi-compatible JSON themes).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

const BUILTIN_DARK: &str = include_str!("../../themes/dark.json");
const BUILTIN_LIGHT: &str = include_str!("../../themes/light.json");

#[derive(Debug, Clone, Deserialize)]
struct ThemeFile {
    name: String,
    #[serde(default)]
    vars: HashMap<String, serde_json::Value>,
    colors: HashMap<String, serde_json::Value>,
}

/// Resolved theme colors for the TUI.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Theme name.
    pub name: String,
    colors: HashMap<String, Color>,
}

impl Theme {
    /// Built-in dark theme.
    pub fn dark() -> Self {
        Self::from_json(BUILTIN_DARK).expect("builtin dark theme")
    }

    /// Built-in light theme.
    pub fn light() -> Self {
        Self::from_json(BUILTIN_LIGHT).expect("builtin light theme")
    }

    /// Parse theme JSON.
    pub fn from_json(raw: &str) -> anyhow::Result<Self> {
        let file: ThemeFile = serde_json::from_str(raw)?;
        let mut vars: HashMap<String, Color> = HashMap::new();
        for (k, v) in &file.vars {
            if let Some(c) = parse_color_value(v, &vars) {
                vars.insert(k.clone(), c);
            }
        }
        let mut colors = HashMap::new();
        for (k, v) in &file.colors {
            if let Some(c) = parse_color_value(v, &vars) {
                colors.insert(k.clone(), c);
            }
        }
        Ok(Self {
            name: file.name,
            colors,
        })
    }

    /// Load theme by name from builtins + search dirs.
    pub fn load(name: &str, search_dirs: &[PathBuf]) -> anyhow::Result<Self> {
        if name == "dark" {
            return Ok(Self::dark());
        }
        if name == "light" {
            return Ok(Self::light());
        }
        for dir in search_dirs {
            let path = dir.join(format!("{name}.json"));
            if path.exists() {
                let raw = std::fs::read_to_string(path)?;
                return Self::from_json(&raw);
            }
        }
        anyhow::bail!("theme not found: {name}")
    }

    /// List available theme names.
    pub fn list(search_dirs: &[PathBuf]) -> Vec<String> {
        let mut names = vec!["dark".into(), "light".into()];
        for dir in search_dirs {
            if !dir.is_dir() {
                continue;
            }
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("json") {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            if stem != "dark" && stem != "light" && !names.iter().any(|n| n == stem)
                            {
                                names.push(stem.to_string());
                            }
                        }
                    }
                }
            }
        }
        names.sort();
        names
    }

    /// Get a color token, falling back to Reset.
    pub fn get(&self, key: &str) -> Color {
        self.colors.get(key).copied().unwrap_or(Color::Reset)
    }

    /// Style for a token.
    pub fn style(&self, key: &str) -> Style {
        Style::default().fg(self.get(key))
    }

    /// Accent style.
    pub fn accent(&self) -> Style {
        self.style("accent")
    }

    /// Muted style.
    pub fn muted(&self) -> Style {
        self.style("muted")
    }

    /// Error style.
    pub fn error(&self) -> Style {
        self.style("error")
    }

    /// Success style.
    pub fn success(&self) -> Style {
        self.style("success")
    }

    /// Dim style.
    pub fn dim(&self) -> Style {
        self.style("dim")
    }

    /// Bold accent.
    pub fn accent_bold(&self) -> Style {
        self.accent().add_modifier(Modifier::BOLD)
    }
}

fn parse_color_value(v: &serde_json::Value, vars: &HashMap<String, Color>) -> Option<Color> {
    match v {
        serde_json::Value::String(s) if s.is_empty() => Some(Color::Reset),
        serde_json::Value::String(s) if s.starts_with('#') => parse_hex(s),
        serde_json::Value::String(s) => vars.get(s).copied().or_else(|| named_color(s)),
        serde_json::Value::Number(n) => n.as_u64().map(|i| Color::Indexed(i as u8)),
        _ => None,
    }
}

fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

fn named_color(s: &str) -> Option<Color> {
    Some(match s.to_lowercase().as_str() {
        "red" => Color::Red,
        "green" => Color::Green,
        "blue" => Color::Blue,
        "yellow" => Color::Yellow,
        "cyan" => Color::Cyan,
        "magenta" => Color::Magenta,
        "white" => Color::White,
        "black" => Color::Black,
        "gray" | "grey" => Color::Gray,
        _ => return None,
    })
}

/// Theme search directories for agent + project.
pub fn theme_search_dirs(agent_dir: &Path, project_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = vec![agent_dir.join("themes")];
    if let Some(p) = project_dir {
        dirs.push(p.join("themes"));
    }
    dirs
}
