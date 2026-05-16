use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;

/// Subdirectory under `ZUNEL_HOME` where users can drop a persistent OAuth
/// callback cert (e.g. produced by `mkcert -cert-file cert.pem -key-file
/// key.pem 127.0.0.1 localhost`). When both files are present the HTTPS
/// callback server uses them, eliminating the browser cert warning. When
/// either is missing the server falls back to a per-run, self-signed cert.
pub(crate) const OAUTH_CALLBACK_CERT_DIR: &str = "oauth-callback";
pub(crate) const OAUTH_CALLBACK_CERT_FILE: &str = "cert.pem";
pub(crate) const OAUTH_CALLBACK_KEY_FILE: &str = "key.pem";

/// A bound local OAuth callback server. The variant matches the redirect URI's
/// URL scheme: `http` for plain loopback (e.g. dynamically registered MCP
/// clients) and `https` for providers that reject `http://` (notably Slack).
pub(crate) enum CallbackServer {
    Http(HttpCallbackServer),
    Https(HttpsCallbackServer),
}

impl CallbackServer {
    pub(crate) async fn wait_for_callback(self) -> Result<String> {
        match self {
            CallbackServer::Http(server) => server.wait_for_callback().await,
            CallbackServer::Https(server) => server.wait_for_callback().await,
        }
    }
}

pub(crate) struct HttpCallbackServer {
    listener: tokio::net::TcpListener,
    origin: String,
    path: String,
}

impl HttpCallbackServer {
    async fn wait_for_callback(self) -> Result<String> {
        loop {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .context("accepting OAuth callback connection")?;
            match respond(stream, &self.origin, &self.path).await {
                Ok(Some(url)) => return Ok(url),
                Ok(None) => continue,
                Err(err) => {
                    eprintln!("oauth callback: ignoring transient request error: {err:#}");
                    continue;
                }
            }
        }
    }
}

pub(crate) struct HttpsCallbackServer {
    listener: tokio::net::TcpListener,
    acceptor: TlsAcceptor,
    origin: String,
    path: String,
}

impl HttpsCallbackServer {
    async fn wait_for_callback(self) -> Result<String> {
        loop {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .context("accepting OAuth callback connection")?;
            let tls_stream = match self.acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(err) => {
                    // Browsers commonly probe TLS or abort after the cert warning;
                    // keep listening so the user can click "Advanced → Proceed".
                    eprintln!("oauth callback: TLS handshake failed (expected before you accept the self-signed cert): {err}");
                    continue;
                }
            };
            match respond(tls_stream, &self.origin, &self.path).await {
                Ok(Some(url)) => return Ok(url),
                Ok(None) => continue,
                Err(err) => {
                    eprintln!("oauth callback: ignoring transient request error: {err:#}");
                    continue;
                }
            }
        }
    }
}

/// Read one HTTP request and respond.
///
/// Returns `Ok(Some(url))` when the request targets the configured callback
/// path AND carries an OAuth `code` query param — the only signal the login
/// flow is actually done. Returns `Ok(None)` for every other request (root,
/// favicon, browser preflights, missing `code`) so the caller can keep
/// listening on the same port. Network/IO errors bubble up as `Err`.
async fn respond<S>(mut stream: S, origin: &str, path: &str) -> Result<Option<String>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Read the request line + headers until we see CRLFCRLF (or hit the
    // 64 KiB cap). The earlier single `stream.read(&mut [0; 4096])`
    // worked when the OAuth provider returned a compact callback URL
    // and the kernel buffered the whole request, but it broke when (a)
    // the request straddled multiple TCP segments (slow client, TLS
    // handshake fragmentation) or (b) the IdP appended enough query
    // params to push the request past 4 KiB.
    const REQUEST_CAP: usize = 64 * 1024;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() >= REQUEST_CAP {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let is_callback = target.starts_with(path) && has_oauth_code(target);
    let status = if is_callback {
        "HTTP/1.1 200 OK"
    } else {
        "HTTP/1.1 404 Not Found"
    };
    let body = if is_callback {
        "zunel OAuth login complete. You can close this tab."
    } else {
        "zunel OAuth callback waiting for the authorization code."
    };
    let response = format!(
        "{status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    let _ = stream.shutdown().await;

    if is_callback {
        Ok(Some(format!("{origin}{target}")))
    } else {
        Ok(None)
    }
}

fn has_oauth_code(target: &str) -> bool {
    let Some(query) = target.split_once('?').map(|(_, q)| q) else {
        return false;
    };
    query
        .split('&')
        .any(|pair| matches!(pair.split_once('='), Some(("code", v)) if !v.is_empty()))
}

/// Bind a local listener for an `http://` or `https://` loopback redirect URI.
///
/// Returns `Ok(None)` if the URI is not a loopback URL the CLI can serve
/// (e.g. an external HTTPS URL like `https://slack.com/robots.txt`); callers
/// should fall back to the manual paste flow.
pub(crate) async fn bind_callback_server(redirect_uri: &str) -> Result<Option<CallbackServer>> {
    let url = reqwest::Url::parse(redirect_uri).context("parsing redirect URI")?;
    let host = url.host_str().context("redirect URI missing host")?;
    if !matches!(host, "127.0.0.1" | "localhost") {
        return Ok(None);
    }
    let port = url.port().context("redirect URI must include a port")?;
    let path = url.path().to_string();
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .with_context(|| format!("binding OAuth callback listener at {host}:{port}"))?;
    match url.scheme() {
        "http" => {
            let origin = format!("http://{host}:{port}");
            Ok(Some(CallbackServer::Http(HttpCallbackServer {
                listener,
                origin,
                path,
            })))
        }
        "https" => {
            let acceptor = build_tls_acceptor(host)
                .with_context(|| format!("preparing TLS for OAuth callback at {host}:{port}"))?;
            let origin = format!("https://{host}:{port}");
            Ok(Some(CallbackServer::Https(HttpsCallbackServer {
                listener,
                acceptor,
                origin,
                path,
            })))
        }
        // Compatibility note: the public `bind_callback_server` API stays the
        // same; persistent-cert discovery happens transparently inside
        // `build_tls_acceptor` via `zunel_config::zunel_home()`.
        _ => Ok(None),
    }
}

fn build_tls_acceptor(host: &str) -> Result<TlsAcceptor> {
    let (certs, key) = match persistent_cert_paths() {
        Some((cert_path, key_path)) if cert_path.is_file() && key_path.is_file() => {
            eprintln!(
                "oauth callback: using persistent TLS cert at {} (run `mkcert -install` once if your browser still warns)",
                cert_path.display()
            );
            load_pem_cert_and_key(&cert_path, &key_path)?
        }
        _ => {
            eprintln!(
                "oauth callback: generating ephemeral self-signed TLS cert (browser will warn; install mkcert + drop cert/key under {} to silence)",
                persistent_cert_dir_hint()
            );
            generate_self_signed(host)?
        }
    };
    build_acceptor_from(certs, key)
}

fn build_acceptor_from(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<TlsAcceptor> {
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building rustls server config")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn generate_self_signed(
    host: &str,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let subject_alt_names = vec![host.to_string()];
    let cert_key = rcgen::generate_simple_self_signed(subject_alt_names)
        .context("generating self-signed TLS certificate")?;
    let cert_der = CertificateDer::from(cert_key.cert.der().to_vec());
    let key_der = PrivateKeyDer::try_from(cert_key.key_pair.serialize_der())
        .map_err(|err| anyhow::anyhow!("converting self-signed key to DER: {err}"))?;
    Ok((vec![cert_der], key_der))
}

fn load_pem_cert_and_key(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_pem = std::fs::read(cert_path)
        .with_context(|| format!("reading OAuth callback cert at {}", cert_path.display()))?;
    let key_pem = std::fs::read(key_path)
        .with_context(|| format!("reading OAuth callback key at {}", key_path.display()))?;

    let mut cert_reader = std::io::BufReader::new(cert_pem.as_slice());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parsing OAuth callback cert at {}", cert_path.display()))?;
    if certs.is_empty() {
        anyhow::bail!(
            "no certificates found in {} (expected at least one PEM-encoded CERTIFICATE block)",
            cert_path.display()
        );
    }

    let mut key_reader = std::io::BufReader::new(key_pem.as_slice());
    let key = rustls_pemfile::private_key(&mut key_reader)
        .with_context(|| format!("parsing OAuth callback key at {}", key_path.display()))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no private key found in {} (expected a PEM-encoded PRIVATE KEY block)",
                key_path.display()
            )
        })?;

    Ok((certs, key))
}

fn persistent_cert_paths() -> Option<(PathBuf, PathBuf)> {
    let home = zunel_config::zunel_home().ok()?;
    let dir = home.join(OAUTH_CALLBACK_CERT_DIR);
    Some((
        dir.join(OAUTH_CALLBACK_CERT_FILE),
        dir.join(OAUTH_CALLBACK_KEY_FILE),
    ))
}

fn persistent_cert_dir_hint() -> String {
    persistent_cert_paths()
        .map(|(cert, _)| {
            cert.parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| cert.display().to_string())
        })
        .unwrap_or_else(|| "$ZUNEL_HOME/oauth-callback/".to_string())
}

pub(crate) fn open_browser(url: &str) {
    let command = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "cmd"
    } else {
        "xdg-open"
    };
    let mut process = std::process::Command::new(command);
    if cfg!(target_os = "windows") {
        process.args(["/C", "start", "", url]);
    } else {
        process.arg(url);
    }
    if process.spawn().is_err() {
        println!("Could not open a browser automatically; open the URL above manually.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_pem_cert_and_key_accepts_a_self_signed_pair() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        let cert_key = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
            .expect("generate test cert");
        std::fs::write(&cert_path, cert_key.cert.pem()).unwrap();
        std::fs::write(&key_path, cert_key.key_pair.serialize_pem()).unwrap();

        let (certs, _key) =
            load_pem_cert_and_key(&cert_path, &key_path).expect("load PEM cert+key");
        assert_eq!(certs.len(), 1, "expected exactly one CERTIFICATE block");
    }

    #[test]
    fn load_pem_cert_and_key_rejects_empty_cert_file() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, "").unwrap();
        std::fs::write(&key_path, "").unwrap();

        let err = load_pem_cert_and_key(&cert_path, &key_path).expect_err("empty cert should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no certificates found"),
            "unexpected error: {msg}"
        );
    }
}
