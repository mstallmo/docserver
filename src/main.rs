mod threadpool;

use bytes::{Buf, BufMut};
use clap::Parser;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::ffi::OsStr;
use std::fmt::Display;
use std::fs;
use std::io::{BufRead, Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::string::FromUtf8Error;
use std::sync::{Arc, RwLock};

#[derive(Debug, Parser)]
struct Args {
    #[arg(short, long, default_value = "4221")]
    port: u16,
    #[arg(short, long)]
    directory: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();

    let app = match args.directory {
        Some(file_dir) => App::new().with_file_dir(file_dir),
        None => App::new(),
    };

    app.add_handler(Method::Get, "/:file", file);

    let listener = TcpListener::bind(format!("127.0.0.1:{}", args.port)).unwrap();
    app.run(listener);
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum Method {
    Get,
    Post,
}

impl From<&str> for Method {
    fn from(method: &str) -> Self {
        match method.to_lowercase().as_str() {
            "get" => Method::Get,
            "post" => Method::Post,
            _ => panic!("Unsupported method"),
        }
    }
}

impl From<&String> for Method {
    fn from(method: &String) -> Self {
        match method.to_lowercase().as_str() {
            "get" => Method::Get,
            "post" => Method::Post,
            _ => panic!("Unsupported method"),
        }
    }
}

type Handlers = Arc<
    RwLock<
        HashMap<
            (Method, Vec<String>),
            Box<dyn Fn(Arc<RwLock<Context>>, Request) -> Vec<u8> + Send + Sync + 'static>,
        >,
    >,
>;

#[derive(Debug, Clone, Default)]
struct Context {
    file_dir: Option<PathBuf>,
}

struct App {
    threadpool: threadpool::ThreadPool,
    handlers: Handlers,
    context: Arc<RwLock<Context>>,
}

impl App {
    pub fn new() -> Self {
        let threadpool = threadpool::ThreadPool::new(4);
        let handlers = Arc::new(RwLock::new(HashMap::new()));

        App {
            threadpool,
            handlers,
            context: Arc::new(RwLock::new(Context::default())),
        }
    }

    pub fn with_file_dir(self, file_dir: PathBuf) -> Self {
        self.context.write().unwrap().file_dir = Some(file_dir);
        self
    }

    pub fn add_handler(
        &self,
        method: Method,
        path: impl ToString,
        handler: impl Fn(Arc<RwLock<Context>>, Request) -> Vec<u8> + Send + Sync + 'static,
    ) {
        let path = path
            .to_string()
            .split('/')
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        let handler = Box::new(handler);
        self.handlers
            .write()
            .unwrap()
            .insert((method, path), handler);
    }

    pub fn run(&self, listener: TcpListener) {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => self.handle_connection(stream),
                Err(e) => {
                    println!("error: {}", e);
                }
            }
        }
    }

    fn handle_connection(&self, mut stream: TcpStream) {
        let context = Arc::clone(&self.context);
        let handlers = Arc::clone(&self.handlers);
        // TODO: Read all bytes from the stream not just a pre-set 1024 bytes
        self.threadpool.execute(move || {
            let buffer_len = 1024;
            let mut req_buffer = Vec::<u8>::new();

            // For `stream.read` to work properly we have to create a sized buffer with
            // a set buffer length
            let mut buffer = vec![0; buffer_len];
            let num_bytes = stream
                .read(&mut buffer)
                .expect("Failed to read request bytes");

            // Truncate the buffer to the actual number of bytes read before appending
            buffer.truncate(num_bytes);
            req_buffer.append(&mut buffer);

            let res = match Request::try_from(req_buffer) {
                Ok(request) => handle_request(context, handlers, request),
                Err(err_res) => err_res,
            };

            stream.write_all(&res).expect("Failed to write response");
            stream.flush().expect("Failed to flush response");
            stream
                .shutdown(std::net::Shutdown::Both)
                .expect("Failed to shutdown connection");
        });
    }
}

fn handle_request(ctx: Arc<RwLock<Context>>, handlers: Handlers, request: Request) -> Vec<u8> {
    let target = request
        .target()
        .split('/')
        .map(|s| s.to_string())
        .collect::<Vec<String>>();

    for ((method, pattern), handler) in &(*handlers.read().unwrap()) {
        let target_matches = pattern.iter().zip(target.iter()).all(|(p, t)| {
            // `:` is the placeholder for a dynamic segment
            if p.starts_with(":") { true } else { p == t }
        });

        if target_matches && method == &request.method() {
            return handler(ctx, request);
        } else {
            continue;
        }
    }

    Response::new(StatusCode::NotFound).into()
}

fn file(ctx: Arc<RwLock<Context>>, request: Request) -> Vec<u8> {
    println!("request target: {}", request.target());
    let mut file_path = match request
        .target()
        .split('/')
        .skip(1)
        .map(|elem| urlencoding::decode(elem).and_then(|data| Ok(data.to_string())))
        .collect::<Result<PathBuf, FromUtf8Error>>()
    {
        Ok(file_path) => file_path,
        Err(err) => {
            eprintln!("{err}");
            return Response::new(StatusCode::InternalServerError).into();
        }
    };

    println!("relative file path: {}", file_path.display());
    if file_path.file_name().is_none() {
        file_path.set_file_name("index.html");
    }
    println!("serving file path {}", file_path.display());

    let Some(file_dir) = &ctx.read().unwrap().file_dir else {
        return Response::new(StatusCode::NotFound).into();
    };

    let resolved_path = file_dir.join(file_path);
    let Ok(exists) = fs::exists(&resolved_path) else {
        return Response::new(StatusCode::NotFound).into();
    };

    if exists {
        let mut file = fs::File::open(&resolved_path).unwrap();

        if resolved_path.extension() == Some(OsStr::new("html")) {
            let mut content = String::new();
            file.read_to_string(&mut content).unwrap();

            Response::new(StatusCode::Ok)
                .with_body(Html(content))
                .into()
        } else if resolved_path.extension() == Some(OsStr::new("svg")) {
            let mut content = String::new();
            file.read_to_string(&mut content).unwrap();

            Response::new(StatusCode::Ok).with_body(Svg(content)).into()
        } else {
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer).unwrap();

            Response::new(StatusCode::Ok).with_body(buffer).into()
        }
    } else {
        Response::new(StatusCode::NotFound).into()
    }
}

#[derive(Default, Debug)]
struct Request {
    method: String,
    target: String,
    http_version: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    pub fn method(&self) -> Method {
        Method::from(&self.method)
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    #[expect(unused)]
    pub fn http_version(&self) -> &str {
        &self.http_version
    }

    pub fn headers(&self) -> &HashMap<String, String> {
        &self.headers
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl TryFrom<Vec<u8>> for Request {
    type Error = Vec<u8>;

    fn try_from(req_bytes: Vec<u8>) -> Result<Self, Self::Error> {
        // Create new request
        let mut request = Request::default();

        let mut req = Cursor::new(req_bytes);
        let mut line = String::new();
        req.read_line(&mut line).map_err(|err| {
            eprintln!("Failed to read line: {}", err);

            Self::Error::from(Response::new(StatusCode::BadRequest))
        })?;
        for (i, req_line_part) in line.split_whitespace().enumerate() {
            match i {
                0 => request.method = req_line_part.to_string(),
                1 => request.target = req_line_part.to_string(),
                2 => request.http_version = req_line_part.to_string(),
                _ => return Err(Response::new(StatusCode::BadRequest).into()),
            }
        }
        line.clear();

        // Parse headers
        while req.read_line(&mut line).map_err(|err| {
            eprintln!("Failed to read header: {}", err);
            Self::Error::from(Response::new(StatusCode::BadRequest))
        })? != 0
        {
            if line.as_bytes() == b"\r\n" {
                break;
            }

            let header_parts = line
                .split_terminator(": ")
                .map(|s| s.trim())
                .collect::<Vec<_>>();
            if header_parts.len() == 2 {
                request
                    .headers
                    .insert(header_parts[0].to_string(), header_parts[1].to_string());
            }

            line.clear()
        }
        line.clear();

        let mut content_length = 0;
        for (key, val) in request.headers() {
            if key == "Content-Length" {
                content_length = val.parse().expect("Failed to parse content length");
            }
        }

        if req.remaining() != 0 && content_length == 0 {
            eprintln!("Missing Content-Length header with body present");
            return Err(Response::new(StatusCode::LengthRequired).into());
        }

        let mut body = vec![0; content_length];
        req.read_exact(&mut body).map_err(|err| {
            eprintln!("Failed to read body into buffer: {err}");
            Self::Error::from(Response::new(StatusCode::BadRequest))
        })?;
        request.body = body;

        Ok(request)
    }
}

#[derive(Debug)]
enum StatusCode {
    Ok,
    Created,
    BadRequest,
    NotFound,
    LengthRequired,
    InternalServerError,
}

impl Display for StatusCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatusCode::Ok => write!(f, "200 OK"),
            StatusCode::Created => write!(f, "201 Created"),
            StatusCode::BadRequest => write!(f, "400 Bad Request"),
            StatusCode::NotFound => write!(f, "404 Not Found"),
            StatusCode::LengthRequired => write!(f, "411 Length Required"),
            StatusCode::InternalServerError => write!(f, "500 Internal Server Error"),
        }
    }
}

trait ResponseBody {
    fn content_type(&self) -> &str;
    fn content_length(&self) -> usize;
    fn to_bytes(self) -> Vec<u8>;
}

#[repr(transparent)]
struct Html(String);

impl ResponseBody for Html {
    fn content_type(&self) -> &str {
        "text/html"
    }

    fn content_length(&self) -> usize {
        self.0.len()
    }

    fn to_bytes(self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }
}

struct Svg(String);

impl ResponseBody for Svg {
    fn content_type(&self) -> &str {
        "image/svg+xml"
    }

    fn content_length(&self) -> usize {
        self.0.len()
    }

    fn to_bytes(self) -> Vec<u8> {
        self.0.as_bytes().to_vec()
    }
}

impl ResponseBody for String {
    fn content_type(&self) -> &str {
        "text/plain"
    }

    fn content_length(&self) -> usize {
        self.len()
    }

    fn to_bytes(self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}

impl ResponseBody for Vec<u8> {
    fn content_type(&self) -> &str {
        "application/octet-stream"
    }

    fn content_length(&self) -> usize {
        self.len()
    }

    fn to_bytes(self) -> Vec<u8> {
        self
    }
}

#[derive(Debug)]
struct Response {
    status_code: StatusCode,
    headers: Option<IndexMap<String, String>>,
    body: Option<Vec<u8>>,
}

impl Response {
    pub fn new(status_code: StatusCode) -> Self {
        Response {
            status_code,
            headers: None,
            body: None,
        }
    }

    pub fn with_body(mut self, body: impl ResponseBody) -> Self {
        let headers = self.headers.get_or_insert(IndexMap::new());
        headers.insert("Content-Type".to_string(), body.content_type().to_string());
        headers.insert(
            "Content-Length".to_string(),
            body.content_length().to_string(),
        );

        self.body = Some(body.to_bytes().to_vec());
        self
    }
}

impl From<Response> for Vec<u8> {
    fn from(mut response: Response) -> Self {
        let mut res = format!("HTTP/1.1 {}\r\n", response.status_code).into_bytes();
        if let Some(headers) = &response.headers {
            for (key, value) in headers {
                res.put(format!("{}: {}\r\n", key, value).as_bytes());
            }
        }
        res.put(&b"\r\n"[..]);
        if let Some(body) = &mut response.body {
            res.append(body);
        }

        res
    }
}

// #[cfg(test)]
// mod tests {
//     use super::*;

//     fn setup_app() -> App {
//         let app = App::new();

//         app.add_handler(Method::Get, "/", root);
//         app.add_handler(Method::Get, "/echo/:name".to_string(), echo);
//         app.add_handler(Method::Get, "/user-agent".to_string(), user_agent);

//         app
//     }

//     #[test]
//     fn it_parses_request_bytes_emtpy_header_empty_body() {
//         let req_bytes = b"GET / HTTP/1.1\r\n\r\n\r\n";
//         let request = Request::try_from(req_bytes.to_vec()).unwrap();

//         assert_eq!(request.method, "GET");
//         assert_eq!(request.target, "/");
//         assert_eq!(request.http_version, "HTTP/1.1");
//         assert_eq!(request.headers.len(), 0);
//     }

//     #[test]
//     fn it_parses_request_bytes_empty_body() {
//         let req_bytes = b"GET / HTTP/1.1\r\nHost: localhost:4221\r\nUser-Agent: curl/7.68.0\r\nAccept: */*\r\n\r\n\r\n";
//         let request = Request::try_from(req_bytes.to_vec()).unwrap();

//         assert_eq!(request.method, "GET");
//         assert_eq!(request.target, "/");
//         assert_eq!(request.http_version, "HTTP/1.1");
//         assert_eq!(request.headers.len(), 3);
//         assert_eq!(
//             request.headers.get("Host"),
//             Some(&"localhost:4221".to_string())
//         );
//         assert_eq!(
//             request.headers.get("User-Agent"),
//             Some(&"curl/7.68.0".to_string())
//         );
//         assert_eq!(request.headers.get("Accept"), Some(&"*/*".to_string()));
//     }

//     #[test]
//     fn it_parses_request_bytes() {
//         let req_bytes = b"GET / HTTP/1.1\r\nHost: localhost:4221\r\nUser-Agent: curl/7.68.0\r\nAccept: */*\r\n\r\nHello, World!\r\n";
//         let request = Request::try_from(req_bytes.to_vec()).unwrap();

//         assert_eq!(request.method, "GET");
//         assert_eq!(request.target, "/");
//         assert_eq!(request.http_version, "HTTP/1.1");
//         assert_eq!(request.headers.len(), 3);
//         assert_eq!(
//             request.headers.get("Host"),
//             Some(&"localhost:4221".to_string())
//         );
//         assert_eq!(
//             request.headers.get("User-Agent"),
//             Some(&"curl/7.68.0".to_string())
//         );
//         assert_eq!(request.headers.get("Accept"), Some(&"*/*".to_string()));
//         assert_eq!(
//             String::from_utf8(request.body).unwrap(),
//             "Hello, World!".to_string()
//         );
//     }

//     #[test]
//     fn it_returns_a_not_found_response() {
//         let app = setup_app();
//         let req_bytes = b"GET /not_found HTTP/1.1\r\nHost: localhost:4221\r\nUser-Agent: curl/7.64.1\r\nAccept: */*\r\n\r\n";
//         let request = Request::try_from(req_bytes.to_vec()).unwrap();

//         let res = handle_request(Arc::clone(&app.context), Arc::clone(&app.handlers), request);
//         assert_eq!(&res, b"HTTP/1.1 404 Not Found\r\n\r\n");
//     }

//     #[test]
//     fn it_returns_an_echo_response() {
//         let app = setup_app();

//         let req_bytes = b"GET /echo/abc HTTP/1.1\r\nHost: localhost:4221\r\nUser-Agent: curl/7.64.1\r\nAccept: */*\r\n\r\n";
//         let request = Request::try_from(req_bytes.to_vec()).unwrap();

//         let res = handle_request(Arc::clone(&app.context), Arc::clone(&app.handlers), request);
//         assert_eq!(
//             &res,
//             b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 3\r\n\r\nabc"
//         );
//     }

//     #[test]
//     fn it_returns_the_user_agent() {
//         let app = setup_app();

//         let req_bytes = b"GET /user-agent HTTP/1.1\r\nHost: localhost:4221\r\nUser-Agent: foobar/1.2.3\r\nAccept: */*\r\n\r\n";
//         let request = Request::try_from(req_bytes.to_vec()).unwrap();

//         let res = handle_request(Arc::clone(&app.context), Arc::clone(&app.handlers), request);
//         assert_eq!(
//             &res,
//             b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 12\r\n\r\nfoobar/1.2.3"
//         );
//     }
// }
