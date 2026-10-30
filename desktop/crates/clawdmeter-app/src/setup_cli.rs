//! Setup from the command line, for `tools/setup/windows.ps1` and anyone
//! scripting an install: what the Hooks page's Install hooks and Settings →
//! Start at login do, without the window.

use std::path::PathBuf;

use clawdmeter_core::autostart::{self, LaunchMode};
use clawdmeter_core::{launch, Paths, Settings};

use crate::hooks::install::{self as hooks, Agent, CODEX_TRUST};
use crate::{errors, tilde};

/// Every agent's hooks this computer has, as the Hooks page lists them: each
/// Claude Code account, then Codex's and Cursor's homes once they exist (the
/// installer never creates those folders).
pub fn hook_targets(paths: &Paths, settings: &Settings) -> Vec<(PathBuf, Agent)> {
    let others = [(&paths.codex_home, Agent::Codex), (&paths.cursor_home, Agent::Cursor)];
    settings
        .config_dirs
        .iter()
        .map(|d| (d.clone(), Agent::Claude))
        .chain(others.into_iter().filter(|(h, _)| h.is_dir()).map(|(h, a)| (h.clone(), a)))
        .collect()
}

/// The executable Start at login runs, unless it lives somewhere that won't
/// last (a zip or temp folder, a disk image).
fn stable_exe() -> Result<PathBuf, String> {
    let exe = launch::stable_launch_path().map_err(|_| "Couldn't locate the Agentsmeter executable.".to_owned())?;
    match launch::unstable(&exe) {
        Some(u) => Err(format!("Agentsmeter is running from {}. {}", u.location_noun(), u.advice())),
        None => Ok(exe),
    }
}

/// `--install-hooks`: install or update Clawdmeter's hooks for every agent
/// found ([`hook_targets`]). One line per file; 0 when all are in place.
pub fn install_hooks(paths: &Paths) -> i32 {
    let settings = Settings::load(paths).unwrap_or_else(|_| Settings::from_config(&Default::default(), paths, None));
    let exe = match stable_exe().and_then(|_| hooks::hook_binary(&paths.state_dir).map_err(|e| errors::io_message(&e)))
    {
        Ok(exe) => exe,
        Err(why) => {
            eprintln!("{why}");
            return 1;
        }
    };
    let mut code = 0;
    for (dir, agent) in hook_targets(paths, &settings) {
        let file = tilde(&hooks::hooks_file(&dir, agent).display().to_string(), &paths.home);
        let done = hooks::plan_install(&dir, &exe, agent).and_then(|p| {
            if p.changes() {
                p.apply().map(|()| "installed")
            } else {
                Ok("already installed")
            }
        });
        match done {
            Ok(what) if agent == Agent::Codex => println!("{file}: {} hooks {what}. {CODEX_TRUST}", agent.name()),
            Ok(what) => println!("{file}: {} hooks {what}.", agent.name()),
            Err(e) => {
                eprintln!("{file}: {e}");
                code = 1;
            }
        }
    }
    code
}

/// `--start-at-login on|off`: Settings → Start at login.
pub fn start_at_login(paths: &Paths, on: bool) -> i32 {
    let done = if on {
        stable_exe()
            .and_then(|exe| autostart::enable(&paths.home, &exe, LaunchMode::Tray).map_err(|e| errors::io_message(&e)))
    } else {
        autostart::disable(&paths.home, LaunchMode::Tray).map_err(|e| errors::io_message(&e))
    };
    match done {
        Ok(()) => {
            println!("Start at login is {}.", if on { "on" } else { "off" });
            0
        }
        Err(why) => {
            eprintln!("{why}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawdmeter_core::config::ConfigFile;

    // Codex and Cursor are listed once their folder exists, after Claude's.
    #[test]
    fn hook_targets_follow_installed_agents() {
        let root = std::env::temp_dir().join(format!("cm-targets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::under(&root);
        let settings = Settings::from_config(&ConfigFile::parse(""), &paths, None);
        let agents = |p: &Paths| hook_targets(p, &settings).into_iter().map(|(_, a)| a).collect::<Vec<_>>();
        assert_eq!(agents(&paths), vec![Agent::Claude]);
        std::fs::create_dir_all(&paths.cursor_home).unwrap();
        assert_eq!(agents(&paths), vec![Agent::Claude, Agent::Cursor]);
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        assert_eq!(agents(&paths), vec![Agent::Claude, Agent::Codex, Agent::Cursor]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
