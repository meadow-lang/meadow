//! The Meadow toolchain's installer, behind `meadowup`.
//!
//! Everything here is about *getting Meadow onto a machine*: where it goes,
//! how a release is fetched, and how the `bin` directory joins the `PATH`.
//! Neither the compiler nor the build tool depends on any of it -- `meadowup`
//! has to work before Meadow is installed at all, which is why it is a separate
//! program with no Meadow crates behind it, and why it is the thing people
//! download rather than something the toolchain carries.
//!
//! Downloading goes through `curl` and `tar`, which ship with macOS, essentially
//! every Linux, and Windows 10 1803 and later. That is deliberate: a tool whose
//! only job is to install another one should not drag in an HTTP and TLS stack.

use std::path::{Path, PathBuf};

/// Where releases come from.
pub const REPO: &str = "meadow-lang/meadow";

/// The build tool's file name on this system.
pub const fn exe_name() -> &'static str {
    if cfg!(windows) {
        "meadow.exe"
    } else {
        "meadow"
    }
}

/// This program's own file name on this system.
pub const fn up_name() -> &'static str {
    if cfg!(windows) {
        "meadowup.exe"
    } else {
        "meadowup"
    }
}

/// Where Meadow keeps everything that is not a source file: the binaries, the
/// extracted standard library, the git cache. `MEADOW_HOME` overrides it.
pub fn home() -> PathBuf {
    if let Ok(dir) = std::env::var("MEADOW_HOME") {
        return PathBuf::from(dir);
    }
    let base = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    Path::new(&base).join(".meadow")
}

/// Where the toolchain in `home` came from, as the last install recorded it.
///
/// A build from source has the same version number as the release it was
/// built after, so the number alone cannot say which is installed -- and
/// `meadowup update` has to, to say why it did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// A published release, by tag.
    Release(String),
    /// Binaries handed over with `--from`: built from source, or unpacked by
    /// hand.
    Local,
}

fn provenance_file(home: &Path) -> PathBuf {
    home.join("toolchain")
}

/// What the last install into `home` said it was. `None` for an install that
/// predates the record.
pub fn provenance(home: &Path) -> Option<Provenance> {
    let text = std::fs::read_to_string(provenance_file(home)).ok()?;
    let line = text.lines().next()?.trim();
    match line.split_once(' ') {
        Some(("release", tag)) => Some(Provenance::Release(tag.trim().to_string())),
        _ if line == "local" => Some(Provenance::Local),
        _ => None,
    }
}

/// Record what was just installed into `home`.
pub fn record(home: &Path, what: &Provenance) -> std::io::Result<()> {
    let line = match what {
        Provenance::Release(tag) => format!("release {tag}"),
        Provenance::Local => "local".to_string(),
    };
    std::fs::write(
        provenance_file(home),
        format!("{line}\n# Written by meadowup: where the installed toolchain came from.\n"),
    )
}

/// Which releases `meadowup update` follows, as rustup's channels: `stable`,
/// the newest release with a version, or `nightly`, the build of `master`
/// published under the tag [`NIGHTLY_TAG`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Nightly,
}

/// The tag the nightly build is published under. It moves; a version's tag
/// does not.
pub const NIGHTLY_TAG: &str = "nightly";

impl Channel {
    pub fn named(name: &str) -> Option<Channel> {
        match name {
            "stable" => Some(Channel::Stable),
            "nightly" => Some(Channel::Nightly),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Nightly => "nightly",
        }
    }
}

fn channel_file(home: &Path) -> PathBuf {
    home.join("channel")
}

/// The channel `home` follows: `stable` until `meadowup default` says
/// otherwise.
pub fn channel(home: &Path) -> Channel {
    std::fs::read_to_string(channel_file(home))
        .ok()
        .and_then(|text| text.lines().next().and_then(|l| Channel::named(l.trim())))
        .unwrap_or(Channel::Stable)
}

/// Have `home` follow `channel` from now on.
pub fn set_channel(home: &Path, channel: Channel) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    std::fs::write(
        channel_file(home),
        format!(
            "{}\n# Written by meadowup: the channel `meadowup update` follows.\n",
            channel.name()
        ),
    )
}

/// The release tag `wanted` names: `0.2.0` and `v0.2.0` are `v0.2.0`, and
/// anything else is taken as it is written.
pub fn tag_for(wanted: &str) -> String {
    if wanted.starts_with(|c: char| c.is_ascii_digit()) {
        format!("v{wanted}")
    } else {
        wanted.to_string()
    }
}

/// Where the binaries go, and what joins the `PATH`.
pub fn bin_dir(home: &Path) -> PathBuf {
    home.join("bin")
}

/// The shell fragment a Unix profile sources. Rustup's `~/.cargo/env`, and for
/// the same reason: one line in a profile that keeps working when the rest of
/// this changes.
pub fn env_file(home: &Path) -> PathBuf {
    home.join("env")
}

/// The triple whose release assets suit this machine.
pub fn target_triple() -> Result<&'static str, String> {
    let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        // Termux on a phone. A meadowup built for Android knows it is on
        // Android, and takes the build linked against Android's own C library
        // -- the Linux one expects glibc and does not run there.
        ("android", "aarch64") => "aarch64-linux-android",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        (os, arch) => return Err(format!("no published build for {os} on {arch}")),
    };
    Ok(triple)
}

/// What a release archive is called on this system.
pub const fn archive_ext() -> &'static str {
    if cfg!(windows) { "zip" } else { "tar.gz" }
}

/// A tag that is safe to put in a URL.
///
/// Release tags are `v1.2.3`, sometimes with a suffix. Anything else is refused
/// rather than escaped, because a tag is not a place to be clever: a `..` or a
/// `/` in one would reach somewhere else entirely.
pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        && !tag.contains("..")
}

/// A directory of this run's own, under the system's temporary one, removed
/// when the [`Scratch`] is dropped.
pub fn scratch_dir(prefix: &str) -> Result<Scratch, String> {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    Ok(Scratch(dir))
}

/// A directory that is removed when this is dropped: when an install is done,
/// and as well when a download or an install fails part way -- a `?` returns
/// past any line that would have removed it, and a download left behind is a
/// toolchain's worth of space on the system drive.
#[derive(Debug)]
pub struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Where a displaced binary is put.
///
/// A running program's image cannot be deleted on Windows, and `meadowup`
/// replacing itself is exactly that case. Moving it aside always works, and the
/// next run clears it.
pub fn backup_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Move whatever is at `dest` out of the way, answering where it went.
pub fn displace(dest: &Path) -> Option<PathBuf> {
    if !dest.exists() {
        return None;
    }
    let backup = backup_path(dest);
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(dest, &backup).ok().map(|()| backup)
}

/// What `exe --version` says, for reporting what was installed.
pub fn version_of(exe: &Path) -> Option<String> {
    let out = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

pub mod release;
pub mod ui;

/// True when this process owns its console, which on Windows means it was
/// double-clicked rather than run from a shell.
///
/// The window would vanish along with the output, so the caller holds it open.
/// Always false elsewhere: nobody double-clicks a Unix binary expecting a
/// terminal to stay.
#[cfg(windows)]
pub fn launched_by_double_click() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    let mut pids = [0u32; 4];
    let count = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    count == 1
}

#[cfg(not(windows))]
pub fn launched_by_double_click() -> bool {
    false
}

/// Wait for a keypress, so that what was printed can be read.
pub fn wait_for_enter() {
    use std::io::Write;
    println!();
    print!("Press Enter to close this window...");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// Adding and removing the `bin` directory from the user's `PATH`.
///
/// Two quite different jobs: on Windows the user `PATH` is a registry value, and
/// everywhere else it is whatever the shell profiles say, which is why this is a
/// file they source rather than a line this writes into each of them.
pub mod path {
    #[cfg(windows)]
    pub(crate) mod imp {

        use std::path::Path;
        use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, RegType};
        use winreg::{RegKey, RegValue};

        /// Add `dir` to the user `PATH`. `Ok(false)` if it was already there.
        ///
        /// The value is read and written **raw**: `PATH` is often a `REG_EXPAND_SZ`
        /// holding entries like `%JAVA_HOME%\bin`, and reading it through an API that
        /// expands those would bake the expansion in permanently.
        pub fn add(dir: &Path) -> Result<bool, String> {
            let dir = dir.to_string_lossy().to_string();
            let (value, kind) = read()?;
            if entries(&value).any(|e| eq_path(e, &dir)) {
                return Ok(false);
            }
            let mut updated: Vec<String> = entries(&value).map(str::to_string).collect();
            updated.push(dir);
            write(&updated.join(";"), kind)?;
            broadcast();
            Ok(true)
        }

        /// Remove `dir` from the user `PATH`. `Ok(false)` if it was not there — in
        /// which case the registry is left completely untouched.
        pub fn remove(dir: &Path) -> Result<bool, String> {
            let dir = dir.to_string_lossy().to_string();
            let (value, kind) = read()?;
            if !entries(&value).any(|e| eq_path(e, &dir)) {
                return Ok(false);
            }
            let kept: Vec<String> = entries(&value)
                .filter(|e| !eq_path(e, &dir))
                .map(str::to_string)
                .collect();
            write(&kept.join(";"), kind)?;
            broadcast();
            Ok(true)
        }

        pub(crate) fn entries(value: &str) -> impl Iterator<Item = &str> {
            value.split(';').filter(|e| !e.is_empty())
        }

        /// Windows paths are case-insensitive, and a trailing slash means nothing.
        pub(crate) fn eq_path(a: &str, b: &str) -> bool {
            let norm = |s: &str| s.trim_end_matches(['\\', '/']).to_lowercase();
            norm(a) == norm(b)
        }

        fn env_key(write: bool) -> Result<RegKey, String> {
            let access = if write {
                KEY_READ | KEY_WRITE
            } else {
                KEY_READ
            };
            RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey_with_flags("Environment", access)
                .map_err(|e| format!("could not open HKCU\\Environment: {e}"))
        }

        fn read() -> Result<(String, RegType), String> {
            let key = env_key(false)?;
            match key.get_raw_value("Path") {
                // `get_raw_value` does not expand `%VARS%`, which is what we want.
                Ok(raw) => {
                    let text = String::from_utf16_lossy(&as_u16(&raw.bytes))
                        .trim_end_matches('\0')
                        .to_string();
                    Ok((text, raw.vtype))
                }
                // No user PATH yet: create one.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Ok((String::new(), RegType::REG_EXPAND_SZ))
                }
                // Anything else is a PATH that exists and could not be read.
                // Carrying on as if it were empty would write `bin` back as the
                // user's entire PATH, so refuse, and leave the value as it is.
                Err(e) => Err(format!("could not read PATH: {e}")),
            }
        }

        fn write(value: &str, kind: RegType) -> Result<(), String> {
            // An entry with `%VARS%` only works from a REG_EXPAND_SZ value.
            let kind = if value.contains('%') {
                RegType::REG_EXPAND_SZ
            } else {
                kind
            };
            let mut bytes: Vec<u8> = Vec::with_capacity(value.len() * 2 + 2);
            for unit in value.encode_utf16().chain(std::iter::once(0)) {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            let key = env_key(true)?;
            key.set_raw_value("Path", &RegValue { bytes, vtype: kind })
                .map_err(|e| format!("could not write PATH: {e}"))
        }

        fn as_u16(bytes: &[u8]) -> Vec<u16> {
            bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect()
        }

        /// Tell running programs the environment changed, so a newly opened terminal
        /// picks up the new `PATH` without a logout.
        fn broadcast() {
            use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
            };
            const HWND_BROADCAST: HWND = 0xffff as HWND;
            let env: Vec<u16> = "Environment"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut result = 0usize;
            unsafe {
                SendMessageTimeoutW(
                    HWND_BROADCAST,
                    WM_SETTINGCHANGE,
                    0 as WPARAM,
                    env.as_ptr() as LPARAM,
                    SMTO_ABORTIFHUNG,
                    5000,
                    &mut result,
                );
            }
        }
    }

    #[cfg(not(windows))]
    pub(crate) mod imp {
        use super::super::{bin_dir, env_file};
        use std::io::Write;
        use std::path::Path;

        /// The profiles a login or interactive shell reads. All of them that
        /// exist are edited: which one is read depends on the shell and on how
        /// it was started, and adding a line to each is cheaper than being
        /// wrong.
        pub(crate) fn profiles_in(home: &Path) -> Vec<std::path::PathBuf> {
            [".profile", ".bash_profile", ".bashrc", ".zshenv"]
                .iter()
                .map(|n| home.join(n))
                .collect()
        }

        fn profiles() -> Vec<std::path::PathBuf> {
            match std::env::var_os("HOME") {
                Some(home) => profiles_in(Path::new(&home)),
                None => Vec::new(),
            }
        }

        /// The line a profile carries. It names the env file, which is what the
        /// removal looks for.
        fn source_line(home: &Path) -> String {
            format!(". \"{}\"", env_file(home).display())
        }

        const MARK: &str = "# added by the meadow installer";

        /// Write the file the profiles source.
        ///
        /// It guards against adding the directory twice, so re-sourcing it --
        /// which a shell does on every new terminal -- does not grow `PATH`.
        fn write_env(home: &Path) -> Result<(), String> {
            let bin = bin_dir(home);
            let text = format!(
                "#!/bin/sh\n\
                 # Adds meadow to PATH. Sourced from your shell profile.\n\
                 case \":${{PATH}}:\" in\n\
                 \x20   *:\"{bin}\":*) ;;\n\
                 \x20   *) export PATH=\"{bin}:$PATH\" ;;\n\
                 esac\n",
                bin = bin.display()
            );
            let file = env_file(home);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("could not make {}: {e}", parent.display()))?;
            }
            std::fs::write(&file, text)
                .map_err(|e| format!("could not write {}: {e}", file.display()))
        }

        pub fn add(dir: &Path) -> Result<bool, String> {
            add_for(dir, &profiles())
        }

        /// [`add`], against a given set of profiles -- which is how it is
        /// tested without writing into the profiles of whoever is running the
        /// tests.
        pub(crate) fn add_for(dir: &Path, profiles: &[std::path::PathBuf]) -> Result<bool, String> {
            // `dir` is the bin directory; the env file sits beside it.
            let home = dir.parent().unwrap_or(dir);
            write_env(home)?;
            let line = source_line(home);
            let marker = env_file(home).display().to_string();
            let mut touched = false;
            for profile in profiles {
                if !profile.is_file() {
                    continue;
                }
                let had = std::fs::read_to_string(profile).unwrap_or_default();
                if had.contains(&marker) {
                    continue;
                }
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(profile)
                    .map_err(|e| format!("could not open {}: {e}", profile.display()))?;
                writeln!(f, "\n{MARK}\n{line}")
                    .map_err(|e| format!("could not write {}: {e}", profile.display()))?;
                touched = true;
            }
            // No profile at all: make the one every shell reads, so a new
            // terminal still finds meadow.
            if !touched
                && let Some(first) = profiles.first()
                && !first.exists()
            {
                std::fs::write(first, format!("{MARK}\n{line}\n"))
                    .map_err(|e| format!("could not write a profile: {e}"))?;
                touched = true;
            }
            Ok(touched)
        }

        pub fn remove(dir: &Path) -> Result<bool, String> {
            remove_for(dir, &profiles())
        }

        pub(crate) fn remove_for(
            dir: &Path,
            profiles: &[std::path::PathBuf],
        ) -> Result<bool, String> {
            let home = dir.parent().unwrap_or(dir);
            let marker = env_file(home).display().to_string();
            let mut touched = false;
            for profile in profiles {
                let Ok(had) = std::fs::read_to_string(profile) else {
                    continue;
                };
                if !had.contains(&marker) {
                    continue;
                }
                let kept: Vec<&str> = had
                    .lines()
                    .filter(|l| !l.contains(&marker) && l.trim() != MARK)
                    .collect();
                // Trailing blank lines would pile up over install and uninstall
                // cycles, since the line added ahead of ours was blank.
                let mut text = kept.join("\n");
                while text.ends_with("\n\n") {
                    text.pop();
                }
                let text = format!("{}\n", text.trim_end());
                std::fs::write(profile, text)
                    .map_err(|e| format!("could not write {}: {e}", profile.display()))?;
                touched = true;
            }
            let _ = std::fs::remove_file(env_file(home));
            Ok(touched)
        }
    }

    pub use imp::{add, remove};
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    #[test]
    fn a_home_follows_stable_until_it_is_told_otherwise() {
        let home = scratch_dir("meadowup-channel").unwrap();
        assert_eq!(channel(&home), Channel::Stable);
        set_channel(&home, Channel::Nightly).unwrap();
        assert_eq!(channel(&home), Channel::Nightly);
        set_channel(&home, Channel::Stable).unwrap();
        assert_eq!(channel(&home), Channel::Stable);
    }

    #[test]
    fn a_version_is_its_tag_with_or_without_the_v() {
        assert_eq!(tag_for("0.2.0"), "v0.2.0");
        assert_eq!(tag_for("v0.2.0"), "v0.2.0");
        assert_eq!(tag_for("nightly"), "nightly");
    }

    #[test]
    fn what_is_recorded_is_read_back() {
        let home = scratch_dir("meadowup-prov").unwrap();
        assert_eq!(provenance(&home), None, "nothing recorded yet");

        record(&home, &Provenance::Release("v0.2.0".into())).unwrap();
        assert_eq!(
            provenance(&home),
            Some(Provenance::Release("v0.2.0".into()))
        );

        record(&home, &Provenance::Local).unwrap();
        assert_eq!(provenance(&home), Some(Provenance::Local));
    }
}

#[cfg(all(test, not(windows)))]
mod unix_path_tests {
    use super::path::imp::{add_for, profiles_in, remove_for};
    use super::{Scratch, bin_dir, env_file, scratch_dir};

    /// A pretend home directory, with the profiles that exist in it named.
    fn home(which: &str, existing: &[&str]) -> Scratch {
        let dir = scratch_dir(&format!("meadowup-path-{which}")).unwrap();
        for name in existing {
            std::fs::write(dir.join(name), "# something that was already here\n").unwrap();
        }
        dir
    }

    #[test]
    fn the_env_file_is_written_and_sourced_from_every_profile_there_is() {
        let shell_home = home("add", &[".profile", ".zshenv"]);
        let meadow_home = shell_home.join(".meadow");
        let bin = bin_dir(&meadow_home);
        let profiles = profiles_in(&shell_home);

        assert!(add_for(&bin, &profiles).unwrap(), "nothing was added");

        // The file itself, guarding against adding the directory twice.
        let env = std::fs::read_to_string(env_file(&meadow_home)).unwrap();
        assert!(env.contains(&bin.display().to_string()), "{env}");
        assert!(
            env.contains("case"),
            "no guard against a repeated PATH: {env}"
        );

        // And a line in each profile that exists, but not in ones that do not.
        for name in [".profile", ".zshenv"] {
            let text = std::fs::read_to_string(shell_home.join(name)).unwrap();
            assert!(text.contains("# something that was already here"), "{name}");
            assert!(
                text.contains(&env_file(&meadow_home).display().to_string()),
                "{name}"
            );
        }
        assert!(
            !shell_home.join(".bashrc").exists(),
            "made a profile that did not exist"
        );
    }

    #[test]
    fn adding_twice_does_not_write_the_line_twice() {
        let shell_home = home("twice", &[".profile"]);
        let bin = bin_dir(&shell_home.join(".meadow"));
        let profiles = profiles_in(&shell_home);

        assert!(add_for(&bin, &profiles).unwrap());
        assert!(
            !add_for(&bin, &profiles).unwrap(),
            "said it added something again"
        );

        let text = std::fs::read_to_string(shell_home.join(".profile")).unwrap();
        let marker = env_file(&shell_home.join(".meadow")).display().to_string();
        assert_eq!(text.matches(&marker).count(), 1, "{text}");
    }

    #[test]
    fn with_no_profile_at_all_one_is_made() {
        // Otherwise a new shell would never find meadow.
        let shell_home = home("none", &[]);
        let bin = bin_dir(&shell_home.join(".meadow"));
        let profiles = profiles_in(&shell_home);

        assert!(add_for(&bin, &profiles).unwrap());
        assert!(shell_home.join(".profile").is_file(), "no profile was made");
    }

    #[test]
    fn removing_leaves_the_profile_as_it_was() {
        let shell_home = home("remove", &[".profile", ".bashrc"]);
        let meadow_home = shell_home.join(".meadow");
        let bin = bin_dir(&meadow_home);
        let profiles = profiles_in(&shell_home);
        let before = std::fs::read_to_string(shell_home.join(".profile")).unwrap();

        add_for(&bin, &profiles).unwrap();
        assert!(remove_for(&bin, &profiles).unwrap(), "nothing was removed");

        let after = std::fs::read_to_string(shell_home.join(".profile")).unwrap();
        assert_eq!(
            after, before,
            "the profile did not come back to what it was"
        );
        assert!(
            !env_file(&meadow_home).exists(),
            "the env file was left behind"
        );
    }

    #[test]
    fn install_and_uninstall_cycles_do_not_pile_up_blank_lines() {
        let shell_home = home("cycles", &[".profile"]);
        let bin = bin_dir(&shell_home.join(".meadow"));
        let profiles = profiles_in(&shell_home);
        let before = std::fs::read_to_string(shell_home.join(".profile")).unwrap();

        for _ in 0..3 {
            add_for(&bin, &profiles).unwrap();
            remove_for(&bin, &profiles).unwrap();
        }
        let after = std::fs::read_to_string(shell_home.join(".profile")).unwrap();
        assert_eq!(after, before, "the profile grew over repeated cycles");
    }

    #[test]
    fn removing_what_was_never_added_changes_nothing() {
        let shell_home = home("noop", &[".profile"]);
        let bin = bin_dir(&shell_home.join(".meadow"));
        let profiles = profiles_in(&shell_home);
        let before = std::fs::read_to_string(shell_home.join(".profile")).unwrap();

        assert!(!remove_for(&bin, &profiles).unwrap());
        assert_eq!(
            std::fs::read_to_string(shell_home.join(".profile")).unwrap(),
            before
        );
    }
}

#[cfg(all(test, windows))]
mod windows_path_tests {
    use super::path::imp::{entries, eq_path};

    #[test]
    fn path_entries_skip_empties() {
        let got: Vec<&str> = entries(r"C:\a;;C:\b;").collect();
        assert_eq!(got, vec![r"C:\a", r"C:\b"]);
    }

    #[test]
    fn path_comparison_ignores_case_and_trailing_slash() {
        assert!(eq_path(
            r"C:\Users\me\.meadow\bin",
            r"c:\users\me\.meadow\bin"
        ));
        assert!(eq_path(r"C:\a\bin\", r"C:\a\bin"));
        assert!(eq_path(r"C:\a\bin/", r"C:\a\bin"));
        assert!(!eq_path(r"C:\a\bin", r"C:\a\binary"));
    }

    #[test]
    fn unexpanded_variables_are_left_alone() {
        // The whole point of reading the registry raw: an entry like this must
        // survive an add/remove round trip untouched.
        let path = r"%JAVA_HOME%\bin;C:\Windows";
        let kept: Vec<&str> = entries(path)
            .filter(|e| !eq_path(e, r"C:\Windows"))
            .collect();
        assert_eq!(kept, vec![r"%JAVA_HOME%\bin"]);
    }
}
