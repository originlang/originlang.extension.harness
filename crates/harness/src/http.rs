//! Minimal HTTPS POST built on the system `curl`.
//!
//! Why not a Rust TLS stack: the harness must stay dependency-light and must
//! build without a network toolchain on Windows and Linux alike, which is the
//! same reason the sibling `bootidea-agent` shells out to `curl`. The cost is a
//! documented runtime dependency on `curl` and the temporary config file
//! described below.
//!
//! Secret handling: the bearer token never appears in `argv`, because `argv` is
//! readable by other users on the same machine. It is written to a `0600`
//! temporary config file that `curl` reads and that is deleted before the call
//! returns. The body (the conversation) travels over stdin and is never written
//! to disk by this module.

use crate::error::{Error, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Sends `body` to `url` and returns `(status, response_body)`.
///
/// `headers` are sent verbatim; the `Content-Type` header is added by the
/// caller so this function stays a transport and nothing more.
pub fn post_json(
    url: &str,
    headers: &[(&str, String)],
    body: &[u8],
    timeout: Duration,
) -> Result<(u16, String)> {
    let mut config = format!("url = \"{}\"\nrequest = \"POST\"\n", escape(url));
    config.push_str("header = \"Content-Type: application/json\"\n");
    for (name, value) in headers {
        config.push_str(&format!(
            "header = \"{}: {}\"\n",
            escape(name),
            escape(value)
        ));
    }
    config.push_str(&format!("max-time = \"{}\"\n", timeout.as_secs().max(1)));

    let secret = SecretFile::create("bootidea-curl", &config)?;

    let mut child = Command::new("curl")
        .arg("--silent")
        .arg("--show-error")
        .arg("--config")
        .arg(secret.path())
        .arg("--data-binary")
        .arg("@-")
        .arg("--write-out")
        .arg("\n%{http_code}")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            Error::Config(format!(
                "cannot run `curl` ({error}); the harness uses the system curl for HTTPS"
            ))
        })?;

    // Closing stdin is what tells curl the body is complete. A broken pipe means
    // curl already exited; the status check below explains why, so a write error
    // is only surfaced when curl itself succeeded.
    let write_result = match child.stdin.take() {
        Some(mut stdin) => stdin
            .write_all(body)
            .and_then(|()| stdin.flush())
            .map_err(Error::Io),
        None => Err(Error::Config("curl has no stdin pipe".to_owned())),
    };

    let output = child.wait_with_output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            format!("curl exited with {}", output.status)
        } else {
            detail.to_owned()
        };
        return Err(Error::Model(format!("request to {url} failed: {detail}")));
    }

    write_result?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (body, status) = stdout
        .rsplit_once('\n')
        .ok_or_else(|| Error::Model("curl did not report a status code".to_owned()))?;
    let status = status
        .trim()
        .parse::<u16>()
        .map_err(|_| Error::Model(format!("curl reported an unparsable status: {status:?}")))?;
    Ok((status, body.to_owned()))
}

/// Escapes a value for a curl config file.
///
/// curl reads `\` and `"` as escapes inside quoted values, so both must be
/// doubled; everything else is literal.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A temporary file that holds the bearer token and is removed on drop.
struct SecretFile {
    path: PathBuf,
}

impl SecretFile {
    fn create(prefix: &str, contents: &str) -> Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path =
            std::env::temp_dir().join(format!("{prefix}-{}-{stamp}.cfg", std::process::id()));

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        Ok(Self { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(escape("a\"b"), "a\\\"b");
        assert_eq!(escape("C:\\tmp"), "C:\\\\tmp");
    }

    #[test]
    fn secret_file_is_removed_on_drop() {
        let path = {
            let file = SecretFile::create("bootidea-test", "url = \"x\"\n").expect("create");
            let path = file.path().to_path_buf();
            assert!(path.exists());
            path
        };
        assert!(!path.exists());
    }
}
