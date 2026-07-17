#![forbid(unsafe_code)]

//! Minimal in-container healthcheck binary for the Docker image.
//!
//! It intentionally uses only the Rust standard library so the runtime image does
//! not need `curl`, `wget`, a shell, or package-manager state. The default target
//! is the server's loopback health endpoint inside the same container.

use std::env;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const DEFAULT_URL: &str = "http://127.0.0.1:3000/healthz";
const TIMEOUT: Duration = Duration::from_secs(2);

fn main() {
    let url = env::var("ARDUR_HEALTHCHECK_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    match check(&url) {
        Ok(()) => {}
        Err(err) => {
            eprintln!("ardur healthcheck failed: {err}");
            std::process::exit(1);
        }
    }
}

fn check(url: &str) -> Result<(), String> {
    let target = parse_http_loopback_url(url)?;
    let addr = format!("{}:{}", target.host, target.port);
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|e| format!("resolving {addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("no socket address resolved for {addr}"))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, TIMEOUT)
        .map_err(|e| format!("connecting to {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|e| format!("setting read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(|e| format!("setting write timeout: {e}"))?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
        target.path, target.host, target.port
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("writing request: {e}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("reading response: {e}"))?;
    if response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        Err(format!(
            "unexpected status line {:?}",
            response.lines().next().unwrap_or("<empty>")
        ))
    }
}

#[derive(Debug)]
struct Target {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_loopback_url(url: &str) -> Result<Target, String> {
    let Some(rest) = url.strip_prefix("http://") else {
        return Err("healthcheck URL must use http:// loopback".to_string());
    };
    let (host_port, path) = match rest.split_once('/') {
        Some((host_port, path)) => (host_port, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .map_err(|e| format!("invalid port {port:?}: {e}"))?;
            (host.to_string(), port)
        }
        None => (host_port.to_string(), 80),
    };
    if !matches!(host.as_str(), "127.0.0.1" | "localhost") {
        return Err(format!(
            "healthcheck URL must target loopback, got host {host:?}"
        ));
    }
    Ok(Target { host, port, path })
}

#[cfg(test)]
mod tests {
    use super::parse_http_loopback_url;

    #[test]
    fn parses_default_loopback_url() {
        let target = parse_http_loopback_url("http://127.0.0.1:3000/healthz").unwrap();
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, 3000);
        assert_eq!(target.path, "/healthz");
    }

    #[test]
    fn rejects_non_loopback_url() {
        let err = parse_http_loopback_url("http://example.com:3000/healthz").unwrap_err();
        assert!(err.contains("loopback"));
    }
}
