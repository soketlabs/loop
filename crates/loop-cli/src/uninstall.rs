//! Removal of the agent data directory for `loop uninstall --purge`.

use std::{
    fs,
    io::{self, BufRead, Write},
    path::Path,
};

/// Whether a confirmation answer means yes. Anything else, an empty line
/// included, keeps the data.
pub fn confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Ask whether to delete `agent_dir`. Reads one line; end of input means no.
pub fn confirm(
    agent_dir: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<bool> {
    write!(
        output,
        "This removes settings, credentials, skills, sessions and usage data in {}.\nContinue? [y/N] ",
        agent_dir.display()
    )?;
    output.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    Ok(confirmed(&answer))
}

/// Remove the agent data directory. `Ok(false)` when it was already gone.
pub fn remove_agent_data(agent_dir: &Path) -> io::Result<bool> {
    match fs::remove_dir_all(agent_dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_y_and_yes_confirm() {
        for answer in ["y", "Y", "yes", "YES", " yes \n"] {
            assert!(confirmed(answer), "{answer:?} should confirm");
        }
        for answer in ["", "\n", "n", "no", "yeah", "purge"] {
            assert!(!confirmed(answer), "{answer:?} must not confirm");
        }
    }

    #[test]
    fn prompt_names_the_directory_and_defaults_to_no() {
        let mut input = &b""[..];
        let mut output = Vec::new();
        assert!(!confirm(Path::new("/fixture/agent"), &mut input, &mut output).unwrap());
        let prompt = String::from_utf8(output).unwrap();
        assert!(prompt.contains("/fixture/agent"), "{prompt}");
        assert!(prompt.contains("[y/N]"), "{prompt}");
    }

    #[test]
    fn confirm_accepts_a_typed_yes() {
        let mut input = &b"y\n"[..];
        let mut output = Vec::new();
        assert!(confirm(Path::new("/fixture/agent"), &mut input, &mut output).unwrap());
    }

    #[test]
    fn remove_agent_data_deletes_the_tree_then_reports_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        fs::create_dir_all(agent.join("sessions")).unwrap();
        fs::write(agent.join("auth.json"), "token").unwrap();
        fs::write(agent.join("sessions/session.jsonl"), "{}").unwrap();

        assert!(remove_agent_data(&agent).unwrap());
        assert!(!agent.exists());
        assert!(!remove_agent_data(&agent).unwrap());
    }
}
