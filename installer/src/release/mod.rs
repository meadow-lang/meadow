//! Finding and fetching a published release.
//!
//! Through `curl` and `tar` rather than an HTTP library: those ship with macOS,
//! essentially every Linux, and Windows 10 1803 and later, and a program whose
//! only job is to install another one should not carry a TLS stack to do it.

use super::ui::Progress;
use super::{REPO, archive_ext, scratch_dir, valid_tag};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// A request to GitHub's API for `url`.
///
/// With the token in `GITHUB_TOKEN`, where it is set: a CI job's unnamed
/// requests share its runner's address's rate limit, which a busy provider's
/// runners use up, and then GitHub answers 403.
fn api(url: &str) -> Command {
    let mut curl = Command::new(curl());
    curl.args(["-fsSL", "-H", "Accept: application/vnd.github+json"]);
    if let Some(token) = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()) {
        curl.arg("-H").arg(format!("Authorization: Bearer {token}"));
    }
    curl.arg(url);
    curl
}

/// The tag of the newest release.
pub fn latest_tag() -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let out = api(&url)
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "could not ask GitHub for the latest release: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let body = String::from_utf8_lossy(&out.stdout);
    field(&body, "tag_name").ok_or_else(|| "GitHub named no release".to_string())
}

/// What the release published under `tag` is called: for the nightly one,
/// `nightly 2026-10-05 892808f` -- the day and the commit it was built from,
/// which is how one nightly is told from the next.
pub fn release_name(tag: &str) -> Result<String, String> {
    if !valid_tag(tag) {
        return Err(format!("`{tag}` is not a release tag"));
    }
    let url = format!("https://api.github.com/repos/{REPO}/releases/tags/{tag}");
    let out = api(&url)
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "could not ask GitHub for the {tag} release: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let body = String::from_utf8_lossy(&out.stdout);
    field(&body, "name").ok_or_else(|| format!("GitHub named no {tag} release"))
}

/// Download and unpack the release `tag` for `target`, answering the directory
/// it was unpacked into and how many bytes were fetched.
///
/// The download is drawn as a progress bar. `curl` does the fetching and this
/// watches the file grow -- which is all a bar needs, and keeps an HTTP library
/// out of a program whose only job is installing another one.
pub fn fetch(tag: &str, target: &str, into: &str) -> Result<(PathBuf, u64), String> {
    if !valid_tag(tag) {
        return Err(format!("`{tag}` is not a release tag"));
    }
    let ext = archive_ext();
    // As `release.yml` names them: the tag is in the URL, not in the file.
    let name = format!("meadow-{target}.{ext}");
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{name}");
    let dir = scratch_dir(into)?;
    let archive = dir.join(&name);

    let total = content_length(&url);
    let mut child = Command::new(curl())
        .args(["-fsSL", "-o"])
        .arg(&archive)
        .arg(&url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run curl: {e}"))?;

    let mut bar = Progress::new(total);
    let grown = || std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                bar.update(grown());
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                bar.abandon();
                return Err(format!("could not wait for curl: {e}"));
            }
        }
    };
    if !status.success() {
        bar.abandon();
        let mut said = String::new();
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_string(&mut said);
        }
        return Err(format!(
            "no build of {tag} for {target}.\n\
             Looked for {name}{}",
            match said.trim() {
                "" => String::new(),
                s => format!(" ({s})"),
            }
        ));
    }
    let fetched = grown();
    bar.finish(fetched);
    unpack(&archive, &dir)?;
    Ok((dir, fetched))
}

/// How long the file at `url` is, when the server says.
///
/// A release asset redirects to storage elsewhere, so the length that counts is
/// the last response's -- the first is the redirect's own, which is nothing.
fn content_length(url: &str) -> Option<u64> {
    let out = Command::new(curl())
        .args(["-sIL", url])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    lengths(&String::from_utf8_lossy(&out.stdout))
}

/// The last non-zero `Content-Length` among a run of response headers.
fn lengths(headers: &str) -> Option<u64> {
    headers
        .lines()
        .rev()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<u64>().ok())
                .flatten()
        })
        .find(|&n| n > 0)
}

/// Unpack `archive` into `dir`.
fn unpack(archive: &Path, dir: &Path) -> Result<(), String> {
    // `tar` reads zip files on Windows, where it is bsdtar.
    let out = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dir)
        .output()
        .map_err(|e| format!("could not run tar: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "could not unpack {}: {}",
            archive.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// The file called `name` somewhere under `dir`.
///
/// A release archive may hold the binaries at its root or inside one directory
/// named after the release; looking rather than assuming survives either.
pub fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir()
            && let Some(found) = find(&path, name)
        {
            return Some(found);
        }
    }
    None
}

/// `curl` on this system.
fn curl() -> &'static str {
    if cfg!(windows) { "curl.exe" } else { "curl" }
}

/// One string field of a flat JSON object.
///
/// Enough for the one field wanted out of GitHub's release JSON, and small
/// enough not to want a parser: a release payload has no nested `tag_name`.
fn field(json: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\"");
    let at = json.find(&key)? + key.len();
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start();
    let body = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            c => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_read_out_of_the_release_json() {
        let json = r#"{"url":"x","tag_name":"v0.2.0","name":"Meadow"}"#;
        assert_eq!(field(json, "tag_name").as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn an_escape_in_a_field_is_read_rather_than_ending_it() {
        let json = r#"{"name":"a \"quoted\" thing","tag_name":"v1"}"#;
        assert_eq!(field(json, "name").as_deref(), Some("a \"quoted\" thing"));
        assert_eq!(field(json, "tag_name").as_deref(), Some("v1"));
    }

    #[test]
    fn the_length_that_counts_is_the_one_after_the_redirects() {
        let headers = "HTTP/2 302\r\nlocation: https://objects.example/x\r\ncontent-length: 0\r\n\r\n\
                       HTTP/2 200\r\nContent-Length: 13043907\r\ncontent-type: application/octet-stream\r\n";
        assert_eq!(lengths(headers), Some(13_043_907));
    }

    #[test]
    fn no_length_is_none_rather_than_zero() {
        assert_eq!(
            lengths("HTTP/2 200\r\ntransfer-encoding: chunked\r\n"),
            None
        );
    }

    #[test]
    fn a_field_that_is_not_there_is_none() {
        assert_eq!(field(r#"{"a":"b"}"#, "tag_name"), None);
    }

    #[test]
    fn a_tag_that_could_reach_elsewhere_is_refused() {
        assert!(valid_tag("v0.2.0"));
        assert!(valid_tag("v1.2.3"));
        assert!(!valid_tag("../../etc/passwd"));
        assert!(!valid_tag("v1/../v2"));
        assert!(!valid_tag(""));
        assert!(!valid_tag("v1 v2"));
    }
}
