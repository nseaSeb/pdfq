//! Webhook delivery: HMAC-SHA256 signed POST, retries with backoff, and SSRF
//! protection (scheme, host allowlist, private address filtering at
//! validation time and again at connection time, no redirects).

use bytes::Bytes;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Client, Url};
use sha2::Sha256;

use crate::error::ApiError;

#[derive(Debug, Clone)]
pub struct WebhookConfig {
    /// HMAC key. Webhooks are refused when absent.
    pub secret: Option<String>,
    /// Allowed hosts (`example.com` or `*.example.com`). Empty: any public host.
    pub allowed_hosts: Vec<String>,
    /// Allow private, loopback and link-local addresses (tests, internal networks).
    pub allow_private: bool,
    /// Allow plain `http://` callbacks.
    pub allow_http: bool,
    /// Timeout of each delivery attempt.
    pub timeout: Duration,
    /// Retries after the first attempt.
    pub retries: u32,
    /// Delay before the first retry; multiplied by 4 for each next one.
    pub retry_delay: Duration,
}

/// What is sent to the callback URL.
pub struct Delivery {
    pub job_id: String,
    pub status: &'static str,
    pub content_type: &'static str,
    /// Shared with the job store: retries do not copy the PDF.
    pub body: Bytes,
}

/// Outcome of a delivery, recorded on the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub delivered: bool,
    pub attempts: u32,
}

/// Longest accepted `callback_url`: a sanity bound on a URL, not a tunable
/// resource limit (memory is bounded by the request size limits).
pub const MAX_URL_LEN: usize = 2048;

#[derive(Clone)]
pub struct Webhooks {
    config: Arc<WebhookConfig>,
    client: Client,
}

impl Webhooks {
    pub fn new(config: WebhookConfig) -> Self {
        let resolver = Arc::new(GuardedResolver {
            allow_private: config.allow_private,
        });
        let client = Client::builder()
            .dns_resolver(resolver)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(config.timeout)
            .connect_timeout(config.timeout)
            .user_agent(concat!("pdfq/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("valid HTTP client configuration");
        Self {
            config: Arc::new(config),
            client,
        }
    }

    /// Checks a callback URL when the job is submitted.
    pub fn validate(&self, raw: &str) -> Result<Url, ApiError> {
        if self.config.secret.is_none() {
            return Err(ApiError::invalid(
                "webhooks are disabled on this server (PDFQ_WEBHOOK_SECRET is not set)",
            ));
        }
        let invalid = |why: &str| ApiError::invalid(format!("invalid callback_url: {why}"));
        if raw.len() > MAX_URL_LEN {
            return Err(invalid("too long"));
        }
        let url = Url::parse(raw).map_err(|_| invalid("not a URL"))?;
        match url.scheme() {
            "https" => {}
            "http" if self.config.allow_http => {}
            _ => return Err(invalid("the scheme must be https")),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(invalid("credentials are not allowed in the URL"));
        }
        let host = url.host_str().ok_or_else(|| invalid("missing host"))?;
        if !host_allowed(host, &self.config.allowed_hosts) {
            return Err(invalid("host not in PDFQ_WEBHOOK_ALLOWED_HOSTS"));
        }
        if self.refused_literal(&url) {
            return Err(invalid("private, loopback or reserved address"));
        }
        Ok(url)
    }

    /// Delivers with retries. Never fails: the outcome is reported.
    pub async fn deliver(&self, url: Url, delivery: Delivery) -> Report {
        let Some(secret) = self.config.secret.as_deref() else {
            return Report {
                delivered: false,
                attempts: 0,
            };
        };
        let mut delay = self.config.retry_delay;
        let mut attempts = 0;
        loop {
            attempts += 1;
            match self.attempt(&url, &delivery, secret).await {
                Outcome::Delivered => {
                    return Report {
                        delivered: true,
                        attempts,
                    };
                }
                Outcome::Permanent => break,
                Outcome::Retry if attempts > self.config.retries => break,
                Outcome::Retry => {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(4);
                }
            }
        }
        tracing::warn!(job = %delivery.job_id, attempts, "webhook delivery failed");
        Report {
            delivered: false,
            attempts,
        }
    }

    /// IP literals bypass the resolver, so they are checked against the URL.
    fn refused_literal(&self, url: &Url) -> bool {
        ip_literal(url).is_some_and(|ip| !self.config.allow_private && !is_public(ip))
    }

    async fn attempt(&self, url: &Url, delivery: &Delivery, secret: &str) -> Outcome {
        // Re-checked at delivery time too (defense in depth).
        if self.refused_literal(url) {
            return Outcome::Permanent;
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let signature = sign(secret, timestamp, &delivery.body);
        let response = self
            .client
            .post(url.clone())
            .header("content-type", delivery.content_type)
            .header("x-pdfq-job-id", &delivery.job_id)
            .header("x-pdfq-status", delivery.status)
            .header("x-pdfq-timestamp", timestamp.to_string())
            .header("x-pdfq-signature", format!("sha256={signature}"))
            .body(delivery.body.clone())
            .send()
            .await;
        match response {
            Ok(r) if r.status().is_success() => Outcome::Delivered,
            Ok(r) if r.status().is_server_error() || r.status().as_u16() == 429 => Outcome::Retry,
            Ok(r) => {
                tracing::warn!(job = %delivery.job_id, status = %r.status(), "webhook refused");
                Outcome::Permanent
            }
            Err(err) => {
                tracing::debug!(job = %delivery.job_id, "webhook attempt failed: {err}");
                Outcome::Retry
            }
        }
    }
}

enum Outcome {
    Delivered,
    Retry,
    Permanent,
}

/// `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))`.
pub fn sign(secret: &str, timestamp: u64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn ip_literal(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        url::Host::Domain(_) => None,
    }
}

fn host_allowed(host: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allowed.iter().any(|pattern| {
        let pattern = pattern.trim().to_ascii_lowercase();
        match pattern.strip_prefix("*.") {
            Some(suffix) => host.ends_with(&format!(".{suffix}")),
            None => host == pattern,
        }
    })
}

/// Whether an address is publicly routable (not loopback, private, link-local,
/// shared, documentation, benchmarking, multicast or reserved).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // shared address space (CGNAT)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 192 && b == 88 && c == 99) // 6to4 relay anycast
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00 // unique local
        || (segments[0] & 0xffc0) == 0xfe80 // link-local
        || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        || (segments[0] == 0x0064 && segments[1] == 0xff9b) // NAT64, incl. local 64:ff9b:1::/48
        || segments[0] == 0x2002 // 6to4 (embeds IPv4)
        || (segments[0] == 0x2001 && segments[1] == 0) // Teredo
        || (segments[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
        || segments[..4] == [0x0100, 0, 0, 0] // discard
        || segments[..6] == [0, 0, 0, 0, 0, 0]) // IPv4-compatible (deprecated)
}

/// Resolves names and drops non-public addresses, so the connection can only
/// go to an address that was checked (no DNS rebinding between check and use).
struct GuardedResolver {
    allow_private: bool,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| allow_private || is_public(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("`{host}` does not resolve to a public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn webhooks(allowed_hosts: &[&str]) -> Webhooks {
        Webhooks::new(WebhookConfig {
            secret: Some("s3cret".into()),
            allowed_hosts: allowed_hosts.iter().map(|h| h.to_string()).collect(),
            allow_private: false,
            allow_http: false,
            timeout: Duration::from_secs(1),
            retries: 0,
            retry_delay: Duration::from_millis(10),
        })
    }

    #[test]
    fn classifies_addresses() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "198.18.0.1",
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",
            "64:ff9b:1::a9fe:a9fe",
            "2002:a9fe:a9fe::",
            "2001::1",
            "fec0::1",
            "100::1",
            "192.88.99.1",
            "2001:db8::1",
        ] {
            assert!(
                !is_public(private.parse().unwrap()),
                "{private} considered public"
            );
        }
        for public in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(
                is_public(public.parse().unwrap()),
                "{public} considered private"
            );
        }
    }

    #[test]
    fn validates_callback_urls() {
        let w = webhooks(&[]);
        assert!(w.validate("https://hooks.example.com/pdf").is_ok());
        for bad in [
            "http://hooks.example.com/pdf",
            "ftp://example.com",
            "https://user:pass@example.com",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "https://169.254.169.254/latest/meta-data",
            "https://10.0.0.1:8443/x",
            "not a url",
        ] {
            assert!(w.validate(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn enforces_the_host_allowlist() {
        let w = webhooks(&["hooks.example.com", "*.acme.io"]);
        assert!(w.validate("https://hooks.example.com/x").is_ok());
        assert!(w.validate("https://a.b.acme.io/x").is_ok());
        assert!(w.validate("https://acme.io/x").is_err());
        assert!(w.validate("https://evil.com/x").is_err());
        assert!(w.validate("https://hooks.example.com.evil.com/x").is_err());
    }

    #[test]
    fn refuses_webhooks_without_a_secret() {
        let w = Webhooks::new(WebhookConfig {
            secret: None,
            ..(*webhooks(&[]).config).clone()
        });
        assert!(w.validate("https://hooks.example.com/x").is_err());
    }

    #[test]
    fn signs_timestamp_and_body() {
        // Reference: printf '1700000000.hello' | openssl dgst -sha256 -hmac s3cret
        assert_eq!(
            sign("s3cret", 1_700_000_000, b"hello"),
            "babfa8b593140ee3160bea38210f8fea6712e1f6e073328eb52c4e965bc0ab8d"
        );
    }
}
