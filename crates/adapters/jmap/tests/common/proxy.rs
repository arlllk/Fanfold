//! A local reverse proxy for exercising `jmap-client` against a REAL server that is
//! only reachable through `kubectl port-forward`.
//!
//! The server advertises its public address in the session document, and `jmap-client`
//! calls exactly that address. From a laptop it is unreachable, so this proxy passes
//! everything through and rewrites the advertised origin to its own. In production no
//! such thing is needed: the cluster's DNS resolves the public name to the server.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use tokio::{io::AsyncWriteExt, net::TcpListener, task::JoinHandle};
use url::Url;

use super::{read_request, respond};

pub struct SessionProxy {
    pub base: Url,
    task: JoinHandle<()>,
}

impl Drop for SessionProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SessionProxy {
    /// Forward to `upstream`, rewriting `public_origin` (e.g. `https://mx.example.org`)
    /// in JSON responses to this proxy's own address.
    pub async fn start(upstream: &str, public_origin: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base: Url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let (upstream, public, own) = (
            upstream.trim_end_matches('/').to_owned(),
            public_origin.to_owned(),
            base.as_str().trim_end_matches('/').to_owned(),
        );
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (upstream, public, own, http) =
                    (upstream.clone(), public.clone(), own.clone(), http.clone());
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut stream).await else {
                        return;
                    };
                    let method = reqwest::Method::from_bytes(request.method.as_bytes()).unwrap();
                    let mut forward = http.request(method, format!("{upstream}{}", request.path));
                    for (name, value) in &request.headers {
                        if !matches!(
                            name.as_str(),
                            "host" | "connection" | "content-length" | "accept-encoding"
                        ) {
                            forward = forward.header(name, value);
                        }
                    }
                    let Ok(mut response) = forward.body(request.body.clone()).send().await else {
                        respond(&mut stream, 500, &[], "upstream unreachable").await;
                        return;
                    };
                    let status = response.status().as_u16();
                    let content_type = response
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    let location = response
                        .headers()
                        .get("location")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    if content_type.starts_with("text/event-stream") {
                        // Stream it through untouched, chunk by chunk.
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n").await;
                        while let Ok(Some(chunk)) = response.chunk().await {
                            if stream.write_all(&chunk).await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
                    let body = response
                        .text()
                        .await
                        .unwrap_or_default()
                        .replace(&public, &own);
                    let mut headers: Vec<(&str, &str)> = vec![("content-type", &content_type)];
                    if let Some(l) = location.as_deref() {
                        headers.push(("location", l));
                    }
                    respond(&mut stream, status, &headers, &body).await;
                });
            }
        });
        Self { base, task }
    }
}
