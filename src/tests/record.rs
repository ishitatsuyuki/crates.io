extern crate futures;
extern crate hyper;
extern crate hyper_tls;
extern crate tokio_core;
extern crate tokio_service;

use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io;
use std::io::prelude::*;
use std::net;
use std::path::PathBuf;
use std::str;
use std::sync::{Arc, Mutex, Once};
use std::thread;

use self::futures::sync::oneshot;
use self::futures::{future, Future, Stream};
use self::tokio_core::net::TcpListener;
use self::tokio_core::reactor::Core;
use curl::easy::{Easy, List};
use serde_json;

use models::NewUser;
use new_user;

// A "bomb" so when the test task exists we know when to shut down
// the server and fail if the subtask failed.
pub struct Bomb {
    iorx: Sink,
    quittx: Option<oneshot::Sender<()>>,
    #[cfg_attr(feature = "cargo-clippy", allow(type_complexity))]
    thread: Option<thread::JoinHandle<Option<(Vec<u8>, PathBuf)>>>,
}

pub struct GhUser {
    pub login: &'static str,
    pub init: Once,
}

#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl<'a> Write for &'a Sink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        Write::write(&mut *self.0.lock().unwrap(), data)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Bomb {
    fn drop(&mut self) {
        drop(self.quittx.take());
        let res = self.thread.take().unwrap().join();
        let stderr = str::from_utf8(&self.iorx.0.lock().unwrap())
            .unwrap()
            .to_string();
        match res {
            Err(..) if !thread::panicking() => panic!("server subtask failed: {}", stderr),
            Err(e) => if !stderr.is_empty() {
                println!("server subtask failed ({:?}): {}", e, stderr)
            },
            Ok(_) if thread::panicking() => {}
            Ok(None) => {}
            Ok(Some((data, file))) => {
                t!(t!(File::create(&file)).write_all(&data));
            }
        }
    }
}

fn cache_file(name: &str) -> PathBuf {
    PathBuf::from(file!())
        .parent()
        .unwrap()
        .join("http-data")
        .join(name)
}

enum Record {
    Capture(Vec<Exchange>, PathBuf),
    Replay(Vec<Exchange>),
}

pub fn proxy() -> (String, Bomb) {
    let me = thread::current().name().unwrap().to_string();
    let record = env::var("RECORD").is_ok();

    let a = t!(net::TcpListener::bind("127.0.0.1:0"));
    let ret = format!("http://{}", t!(a.local_addr()));

    let data = cache_file(&me.replace("::", "_"));
    let record = if record && !data.exists() {
        Record::Capture(Vec::new(), data)
    } else if !data.exists() {
        Record::Replay(serde_json::from_slice(b"[]").unwrap())
    } else {
        let mut body = Vec::new();
        t!(t!(File::open(&data)).read_to_end(&mut body));
        Record::Replay(serde_json::from_slice(&body).unwrap())
    };

    let sink = Arc::new(Mutex::new(Vec::new()));
    let sink2 = Sink(Arc::clone(&sink));

    let (quittx, quitrx) = oneshot::channel();

    let thread = thread::spawn(move || {
        let mut core = t!(Core::new());
        let handle = core.handle();
        let addr = t!(a.local_addr());
        let listener = t!(TcpListener::from_listener(a, &addr, &handle));
        let client = hyper::Client::builder().build(hyper_tls::HttpsConnector::new(4).unwrap());

        let record = Arc::new(Mutex::new(record));
        let srv = hyper::Server::builder(listener.incoming().map(|(l, _)| l))
            .serve(Proxy {
                sink: sink2,
                record: Arc::clone(&record),
                client,
            }).map_err(|e| eprintln!("server connection error: {}", e));

        drop(core.run(srv.select2(quitrx)));

        let record = record.lock().unwrap();
        match *record {
            Record::Capture(ref data, ref path) => {
                let data = t!(serde_json::to_string(data));
                Some((data.into_bytes(), path.clone()))
            }
            Record::Replay(..) => None,
        }
    });

    (
        ret,
        Bomb {
            iorx: Sink(sink),
            quittx: Some(quittx),
            thread: Some(thread),
        },
    )
}

#[derive(Clone)]
struct Proxy {
    sink: Sink,
    record: Arc<Mutex<Record>>,
    client: Client,
}

impl hyper::service::Service for Proxy {
    type ReqBody = hyper::Body;
    type ResBody = hyper::Body;
    type Error = hyper::Error;
    type Future = Box<Future<Item = hyper::Response<Self::ResBody>, Error = Self::Error> + Send>;

    fn call(&mut self, req: hyper::Request<Self::ReqBody>) -> Self::Future {
        let record2 = self.record.clone();
        match *self.record.lock().unwrap() {
            Record::Capture(_, _) => Box::new(record_http(req, &self.client).map(
                move |(response, exchange)| {
                    if let Record::Capture(ref mut d, _) = *record2.lock().unwrap() {
                        d.push(exchange);
                    }
                    response
                },
            )),
            Record::Replay(ref mut exchanges) => {
                replay_http(req, exchanges.remove(0), &mut &self.sink)
            }
        }
    }
}

impl hyper::service::NewService for Proxy {
    type ReqBody = hyper::Body;
    type ResBody = hyper::Body;
    type Error = hyper::Error;
    type Service = Proxy;
    type Future = Box<Future<Item = Self::Service, Error = Self::InitError> + Send>;
    type InitError = hyper::Error;

    fn new_service(&self) -> Self::Future {
        Box::new(future::ok(self.clone()))
    }
}

#[derive(Serialize, Deserialize)]
struct Exchange {
    request: Request,
    response: Response,
}

#[derive(Serialize, Deserialize)]
struct Request {
    uri: String,
    method: String,
    headers: HashSet<(String, String)>,
    body: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Response {
    status: u16,
    headers: HashSet<(String, String)>,
    body: Vec<u8>,
}

type Client = hyper::Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>;

fn record_http(
    req: hyper::Request<hyper::Body>,
    client: &Client,
) -> Box<Future<Item = (hyper::Response<hyper::Body>, Exchange), Error = hyper::Error> + Send> {
    let (header_parts, body) = req.into_parts();
    let method = header_parts.method;
    let uri = header_parts.uri;
    let headers = header_parts.headers;

    let mut request = Request {
        uri: uri.to_string(),
        method: method.to_string(),
        headers: headers
            .iter()
            .map(|h| (h.0.as_str().to_string(), h.1.to_str().unwrap().to_string()))
            .collect(),
        body: Vec::new(),
    };
    let body = body.concat2();

    let client = client.clone();
    let response = body.and_then(move |body| {
        request.body = body.to_vec();
        let uri = uri.to_string().replace("http://", "https://");
        let uri = uri.parse::<hyper::Uri>().unwrap();
        let mut req = hyper::Request::builder()
            .method(method.clone())
            .uri(uri)
            .body(body.into())
            .unwrap();
        *req.headers_mut() = headers.clone();
        client.request(req).map(|r| (r, request))
    });

    Box::new(response.and_then(|(hyper_response, request)| {
        let status = hyper_response.status();
        let headers = hyper_response.headers().clone();
        let mut response = Response {
            status: status.as_u16(),
            headers: headers
                .iter()
                .map(|h| (h.0.as_str().to_string(), h.1.to_str().unwrap().to_string()))
                .collect(),
            body: Vec::new(),
        };

        hyper_response.into_body().concat2().map(move |body| {
            response.body = body.to_vec();
            let mut hyper_response = hyper::Response::builder();
            hyper_response.status(status);
            let mut hyper_response = hyper_response.body(body.into()).unwrap();
            *hyper_response.headers_mut() = headers;
            (
                hyper_response,
                Exchange {
                    response: response,
                    request: request,
                },
            )
        })
    }))
}

fn replay_http(
    req: hyper::Request<hyper::Body>,
    mut exchange: Exchange,
    stdout: &mut Write,
) -> Box<Future<Item = hyper::Response<hyper::Body>, Error = hyper::Error> + Send> {
    assert_eq!(req.uri().to_string(), exchange.request.uri);
    assert_eq!(req.method().to_string(), exchange.request.method);
    t!(writeln!(
        stdout,
        "expecting: {:?}",
        exchange.request.headers
    ));
    for (name, value) in req.headers().iter() {
        let pair = (
            name.as_str().to_string(),
            value.to_str().unwrap().to_string(),
        );
        t!(writeln!(stdout, "received: {:?}", pair));
        if name.as_str().starts_with("date") {
            continue;
        }
        if name.as_str().starts_with("authorization") {
            continue;
        }
        if !exchange.request.headers.remove(&pair) {
            panic!("found {:?} but didn't expect it", pair);
        }
    }
    for (name, value) in exchange.request.headers.drain() {
        if name.starts_with("date") {
            continue;
        }
        if name.starts_with("authorization") {
            continue;
        }
        panic!("didn't find header {:?}", (name, value));
    }
    let req_body = exchange.request.body;
    let verify_body = req.into_body().concat2().map(move |body| {
        assert_eq!(&body[..], &req_body[..]);
    });

    let mut response = hyper::Response::builder();
    response.status(hyper::StatusCode::from_u16(exchange.response.status).unwrap());
    for (key, value) in exchange.response.headers {
        response.header(key.as_str(), value.as_str());
    }
    let response = response.body(exchange.response.body.into()).unwrap();

    Box::new(verify_body.map(|()| response))
}

impl GhUser {
    pub fn user(&'static self) -> NewUser<'_> {
        self.init.call_once(|| self.init());
        let mut u = new_user(self.login);
        u.gh_access_token = Cow::Owned(self.token());
        u
    }

    fn filename(&self) -> PathBuf {
        cache_file(&format!("gh-{}", self.login))
    }

    fn token(&self) -> String {
        let mut token = String::new();
        File::open(&self.filename())
            .unwrap()
            .read_to_string(&mut token)
            .unwrap();
        token
    }

    fn init(&self) {
        if fs::metadata(&self.filename()).is_ok() {
            return;
        }

        let password = ::env(&format!("GH_PASS_{}", self.login.replace("-", "_")));
        #[derive(Serialize)]
        struct Authorization {
            scopes: Vec<String>,
            note: String,
            client_id: String,
            client_secret: String,
        }
        let mut handle = Easy::new();
        let body = serde_json::to_string(&Authorization {
            scopes: vec!["read:org".to_string()],
            note: "crates.io test".to_string(),
            client_id: ::env("GH_CLIENT_ID"),
            client_secret: ::env("GH_CLIENT_SECRET"),
        }).unwrap();

        t!(handle.url("https://api.github.com/authorizations"));
        t!(handle.username(self.login));
        t!(handle.password(&password));
        t!(handle.post(true));
        t!(handle.post_fields_copy(body.as_bytes()));

        let mut headers = List::new();
        headers.append("User-Agent: hello!").unwrap();
        t!(handle.http_headers(headers));

        let mut response = Vec::new();
        {
            let mut transfer = handle.transfer();
            t!(transfer.write_function(|data| {
                response.extend(data);
                Ok(data.len())
            }));
            t!(transfer.perform())
        }

        if t!(handle.response_code()) < 200 || t!(handle.response_code()) >= 300 {
            panic!("failed to get a 200 {}", String::from_utf8_lossy(&response));
        }

        #[derive(Deserialize)]
        struct Response {
            token: String,
        }
        let resp: Response = serde_json::from_str(str::from_utf8(&response).unwrap()).unwrap();
        File::create(&self.filename())
            .unwrap()
            .write_all(resp.token.as_bytes())
            .unwrap();
    }
}
