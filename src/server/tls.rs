//! TLS listener for `ransible serve --tls-cert/--tls-key` (rustls, ring provider).
//!
//! Handshakes run in their own tasks with a timeout so a slow client cannot stall accepting.
use anyhow::{Context, Result, anyhow};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::{TlsAcceptor, server::TlsStream};

pub struct TlsListener {
    rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    addr: SocketAddr,
}

pub fn load_config(cert: &Path, key: &Path) -> Result<rustls::ServerConfig> {
    let cert_pem = std::fs::read(cert)
        .with_context(|| format!("reading TLS certificate {}", cert.display()))?;
    let key_pem =
        std::fs::read(key).with_context(|| format!("reading TLS key {}", key.display()))?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow!("invalid certificate PEM: {e}"))?;
    if certs.is_empty() {
        return Err(anyhow!("no certificate found in {}", cert.display()));
    }
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| anyhow!("invalid private key PEM: {e}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow!("certificate/key mismatch: {e}"))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

pub async fn bind(listen: &str, cert: &Path, key: &Path) -> Result<TlsListener> {
    let acceptor = TlsAcceptor::from(Arc::new(load_config(cert, key)?));
    let tcp = TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let addr = tcp.local_addr()?;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = tcp.accept().await else {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            };
            let (acceptor, tx) = (acceptor.clone(), tx.clone());
            tokio::spawn(async move {
                if let Ok(Ok(tls)) = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    acceptor.accept(stream),
                )
                .await
                {
                    let _ = tx.send((tls, peer)).await;
                }
            });
        }
    });
    Ok(TlsListener { rx, addr })
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // the sender lives as long as the accept task, i.e. the process
        self.rx.recv().await.expect("TLS accept task ended")
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.addr)
    }
}
