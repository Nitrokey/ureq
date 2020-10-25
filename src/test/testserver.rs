use std::io::{self, BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

pub struct TestServer {
    pub port: u16,
    pub done: Arc<AtomicBool>,
}

pub struct TestHeaders(Vec<String>);

impl TestHeaders {
    // Return the path for a request, e.g. /foo from "GET /foo HTTP/1.1"
    #[cfg(feature = "cookies")]
    pub fn path(&self) -> &str {
        if self.0.len() == 0 {
            ""
        } else {
            &self.0[0].split(" ").nth(1).unwrap()
        }
    }

    #[cfg(feature = "cookies")]
    pub fn headers(&self) -> &[String] {
        &self.0[1..]
    }
}

// Read a stream until reaching a blank line, in order to consume
// request headers.
pub fn read_headers(stream: &TcpStream) -> TestHeaders {
    let mut results = vec![];
    for line in BufReader::new(stream).lines() {
        match line {
            Err(e) => {
                eprintln!("testserver: in read_headers: {}", e);
                break;
            }
            Ok(line) if line == "" => break,
            Ok(line) => results.push(line),
        };
    }
    TestHeaders(results)
}

impl TestServer {
    pub fn new(handler: fn(TcpStream) -> io::Result<()>) -> Self {
        let listener = TcpListener::bind("localhost:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = done.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                if let Err(e) = stream {
                    eprintln!("testserver: handling just-accepted stream: {}", e);
                    break;
                }
                if done.load(Ordering::SeqCst) {
                    break;
                } else {
                    thread::spawn(move || handler(stream.unwrap()));
                }
            }
        });
        TestServer {
            port,
            done: done_clone,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        // Connect once to unblock the listen loop.
        TcpStream::connect(format!("localhost:{}", self.port)).unwrap();
    }
}
