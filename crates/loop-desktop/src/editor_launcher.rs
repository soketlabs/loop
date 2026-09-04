//! Detect and launch external code editors.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Supported external editors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalEditor {
    Cursor,
    VsCode,
    Zed,
}

impl ExternalEditor {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cursor => "Cursor",
            Self::VsCode => "VS Code",
            Self::Zed => "Zed",
        }
    }

    fn command(self) -> Option<PathBuf> {
        let candidates: &[&str] = match self {
            Self::Cursor => &["cursor"],
            Self::VsCode => &["code"],
            Self::Zed => &["zed"],
        };
        for name in candidates {
            if let Some(p) = which(name) {
                return Some(p);
            }
        }
        #[cfg(target_os = "macos")]
        {
            let app = match self {
                Self::Cursor => "/Applications/Cursor.app/Contents/MacOS/Cursor",
                Self::VsCode => {
                    "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"
                }
                Self::Zed => "/Applications/Zed.app/Contents/MacOS/zed",
            };
            if Path::new(app).exists() {
                return Some(PathBuf::from(app));
            }
        }
        None
    }
}

/// Editors found on this machine.
pub fn detect_editors() -> Vec<ExternalEditor> {
    [ExternalEditor::Cursor, ExternalEditor::VsCode, ExternalEditor::Zed]
        .into_iter()
        .filter(|e| e.command().is_some())
        .collect()
}

/// Open `path` at `line` (1-based) in the chosen editor.
pub fn open_in_editor(editor: ExternalEditor, path: &Path, line: u32) -> anyhow::Result<()> {
    let cmd = editor
        .command()
        .ok_or_else(|| anyhow::anyhow!("{} not installed", editor.label()))?;
    let path_str = path.to_string_lossy();
    let status = match editor {
        ExternalEditor::Cursor | ExternalEditor::VsCode => Command::new(cmd)
            .args(["--goto", &format!("{path_str}:{line}")])
            .status()?,
        ExternalEditor::Zed => Command::new(cmd)
            .args([path_str.as_ref(), &line.to_string()])
            .status()?,
    };
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("failed to open {} in {}", path.display(), editor.label())
    }
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let full = dir.join(name);
            if full.is_file() {
                Some(full)
            } else {
                None
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_does_not_panic() {
        let _ = detect_editors();
    }
}
