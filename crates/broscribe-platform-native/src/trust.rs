//! Native OS verification on macOS/Windows; exact anchor snapshots for explicit
//! certificate-environment overrides and platforms without an OS policy engine.

use broscribe_platform::{Error, Result, TlsTrust, TrustStore};

pub(crate) struct NativeTrustStore;

impl TrustStore for NativeTrustStore {
    fn tls_trust(&self) -> Result<TlsTrust> {
        // Presence, including an empty value, names an explicit override.
        let cert_file = std::env::var_os("SSL_CERT_FILE");
        let cert_dir = std::env::var_os("SSL_CERT_DIR");
        let loaded = rustls_native_certs::load_native_certs();
        select_trust(
            loaded.certs.into_iter(),
            loaded.errors.len(),
            cfg!(any(target_os = "macos", target_os = "windows")),
            cert_file.as_deref(),
            cert_dir.as_deref(),
        )
    }
}

/// The availability probe fails closed; readable roots survive partial errors.
/// A platform strategy avoids copying DERs, but the probe itself still loads them.
fn select_trust(
    certs: impl ExactSizeIterator<Item = impl AsRef<[u8]>>,
    errors: usize,
    platform_verifier: bool,
    cert_file: Option<&std::ffi::OsStr>,
    cert_dir: Option<&std::ffi::OsStr>,
) -> Result<TlsTrust> {
    let roots = certs.len();
    if roots == 0 {
        return Err(Error::Unsupported(format!(
            "no trusted root certificates could be read from this system ({errors} errors)"
        )));
    }
    if platform_verifier && cert_file.is_none() && cert_dir.is_none() {
        Ok(TlsTrust::Platform { roots })
    } else {
        Ok(TlsTrust::Certificates(
            certs.map(|cert| cert.as_ref().to_vec()).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_probe_fails_closed_and_readable_snapshots_survive_partial_errors() {
        for errors in [0, 3] {
            assert_eq!(
                select_trust(std::iter::empty::<&[u8]>(), errors, true, None, None)
                    .unwrap_err()
                    .code(),
                "unsupported"
            );
        }
        assert_eq!(
            select_trust([&[0x30][..]].into_iter(), 2, false, None, None).unwrap(),
            TlsTrust::Certificates(vec![vec![0x30]])
        );
    }

    #[test]
    fn certificate_environment_overrides_preserve_the_exact_snapshot() {
        use std::ffi::OsStr;
        for (file, dir, native) in [
            (None, None, true),
            (Some(OsStr::new("")), None, false),
            (None, Some(OsStr::new("")), false),
            (Some(OsStr::new("company.pem")), None, false),
            (None, Some(OsStr::new("company-roots")), false),
            (Some(OsStr::new("")), Some(OsStr::new("")), false),
        ] {
            let trust = select_trust([&[0x30, 0x01][..]].into_iter(), 0, true, file, dir).unwrap();
            let expected = if native {
                TlsTrust::Platform { roots: 1 }
            } else {
                TlsTrust::Certificates(vec![vec![0x30, 0x01]])
            };
            assert_eq!(trust, expected);
        }
    }
}
