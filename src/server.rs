use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
};

use anyhow::{anyhow, Error, Result};
use async_compression::futures::bufread::{
    BrotliDecoder, BrotliEncoder, DeflateDecoder, DeflateEncoder, GzipDecoder, GzipEncoder,
};
use http_types::{
    headers::HeaderValue, Body, Error as HttpError, Request, Response, StatusCode, Url,
};
use smol::{io::AsyncRead, Async, Task};

use crate::constants::{CONFIG, FORWARD};

struct Target {
    scheme: String,
    host: String,
    port: u16,
}

impl Target {
    fn scheme(&self) -> &str {
        &self.scheme
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn port(&self) -> u16 {
        self.port
    }

    async fn address(&self) -> Result<SocketAddr> {
        let host = self.host.to_string();
        let port = self.port;
        smol::unblock!((host.as_str(), port)
            .to_socket_addrs()?
            .next()
            .ok_or(anyhow!("invalid domain")))
    }

    fn fuse_request(&self, req: Request) -> Result<Request> {
        let mut req = req;
        req.insert_header("host", self.host());
        let dest_url = req.url_mut();
        dest_url
            .set_scheme(self.scheme())
            .map_err(|_| anyhow!("set scheme error"))?;
        dest_url.set_host(Some(self.host()))?;
        dest_url
            .set_port(Some(self.port))
            .map_err(|_| anyhow!("set port error"))?;
        Ok(req)
    }

    fn host_with_port(&self) -> String {
        if (self.scheme == "http" && self.port == 80)
            || (self.scheme == "https" && self.port == 443)
        {
            self.host.to_string()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl TryFrom<&str> for Target {
    type Error = Error;

    fn try_from(s: &str) -> Result<Target> {
        let s = if s.find("://").is_some() {
            s.to_string()
        } else {
            format!("https://{}", s)
        };
        let url: Url = s.parse()?;
        let host = url.host_str().ok_or(anyhow!("invalid domain"))?;
        let port = url
            .port_or_known_default()
            .ok_or(anyhow!("invalid domain"))?;
        Ok(Target {
            scheme: url.scheme().to_string(),
            host: host.to_string(),
            port,
        })
    }
}

pub struct Forward<'a> {
    domain: HashMap<&'a str, Target>,
}

impl<'a> Forward<'a> {
    pub fn new(domain_name: &'a HashMap<String, String>) -> Result<Forward<'a>> {
        let mut domain = HashMap::new();
        for (k, v) in domain_name {
            let target = v.as_str().try_into()?;
            domain.insert(k.as_str(), target);
        }
        Ok(Forward { domain })
    }

    pub async fn forward(&self, req: Request) -> http_types::Result<Response> {
        let url = req.url();
        let domain = match url.domain() {
            Some(h) => h,
            None => return Err(http_error("missing domain".to_string())),
        };
        match self.domain.get(domain) {
            Some(domain) => self.request(req, domain).await,
            None => return Err(http_error("invalid domain, check config file".to_string())),
        }
    }

    async fn request(&self, req: Request, target: &Target) -> http_types::Result<Response> {
        let host = target.host();
        let addr = target
            .address()
            .await
            .map_err(|_| http_error("invalid target".to_string()))?;
        let req = target
            .fuse_request(req)
            .map_err(|e| http_error(e.to_string()))?;

        let stream = match &CONFIG.socks5_server {
            Some(server) => {
                let server = server.clone();
                let server = smol::unblock!(server
                    .to_socket_addrs()?
                    .next()
                    .ok_or(anyhow!("invalid host")))?;
                socks5::connect_without_auth(server, (host.to_string(), target.port()).into())
                    .await?
            }
            None => Async::<TcpStream>::connect(addr).await?,
        };

        let mut resp = match target.scheme() {
            "https" => {
                let stream = async_native_tls::connect(host, stream).await?;
                async_h1::connect(stream, req).await?
            }
            "http" => async_h1::connect(stream, req).await?,
            s => return Err(http_error(format!("unsupported scheme: {}", s))),
        };

        if let Some(location) = resp.header("location") {
            let mut location = location.as_str().to_string();
            for (k, v) in &self.domain {
                location = location.replace(&v.host_with_port(), k);
            }
            resp.insert_header("location", location);
        }

        if let Some(referer) = resp.header("referer") {
            let mut referer = referer.as_str().to_string();
            for (k, v) in &self.domain {
                referer = referer.replace(&v.host_with_port(), k);
            }
            resp.insert_header("referer", referer);
        }

        if let Some(cookie) = resp.header("set-cookie") {
            let cookie: Vec<_> = cookie
                .iter()
                .map(|i| {
                    let i = i.as_str();
                    let i: Vec<_> = i
                        .split(';')
                        .filter(|i| {
                            let i = i.trim_start();
                            !(i.len() > 7 && i[..7].to_lowercase() == "domain=")
                        })
                        .collect();
                    let i = i.join(";");
                    unsafe { HeaderValue::from_bytes_unchecked(i.as_bytes().to_vec()) }
                })
                .collect();
            resp.insert_header("set-cookie", cookie.as_slice());
        }

        if resp.status() == StatusCode::NotModified {
            return Ok(resp);
        }

        Coder::De.code(&mut resp);

        // replace domain
        if let Some(content_type) = resp.content_type() {
            match content_type.essence() {
                "text/html"
                | "text/javascript"
                | "application/json"
                | "application/manifest+json" => match resp.body_string().await {
                    Ok(mut body) => {
                        for (k, v) in &self.domain {
                            body = body.replace(&v.host_with_port(), k);
                        }
                        resp.set_body(body);
                    }
                    Err(_) => error!("can not convert body to utf-8 string"),
                },
                _ => (),
            }
        }

        Coder::En.code(&mut resp);

        Ok(resp)
    }
}

enum Coder {
    De,
    En,
}

impl Coder {
    fn set_body<T>(resp: &mut Response, coder: T)
    where
        T: AsyncRead + Unpin + Send + Sync + 'static,
    {
        let coder = async_std::io::BufReader::new(coder);
        let body = Body::from_reader(coder, None);
        resp.set_body(body);
    }

    fn code(&self, resp: &mut Response) {
        if let Some(encoding) = resp.header("content-encoding") {
            let encoding = encoding.as_str();
            match encoding {
                "gzip" => {
                    let body = resp.take_body();
                    match self {
                        Coder::En => Coder::set_body(resp, GzipEncoder::new(body)),
                        Coder::De => Coder::set_body(resp, GzipDecoder::new(body)),
                    }
                }
                "br" => {
                    let body = resp.take_body();
                    match self {
                        Coder::En => Coder::set_body(resp, BrotliEncoder::new(body)),
                        Coder::De => Coder::set_body(resp, BrotliDecoder::new(body)),
                    }
                }
                "deflate" => {
                    let body = resp.take_body();
                    match self {
                        Coder::En => Coder::set_body(resp, DeflateEncoder::new(body)),
                        Coder::De => Coder::set_body(resp, DeflateDecoder::new(body)),
                    }
                }
                e => error!("unhandled encoding: {}", e),
            }
        }
    }
}

fn http_error(error: String) -> HttpError {
    HttpError::from_str(StatusCode::InternalServerError, error)
}

async fn serve(req: Request) -> http_types::Result<Response> {
    FORWARD.forward(req).await
}

pub fn run() -> Result<()> {
    smol::run(async {
        let addr: SocketAddr = CONFIG.listen_address.as_str().parse()?;
        let listener = Async::<TcpListener>::bind(addr)?;
        loop {
            let (stream, _) = listener.accept().await?;
            let stream = async_dup::Arc::new(stream);
            let task = Task::spawn(async move {
                if let Err(err) = async_h1::accept(stream, serve).await {
                    error!("Connection error: {:#?}", err);
                }
            });

            task.detach();
        }
    })
}
