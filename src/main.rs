//! Ada's fast HTTPS cache server
// TODO: naming and cache invalidation

mod cache;

use anyhow::anyhow;
use bytes::Bytes;
use docopt::Docopt;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::http::uri::Scheme;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use rustls::{Certificate, PrivateKey};
use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

use crate::cache::{CacheError, CacheResult, TlruCache};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &'static str = "
Usage: untitled-cache-server [options] --cert CERT --key PRIVATEKEY <backend-url>
       untitled-cache-server --help
       untitled-cache-server --version

Options:
    -m, --mem MEMORY        Soft memory limit. This might be temporarily exceeded.
    -p, --port PORT         What port to bind to.
    -a, --addr ADDRESS      What interface/ip address to bind to.
    -c, --cert CERT         Certificate path.
    -k, --key PRIVATEKEY    Private key path.
    --help                  Show help message.
";

#[derive(Debug, Deserialize)]
struct Args {
    arg_backend_url: String,
    flag_mem: Option<usize>,
    flag_port: Option<u16>,
    flag_addr: Option<IpAddr>,
    flag_cert: String,
    flag_key: String,
}

#[derive(Debug)]
struct FetchSettings {
    backend_url: String,
}

impl TryFrom<&Args> for FetchSettings {
    type Error = anyhow::Error;

    fn try_from(args: &Args) -> Result<Self, Self::Error> {
        let backend_uri = Uri::try_from(&args.arg_backend_url)?;

        if backend_uri.scheme() != Some(&Scheme::HTTP) {
            return Err(anyhow!(
                "backend should serve over HTTP, since HTTPS is terminated at the cache"
            ));
        }

        if backend_uri.query().is_some() {
            return Err(anyhow!(
                "backend URL should not contain query, only a base path is allowed"
            ));
        }

        if backend_uri.authority().is_none() {
            return Err(anyhow!("backend URL should not be relative URL"));
        }

        if !backend_uri.path().ends_with("/") {
            return Err(anyhow!("backend URL should end with a /"));
        }

        // here we can safely unwrap because of the guards above
        let mut backend_url = backend_uri.scheme_str().unwrap().to_owned();
        backend_url.push_str("://");
        backend_url.push_str(backend_uri.authority().unwrap().as_str());
        backend_url.push_str(backend_uri.path());

        Ok(FetchSettings { backend_url })
    }
}

#[tracing::instrument]
async fn fetch_content(resource: String, fetch_settings: Arc<FetchSettings>) -> CacheResult {
    let map_reqwest_error = |e: reqwest::Error| {
        let status_code = e
            .status()
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            .as_u16();

        CacheError::Fetch(Arc::new(e), status_code)
    };

    // we skip the first character of the resource path here, as that is always /
    let resource_uri = fetch_settings.backend_url.as_str().to_owned() + &resource[1..];

    let response = reqwest::get(resource_uri)
        .await
        .map_err(map_reqwest_error)?;

    let headers = Arc::new(response.headers().clone());

    let bytes = response.bytes().await.map_err(map_reqwest_error)?;

    // TODO: TTL from response or fetch_settings
    Ok((bytes, headers, Instant::now() + Duration::from_secs(30)))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 32)]
async fn main() -> Result<(), anyhow::Error> {
    console_subscriber::init();
    // env_logger::init();

    println!("Untitled Cache Server {}", VERSION);

    let args: Args = Docopt::new(USAGE)
        .map(|d| d.help(true))
        .map(|d| d.version(Some(VERSION.to_owned())))
        .and_then(|d| d.deserialize())
        .unwrap_or_else(|e| e.exit());

    let (certs, private_key) = load_certs_and_private_key(&args).await?;

    let tls_config: tokio_rustls::TlsAcceptor = Arc::new(
        rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(certs, private_key)?,
    )
    .into();

    let fetch_settings = Arc::new(FetchSettings::try_from(&args)?);

    let content_cache = Arc::new(TlruCache::new(args.flag_mem.unwrap_or(42_000_000)));

    let bind_address: SocketAddr = (
        args.flag_addr.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        args.flag_port.unwrap_or(443),
    )
        .into();

    println!(
        "Serving content from {} at {}",
        &fetch_settings.backend_url, bind_address
    );

    let listener = tokio::net::TcpListener::bind(bind_address).await?;

    let diagnostics_tick = tokio::time::sleep(tokio::time::Duration::from_secs(3));
    tokio::pin!(diagnostics_tick);

    loop {
        // TODO: use io_uring here
        // I'm currently on a windows machine so using it is a bit difficult, I'll try to get my
        // hands on a linux machine.

        tokio::select! {
            maybe_socket = listener.accept() => {
                let (socket, client_address) = maybe_socket?;
                let content_cache = Arc::clone(&content_cache);
                let fetch_settings = Arc::clone(&fetch_settings);
                let tls_config = tls_config.clone();

                tokio::task::Builder::new()
                    .name("handle connection")
                    .spawn(handle_socket_connection(socket, client_address, tls_config, content_cache, fetch_settings)).unwrap();
            },

            () = &mut diagnostics_tick => {
                let (entries, hits, misses, memory_used, memory_limit) = content_cache.diagnostics();
                println!("cache entries: {}  hit/miss: {}/{} memory use: {}/{}", entries, hits, misses, memory_used, memory_limit);

                diagnostics_tick.as_mut().reset(tokio::time::Instant::now() + tokio::time::Duration::from_secs(3));
            }
        }
    }
}

#[tracing::instrument(skip_all)]
async fn handle_socket_connection(
    socket: TcpStream,
    client_address: SocketAddr,
    tls_config: TlsAcceptor,
    content_cache: Arc<TlruCache>,
    fetch_settings: Arc<FetchSettings>,
) {
    match tls_config.accept(socket).await {
        Ok(connection) => {
            if let Err(e) = http1::Builder::new()
                .keep_alive(false)
                .serve_connection(
                    connection,
                    service_fn(move |req| {
                        let content_cache = Arc::clone(&content_cache);
                        let fetch_settings = Arc::clone(&fetch_settings);

                        handle_request(req, fetch_settings, content_cache, client_address.clone())
                    }),
                )
                .await
            {
                eprintln!("failed to serve {}: {}", client_address, e);
            }
        }
        Err(e) => {
            eprintln!("failed to establish TLS connection: {}", e);
        }
    }
}

/// Handle incoming HTTP request.
///
/// At this point the request has been parsed, but the body might still be streaming. We don't care
/// about the request body so this is fine.
#[tracing::instrument(skip(content_cache))]
async fn handle_request(
    req: Request<Incoming>,
    fetch_settings: Arc<FetchSettings>,
    content_cache: Arc<TlruCache>,
    client_address: SocketAddr,
) -> Result<Response<Full<Bytes>>, anyhow::Error> {
    // println!("{}: {}", client_address, req.uri().path());

    let path = req.uri().path().to_owned();

    let maybe_content = content_cache
        .get_or_fetch(req.uri().path(), move || {
            fetch_content(path, fetch_settings)
        })
        .await;

    let response = match maybe_content {
        Ok(content) => {
            let mut response = Response::new(Full::new(content.0));
            *response.headers_mut() = content.1.deref().clone();

            response
        }
        Err(CacheError::Fetch(_, code)) => {
            let mut response = Response::new(Full::new(Bytes::new()));
            *response.status_mut() = StatusCode::from_u16(code).unwrap();

            response
        }
        Err(_) => {
            let mut response = Response::new(Full::new(Bytes::new()));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;

            response
        }
    };

    Ok(response)
}

/// Load certificate and private key from specified files
///
/// This method is largely adapted from https://github.com/rustls/rustls/blob/main/examples/src/bin/tlsserver-mio.rs
async fn load_certs_and_private_key(args: &Args) -> anyhow::Result<(Vec<Certificate>, PrivateKey)> {
    let mut certfile = tokio::fs::File::open(&args.flag_cert).await?;
    let mut contents = Vec::new();
    certfile.read_to_end(&mut contents).await?;

    let certs = rustls_pemfile::certs(&mut &contents[..])?
        .into_iter()
        .map(|v| rustls::Certificate(v))
        .collect();

    contents.clear();
    let mut keyfile = tokio::fs::File::open(&args.flag_key).await?;
    keyfile.read_to_end(&mut contents).await?;

    // I have no idea why we loop here :shrug:
    let key = loop {
        match rustls_pemfile::read_one(&mut &contents[..])
            .expect("cannot parse private key .pem file")
        {
            Some(rustls_pemfile::Item::RSAKey(key)) => break rustls::PrivateKey(key),
            Some(rustls_pemfile::Item::PKCS8Key(key)) => break rustls::PrivateKey(key),
            Some(rustls_pemfile::Item::ECKey(key)) => break rustls::PrivateKey(key),
            None => return Err(anyhow!("no valid private key found in key file")),
            _ => {}
        }
    };

    Ok((certs, key))
}
