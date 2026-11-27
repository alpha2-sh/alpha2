use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct TestRequest {
    pub path: String,
    pub headers: Vec<(String, String)>,
}

impl TestRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub struct TestResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl TestResponse {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

type Handler = dyn Fn(&TestRequest) -> TestResponse + Send + Sync;

pub struct HttpsServer {
    port: u16,
    root: Vec<u8>,
    requests: Arc<Mutex<Vec<TestRequest>>>,
    connections: Arc<AtomicUsize>,
    served: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    accepting: Option<std::thread::JoinHandle<()>>,
}

impl HttpsServer {
    pub fn start(handler: impl Fn(&TestRequest) -> TestResponse + Send + Sync + 'static) -> Self {
        let certified = rcgen::generate_simple_self_signed(vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
        ])
        .expect("a certificate");
        let root = certified.cert.der().to_vec();
        let key =
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key)
            .expect("a server config"),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("a listener");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("an address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let served = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let accepting = {
            let (requests, connections, served, stop) = (
                Arc::clone(&requests),
                Arc::clone(&connections),
                Arc::clone(&served),
                Arc::clone(&stop),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            connections.fetch_add(1, Ordering::AcqRel);
                            let (config, handler, requests, served, stop) = (
                                Arc::clone(&config),
                                Arc::clone(&handler),
                                Arc::clone(&requests),
                                Arc::clone(&served),
                                Arc::clone(&stop),
                            );
                            std::thread::spawn(move || {
                                serve(stream, &config, &*handler, &requests, &served, &stop);
                            });
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(2)),
                    }
                }
            })
        };
        Self {
            port,
            root,
            requests,
            connections,
            served,
            stop,
            accepting: Some(accepting),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.port)
    }

    pub fn origin(&self) -> String {
        format!("https://localhost:{}", self.port)
    }

    pub fn root(&self) -> &[u8] {
        &self.root
    }

    pub fn requests(&self) -> Vec<TestRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Acquire)
    }

    pub fn served_bytes(&self) -> usize {
        self.served.load(Ordering::Acquire)
    }
}

impl Drop for HttpsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(accepting) = self.accepting.take() {
            let _ = accepting.join();
        }
    }
}

fn read_head(stream: &mut impl Read) -> Option<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return None,
        }
    }
    Some(head)
}

fn serve(
    stream: TcpStream,
    config: &Arc<rustls::ServerConfig>,
    handler: &Handler,
    requests: &Mutex<Vec<TestRequest>>,
    served: &AtomicUsize,
    stop: &AtomicBool,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(connection) = rustls::ServerConnection::new(Arc::clone(config)) else {
        return;
    };
    let mut tls = rustls::StreamOwned::new(connection, stream);
    while !stop.load(Ordering::Acquire) {
        let Some(head) = read_head(&mut tls) else {
            return;
        };
        let text = String::from_utf8_lossy(&head);
        let mut lines = text.split("\r\n");
        let path = lines
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .unwrap_or_default()
            .to_owned();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
            .collect();
        let request = TestRequest { path, headers };
        requests.lock().unwrap().push(request.clone());
        let response = handler(&request);
        served.fetch_add(response.body.len(), Ordering::AcqRel);
        let mut out = format!("HTTP/1.1 {} Status\r\n", response.status);
        for (name, value) in &response.headers {
            let _ = write!(out, "{name}: {value}\r\n");
        }
        let _ = write!(out, "Content-Length: {}\r\n\r\n", response.body.len());
        if tls.write_all(out.as_bytes()).is_err()
            || tls.write_all(&response.body).is_err()
            || tls.flush().is_err()
        {
            return;
        }
    }
}

/// Serves `bytes` by range as an object store does: `206` with its `ETag`, and `412` once
/// `If-Match` names another tag.
pub fn ranges(
    bytes: Arc<Vec<u8>>,
    etag: Arc<Mutex<String>>,
) -> impl Fn(&TestRequest) -> TestResponse {
    move |request| {
        let etag = etag.lock().unwrap().clone();
        if request.header("if-match").is_some_and(|tag| tag != etag) {
            return TestResponse::status(412);
        }
        let total = bytes.len() as u64;
        let Some((first, last)) = request
            .header("range")
            .and_then(|range| range.strip_prefix("bytes="))
            .and_then(|range| range.split_once('-'))
            .and_then(|(first, last)| {
                Some((first.parse::<u64>().ok()?, last.parse::<u64>().ok()?))
            })
        else {
            return TestResponse {
                status: 200,
                headers: vec![("ETag".into(), etag)],
                body: bytes.to_vec(),
            };
        };
        if first >= total {
            return TestResponse::status(416).header("Content-Range", &format!("bytes */{total}"));
        }
        let last = last.min(total - 1);
        let range = usize::try_from(first).unwrap()..=usize::try_from(last).unwrap();
        TestResponse {
            status: 206,
            headers: vec![
                (
                    "Content-Range".into(),
                    format!("bytes {first}-{last}/{total}"),
                ),
                ("ETag".into(), etag),
                (
                    "Last-Modified".into(),
                    "Sun, 06 Nov 1994 08:49:37 GMT".into(),
                ),
            ],
            body: bytes[range].to_vec(),
        }
    }
}
