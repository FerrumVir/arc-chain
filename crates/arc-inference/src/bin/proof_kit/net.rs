//! Network access, through the system `curl` (built into macOS and into
//! Windows 10 and later; one package away on Linux). Three things only:
//! resumable Hugging Face downloads, `GET <endpoint>/challenge`, and, after
//! the person typed yes, `POST <endpoint>` with the result file.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use arc_inference::modern::ModernError;
use arc_inference::modern::convert;
use arc_inference::modern::proof::{self, Challenge};
use serde_json::Value;

const ATTEMPTS: u64 = 6;
/// curl's exit code when a server ignores the resume range.
const CURL_CANNOT_RESUME: i32 = 33;

fn invalid(message: impl Into<String>) -> ModernError {
    ModernError::Invalid(message.into())
}

fn io_error(path: &Path, error: std::io::Error) -> ModernError {
    ModernError::Io(format!("{}: {error}", path.display()))
}

fn curl() -> Command {
    let mut command = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    command
        .arg("--user-agent")
        .arg(format!("arc-proof-kit/{}", proof::KIT_VERSION))
        .stdin(Stdio::null());
    command
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Whether curl can be run at all.
pub(super) fn curl_available() -> bool {
    curl()
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Bytes moved and seconds spent by one download.
#[derive(Debug, Clone, Copy)]
pub(super) struct Transfer {
    pub bytes: u64,
    pub seconds: f64,
}

/// Download `url` to `dest` through `dest.part`, resuming what an earlier
/// run left, then require the pinned length and SHA-256 before renaming.
pub(super) fn download(
    url: &str,
    dest: &Path,
    bytes: u64,
    sha256: &str,
) -> Result<Transfer, ModernError> {
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("download target has no file name"))?;
    let part = dest.with_file_name(format!("{name}.part"));
    let mut transfer = Transfer {
        bytes: 0,
        seconds: 0.0,
    };
    for attempt in 1..=ATTEMPTS {
        let mut before = file_len(&part);
        if before > bytes {
            std::fs::remove_file(&part).map_err(|e| io_error(&part, e))?;
            before = 0;
        }
        if before == bytes {
            break;
        }
        if before > 0 {
            eprintln!("  resuming at {:.1} GB", before as f64 / 1e9);
        }
        let started = Instant::now();
        let status = curl()
            .args([
                "--fail",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--tlsv1.2",
                "--connect-timeout",
                "30",
                "--retry",
                "3",
                "--retry-delay",
                "5",
                "--continue-at",
                "-",
                "--progress-bar",
                "--output",
            ])
            .arg(&part)
            .arg(url)
            .status()
            .map_err(|e| ModernError::Io(format!("could not run curl: {e}")))?;
        transfer.seconds += started.elapsed().as_secs_f64();
        transfer.bytes += file_len(&part).saturating_sub(before);
        if file_len(&part) == bytes {
            break;
        }
        if status.code() == Some(CURL_CANNOT_RESUME) {
            // The server would not resume; start this file again.
            let _ = std::fs::remove_file(&part);
        }
        if attempt == ATTEMPTS {
            return Err(ModernError::Io(format!(
                "could not download {url} ({status}); run the kit again to resume"
            )));
        }
        eprintln!(
            "  the download stopped ({status}); trying again in {} s",
            5 * attempt
        );
        std::thread::sleep(Duration::from_secs(5 * attempt));
    }
    let (length, digest) = convert::sha256_file(&part)?;
    if length != bytes || digest != sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(invalid(format!(
            "{name}: received {length} bytes with SHA-256 {digest}; the pinned file is \
             {bytes} bytes with SHA-256 {sha256}. The partial file was removed; run again."
        )));
    }
    std::fs::rename(&part, dest).map_err(|e| io_error(dest, e))?;
    Ok(transfer)
}

/// Check a Hash Wall endpoint: https, or plain http on a loopback address
/// (local tests). No credentials, query or fragment.
pub(super) fn endpoint(raw: &str) -> Result<String, ModernError> {
    let url = raw.trim().trim_end_matches('/');
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"];
    let scheme_ok = url.starts_with("https://")
        || loopback.iter().any(|&base| {
            url.strip_prefix(base).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with(':') || rest.starts_with('/')
            })
        });
    let text_ok = url.len() <= 200
        && url.bytes().all(|b| b.is_ascii_graphic())
        && !url.contains(['@', '?', '#', '\\']);
    if scheme_ok && text_ok && url.len() > "https://".len() {
        Ok(url.to_string())
    } else {
        Err(invalid(format!(
            "{raw:?} is not a usable Hash Wall endpoint (https://..., or http://127.0.0.1:PORT for a local test)"
        )))
    }
}

fn protocol(endpoint: &str) -> [&'static str; 2] {
    if endpoint.starts_with("https://") {
        ["--proto", "=https"]
    } else {
        ["--proto", "=http"]
    }
}

fn printable(bytes: &[u8], limit: usize) -> String {
    let text: String = String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    text.chars().take(limit).collect()
}

/// `GET <endpoint>/challenge`. The request carries nothing from this
/// computer beyond what any web request does.
pub(super) fn fetch_challenge(endpoint: &str) -> Result<Challenge, ModernError> {
    let url = format!("{endpoint}/challenge");
    let out = curl()
        .args(["--silent", "--show-error", "--fail", "--max-time", "30"])
        .args([
            "--max-filesize",
            "65536",
            "--header",
            "Accept: application/json",
        ])
        .args(protocol(endpoint))
        .arg(&url)
        .output()
        .map_err(|e| ModernError::Io(format!("could not run curl: {e}")))?;
    if !out.status.success() {
        return Err(ModernError::Io(format!(
            "GET {url} failed: {}",
            printable(&out.stderr, 300)
        )));
    }
    let value: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| invalid(format!("the challenge from {url} is not JSON: {e}")))?;
    let challenge = Challenge::from_json(&value)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    if challenge.expires_unix().is_none_or(|at| at <= now) {
        return Err(invalid(format!(
            "the challenge expires at {}, which is not in the future by this computer's \
             clock; check the clock and try again",
            challenge.expires_at
        )));
    }
    Ok(challenge)
}

/// What the Hash Wall answered.
#[derive(Debug)]
pub(super) struct Response {
    pub status: u16,
    pub body: String,
}

/// `POST <endpoint>` with exactly the bytes of `body_path`.
pub(super) fn submit(endpoint: &str, body_path: &Path) -> Result<Response, ModernError> {
    let mut data = std::ffi::OsString::from("@");
    data.push(body_path.as_os_str());
    let out = curl()
        .args(["--silent", "--show-error", "--max-time", "60"])
        .args(["--max-filesize", "65536"])
        .args(["--header", "Content-Type: application/json"])
        .args(["--header", "Accept: application/json"])
        .args(["--write-out", "\n%{http_code}"])
        .args(protocol(endpoint))
        .arg("--data-binary")
        .arg(&data)
        .arg(endpoint)
        .output()
        .map_err(|e| ModernError::Io(format!("could not run curl: {e}")))?;
    if !out.status.success() {
        return Err(ModernError::Io(format!(
            "POST {endpoint} failed: {}",
            printable(&out.stderr, 300)
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
    let status = code
        .trim()
        .parse()
        .map_err(|_| invalid(format!("POST {endpoint}: no HTTP status in the reply")))?;
    Ok(Response {
        status,
        body: printable(body.as_bytes(), 500),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_must_be_https_or_loopback() {
        assert_eq!(
            endpoint("https://example.org/api/hashwall/").unwrap(),
            "https://example.org/api/hashwall"
        );
        assert!(endpoint("http://127.0.0.1:8787").is_ok());
        assert!(endpoint("http://localhost:8787/api").is_ok());
        for bad in [
            "http://example.org/api",
            "http://127.0.0.1.example.org/api",
            "https://user:pass@example.org/api",
            "https://example.org/api?x=1",
            "https://exa mple.org",
            "ftp://example.org",
            "https://",
            "",
        ] {
            assert!(endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn replies_are_reduced_to_printable_text() {
        assert_eq!(printable(b"ok\n\x1b[31mred\t", 100), "ok [31mred");
        assert_eq!(printable(b"abcdef", 3), "abc");
    }
}
