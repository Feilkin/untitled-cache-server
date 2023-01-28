//! Ada's fast HTTPS cache server
// TODO: naming and cache invalidation

mod cache;

use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cache::CacheError;
use bytes::Bytes;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use docopt::Docopt;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::http::HeaderValue;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use serde::Deserialize;
use tokio;

const USAGE: &'static str = "
Usage: zoomer -c CERT -k PRIVATEKEY [options] 
       zoomer --help

Options:
    -p PORT         What port to bind to.
    -a ADDRESS      What interface/ip address to bind to.
    -c CERT         Certificate path.
    -k PRIVATEKEY  Private key path.
    --help          Show help message.
";

#[derive(Debug, Deserialize)]
struct Args {
    flag_p: Option<u16>,
    flag_a: Option<String>,
    flag_c: String,
    flag_k: String,
}

async fn fetch_content(path: String) -> cache::CacheResult {
    use tokio::fs::File;
    use tokio::io::AsyncReadExt;
    let mut file_path = PathBuf::new();

    file_path.push("content/");
    file_path.push(&path[1..]);

    let mut file = File::open(&file_path).await.map_err(|e| {
        eprintln!("can't load {}: {}", file_path.to_str().unwrap(), e);
        CacheError::Fetch(Arc::new(e))
    })?;

    let mut contents = vec![];
    file.read_to_end(&mut contents).await.map_err(|e| {
        eprintln!("{}", e);
        CacheError::Fetch(Arc::new(e))
    })?;

    Ok((contents.into(), Instant::now() + Duration::from_secs(30)))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Hello, world!");
    // TODO: load configuration for ports and stuff
    // TODO: TLS

    let args: Args = Docopt::new(USAGE)
        .map(|d| d.help(true))
        .and_then(|d| d.deserialize())
        .unwrap_or_else(|e| e.exit());

    println!("{:?}", args);

    let content_cache = Arc::new(cache::TlruCache::new(1_000_000));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:80").await?;

    println!("Serving at 127.0.0.1:80");

    loop {
        let (socket, client_address) = listener.accept().await?;
        let content_cache = Arc::clone(&content_cache);

        tokio::task::spawn(async move {
            if let Err(e) = http1::Builder::new()
                // .header_read_timeout(Duration::from_secs(5))
                .serve_connection(
                    socket,
                    service_fn(move |req| {
                        let content_cache = Arc::clone(&content_cache);
                        handle_request(req, content_cache, client_address.clone())
                    }),
                )
                .await
            {
                eprintln!("failed to serve {}: {}", client_address, e);
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    content_cache: Arc<cache::TlruCache>,
    client_address: SocketAddr,
) -> Result<Response<Full<Bytes>>, &'static str> {
    println!("{}: {}", client_address, req.uri().path());

    let path = req.uri().path().to_owned();

    let content = content_cache
        .get_or_fetch(req.uri().path(), move || fetch_content(path))
        .await
        .map_err(|_| "it okay :)")?;

    let mut response = Response::new(Full::new(content.0));
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("image/jpeg"),
    );

    Ok(response)
}
