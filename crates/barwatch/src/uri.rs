//! Making `barwatch://` links open this app.
//!
//! * Linux: a `.desktop` file that claims `x-scheme-handler/barwatch`. A package
//!   installs one system-wide; for a bare binary we write one for the user.
//! * Windows: three registry values under `HKCU\Software\Classes\barwatch`. No
//!   installer and no administrator rights. Because a link breaks if the downloaded
//!   file is later moved or deleted, the app first copies itself to
//!   `%LOCALAPPDATA%\Programs\barwatch` and registers that copy.

use anyhow::Result;

/// Whether links currently open this app, as far as can be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Registered,
    /// Not registered, or registered for something else.
    Missing,
    /// This platform is not handled.
    #[cfg_attr(any(target_os = "linux", windows), allow(dead_code))]
    Unsupported,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Status;
    use anyhow::{Context, Result};
    use std::path::PathBuf;
    use std::process::Command;

    const DESKTOP_FILE: &str = "barwatch.desktop";
    const MIME: &str = "x-scheme-handler/barwatch";

    fn user_applications() -> Option<PathBuf> {
        Some(dirs::data_dir()?.join("applications"))
    }

    fn system_file_exists() -> bool {
        ["/usr/share/applications", "/usr/local/share/applications"]
            .iter()
            .any(|d| std::path::Path::new(d).join(DESKTOP_FILE).is_file())
    }

    pub fn status() -> Status {
        let out = Command::new("xdg-mime").args(["query", "default", MIME]).output();
        match out {
            Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == DESKTOP_FILE => Status::Registered,
            _ => Status::Missing,
        }
    }

    pub fn register() -> Result<()> {
        if !system_file_exists() {
            let exe = std::env::current_exe().context("finding this program's path")?;
            let dir = user_applications().context("no user data directory")?;
            std::fs::create_dir_all(&dir)?;
            let entry = format!(
                "[Desktop Entry]\nType=Application\nName=barwatch\nComment=Watch Beyond All Reason replays together\n\
                 Exec=\"{}\" -- %u\nIcon=barwatch\nTerminal=false\nCategories=Game;\nMimeType={MIME};\nStartupWMClass=barwatch\n",
                exe.display()
            );
            std::fs::write(dir.join(DESKTOP_FILE), entry)?;
            let _ = Command::new("update-desktop-database").arg(&dir).status();
        }
        let done = Command::new("xdg-mime")
            .args(["default", DESKTOP_FILE, MIME])
            .status()
            .context("running xdg-mime (is xdg-utils installed?)")?;
        anyhow::ensure!(done.success(), "xdg-mime refused to set the link handler");
        Ok(())
    }

    pub fn unregister() -> Result<()> {
        if let Some(dir) = user_applications() {
            let _ = std::fs::remove_file(dir.join(DESKTOP_FILE));
            let _ = Command::new("update-desktop-database").arg(&dir).status();
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::Status;
    use anyhow::{Context, Result};
    use std::path::PathBuf;
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    const KEY: &str = r"Software\Classes\barwatch";

    fn installed_path() -> Option<PathBuf> {
        Some(dirs::data_local_dir()?.join("Programs").join("barwatch").join("barwatch.exe"))
    }

    fn command_for(exe: &std::path::Path) -> String {
        // `--` ends the options: whatever a link contains, it is only ever the link.
        format!("\"{}\" -- \"%1\"", exe.display())
    }

    /// Copies this program to its permanent place, unless it already runs from there.
    /// Returns the path links should start.
    fn settle() -> Result<PathBuf> {
        let current = std::env::current_exe().context("finding this program's path")?;
        let Some(target) = installed_path() else { return Ok(current) };
        if current == target {
            return Ok(target);
        }
        std::fs::create_dir_all(target.parent().unwrap())?;
        // Fails while an older copy is running; that copy keeps working for links.
        match std::fs::copy(&current, &target) {
            Ok(_) => {}
            Err(_) if target.is_file() => {}
            Err(e) => return Err(e).context("copying barwatch to its permanent place"),
        }
        add_start_menu_entry(&target);
        Ok(target)
    }

    /// Best effort: without it the app can still be started from a link.
    fn add_start_menu_entry(target: &std::path::Path) {
        use std::os::windows::process::CommandExt;
        let Some(programs) = dirs::data_dir().map(|d| d.join(r"Microsoft\Windows\Start Menu\Programs")) else { return };
        let Some(system) = std::env::var_os("SystemRoot") else { return };
        // The paths travel as environment variables, so no path can be read as script.
        let powershell = std::path::Path::new(&system).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let script = "$s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:BARWATCH_LNK);$s.TargetPath=$env:BARWATCH_EXE;$s.Save()";
        let _ = std::process::Command::new(powershell)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("BARWATCH_LNK", programs.join("barwatch.lnk"))
            .env("BARWATCH_EXE", target)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn();
    }

    pub fn status() -> Status {
        let registered = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(format!(r"{KEY}\shell\open\command"))
            .and_then(|k| k.get_value::<String, _>(""))
            .ok();
        let expected: Vec<String> =
            [std::env::current_exe().ok(), installed_path()].into_iter().flatten().map(|p| command_for(&p)).collect();
        match registered {
            Some(cmd) if expected.iter().any(|e| e.eq_ignore_ascii_case(&cmd)) => Status::Registered,
            _ => Status::Missing,
        }
    }

    pub fn register() -> Result<()> {
        let exe = settle()?;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(KEY).context("writing the registry")?;
        key.set_value("", &"URL:barwatch")?;
        key.set_value("URL Protocol", &"")?;
        let (command, _) = hkcu.create_subkey(format!(r"{KEY}\shell\open\command"))?;
        command.set_value("", &command_for(&exe))?;
        Ok(())
    }

    pub fn unregister() -> Result<()> {
        let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(KEY);
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::Status;
    use anyhow::Result;

    pub fn status() -> Status {
        Status::Unsupported
    }

    pub fn register() -> Result<()> {
        anyhow::bail!("registering barwatch:// links is not supported on this system")
    }

    pub fn unregister() -> Result<()> {
        Ok(())
    }
}

pub fn status() -> Status {
    imp::status()
}

/// Makes `barwatch://` links open this app for the current user.
pub fn register() -> Result<()> {
    imp::register()
}

pub fn unregister() -> Result<()> {
    imp::unregister()
}

/// Reads a link the user pasted or a browser handed over: a `barwatch://` link or
/// an invite page's address. A watch-party link from barwatch 0.3 (`session=`) is
/// refused with words that say what to do instead.
pub fn parse_link(text: &str) -> Result<barwatch_core::api::Link> {
    use barwatch_core::api::Link;
    Link::parse_any(text).map_err(|e| {
        let old = url::Url::parse(text.trim()).is_ok_and(|u| u.scheme() == "barwatch" && u.query_pairs().any(|(k, _)| k == "session"));
        if old {
            anyhow::anyhow!(
                "this is an invite to a watch party of an older barwatch; watch parties have become rooms, so ask for the room's link"
            )
        } else {
            e
        }
    })
}

#[cfg(test)]
mod link_tests {
    #[test]
    fn an_old_party_link_says_what_to_do() {
        let old = "barwatch://join?server=https%3A%2F%2Fx.org%2F&session=abcdefghijklmnopqrstuv";
        let e = super::parse_link(old).unwrap_err().to_string();
        assert!(e.contains("rooms"), "{e}");
        assert!(super::parse_link("barwatch://join?server=https%3A%2F%2Fx.org%2F&room=abcdefghijklmnopqrstuv").is_ok());
        assert!(super::parse_link("nonsense").unwrap_err().to_string().contains("not a link"));
    }
}
