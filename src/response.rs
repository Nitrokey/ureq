use std::fmt;
use std::io::{self, Cursor, ErrorKind, Read};
use std::str::FromStr;
use std::time::Instant;

use chunked_transfer::Decoder as ChunkDecoder;

use crate::error::Error;
use crate::header::Header;
use crate::pool::PoolReturnRead;
use crate::stream::{DeadlineStream, Stream};
use crate::unit::Unit;

#[cfg(feature = "json")]
use serde::de::DeserializeOwned;

#[cfg(feature = "charset")]
use encoding::label::encoding_from_whatwg_label;
#[cfg(feature = "charset")]
use encoding::DecoderTrap;

pub const DEFAULT_CONTENT_TYPE: &str = "text/plain";
pub const DEFAULT_CHARACTER_SET: &str = "utf-8";

/// Response instances are created as results of firing off requests.
///
/// The `Response` is used to read response headers and decide what to do with the body.
/// Note that the socket connection is open and the body not read until one of
/// [`into_reader()`](#method.into_reader), [`into_json()`](#method.into_json),
/// [`into_json_deserialize()`](#method.into_json_deserialize) or
/// [`into_string()`](#method.into_string) consumes the response.
///
/// ```
/// let response = ureq::get("http://example.com/").call().unwrap();
///
/// // socket is still open and the response body has not been read.
///
/// let text = response.into_string().unwrap();
///
/// // response is consumed, and body has been read.
/// ```
pub struct Response {
    url: Option<String>,
    status_line: String,
    index: ResponseStatusIndex,
    status: u16,
    headers: Vec<Header>,
    unit: Option<Unit>,
    stream: Option<Stream>,
    deadline: Option<Instant>,
}

/// index into status_line where we split: HTTP/1.1 200 OK
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct ResponseStatusIndex {
    http_version: usize,
    response_code: usize,
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Response[status: {}, status_text: {}]",
            self.status(),
            self.status_text()
        )
    }
}

impl Response {
    /// Construct a response with a status, status text and a string body.
    ///
    /// This is hopefully useful for unit tests.
    ///
    /// Example:
    ///
    /// ```
    /// let resp = ureq::Response::new(401, "Authorization Required", "Please log in").unwrap();
    ///
    /// assert_eq!(resp.status(), 401);
    /// ```
    pub fn new(status: u16, status_text: &str, body: &str) -> Result<Response, Error> {
        let r = format!("HTTP/1.1 {} {}\r\n\r\n{}\n", status, status_text, body);
        (r.as_ref() as &str).parse()
    }

    /// The URL we ended up at. This can differ from the request url when
    /// we have followed redirects.
    pub fn get_url(&self) -> &str {
        self.url.as_ref().map(|s| &s[..]).unwrap_or("")
    }

    /// The entire status line like: `HTTP/1.1 200 OK`
    pub fn status_line(&self) -> &str {
        self.status_line.as_str()
    }

    /// The http version: `HTTP/1.1`
    pub fn http_version(&self) -> &str {
        &self.status_line.as_str()[0..self.index.http_version]
    }

    /// The status as a u16: `200`
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The status text: `OK`
    pub fn status_text(&self) -> &str {
        &self.status_line.as_str()[self.index.response_code + 1..].trim()
    }

    /// The header corresponding header value for the give name, if any.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.is_name(name))
            .map(|h| h.value())
    }

    /// A list of the header names in this response.
    /// Lowercased to be uniform.
    pub fn headers_names(&self) -> Vec<String> {
        self.headers
            .iter()
            .map(|h| h.name().to_lowercase())
            .collect()
    }

    /// Tells if the response has the named header.
    pub fn has(&self, name: &str) -> bool {
        self.header(name).is_some()
    }

    /// All headers corresponding values for the give name, or empty vector.
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|h| h.is_name(name))
            .map(|h| h.value())
            .collect()
    }

    /// Whether the response status is: 200 <= status <= 299
    pub fn ok(&self) -> bool {
        self.status >= 200 && self.status <= 299
    }

    pub fn redirect(&self) -> bool {
        self.status >= 300 && self.status <= 399
    }

    /// Whether the response status is: 400 <= status <= 499
    pub fn client_error(&self) -> bool {
        self.status >= 400 && self.status <= 499
    }

    /// Whether the response status is: 500 <= status <= 599
    pub fn server_error(&self) -> bool {
        self.status >= 500 && self.status <= 599
    }

    /// Whether the response status is: 400 <= status <= 599
    pub fn error(&self) -> bool {
        self.client_error() || self.server_error()
    }

    /// The content type part of the "Content-Type" header without
    /// the charset.
    ///
    /// Example:
    ///
    /// ```
    /// # #[cfg(feature = "tls")] {
    /// let resp = ureq::get("https://www.google.com/").call().unwrap();
    /// assert_eq!("text/html; charset=ISO-8859-1", resp.header("content-type").unwrap());
    /// assert_eq!("text/html", resp.content_type());
    /// # }
    /// ```
    pub fn content_type(&self) -> &str {
        self.header("content-type")
            .map(|header| {
                header
                    .find(';')
                    .map(|index| &header[0..index])
                    .unwrap_or(header)
            })
            .unwrap_or(DEFAULT_CONTENT_TYPE)
    }

    /// The character set part of the "Content-Type".
    ///
    /// Example:
    ///
    /// ```
    /// # #[cfg(feature = "tls")] {
    /// let resp = ureq::get("https://www.google.com/").call().unwrap();
    /// assert_eq!("text/html; charset=ISO-8859-1", resp.header("content-type").unwrap());
    /// assert_eq!("ISO-8859-1", resp.charset());
    /// # }
    /// ```
    pub fn charset(&self) -> &str {
        charset_from_content_type(self.header("content-type"))
    }

    /// Turn this response into a `impl Read` of the body.
    ///
    /// 1. If `Transfer-Encoding: chunked`, the returned reader will unchunk it
    ///    and any `Content-Length` header is ignored.
    /// 2. If `Content-Length` is set, the returned reader is limited to this byte
    ///    length regardless of how many bytes the server sends.
    /// 3. If no length header, the reader is until server stream end.
    ///
    /// Example:
    ///
    /// ```
    /// # #[cfg(feature = "tls")] {
    /// use std::io::Read;
    ///
    /// let resp =
    ///     ureq::get("https://ureq.s3.eu-central-1.amazonaws.com/hello_world.json")
    ///         .call().unwrap();
    ///
    /// assert!(resp.has("Content-Length"));
    /// let len = resp.header("Content-Length")
    ///     .and_then(|s| s.parse::<usize>().ok()).unwrap();
    ///
    /// let mut reader = resp.into_reader();
    /// let mut bytes = vec![];
    /// reader.read_to_end(&mut bytes);
    ///
    /// assert_eq!(bytes.len(), len);
    /// # }
    /// ```
    pub fn into_reader(self) -> impl Read + Send {
        //
        let is_http10 = self.http_version().eq_ignore_ascii_case("HTTP/1.0");
        let is_close = self
            .header("connection")
            .map(|c| c.eq_ignore_ascii_case("close"))
            .unwrap_or(false);

        let is_head = (&self.unit).as_ref().map(|u| u.is_head()).unwrap_or(false);
        let has_no_body = is_head
            || match self.status {
                204 | 304 => true,
                _ => false,
            };

        let is_chunked = self
            .header("transfer-encoding")
            .map(|enc| !enc.is_empty()) // whatever it says, do chunked
            .unwrap_or(false);

        let use_chunked = !is_http10 && !has_no_body && is_chunked;

        let limit_bytes = if is_http10 || is_close {
            None
        } else if has_no_body {
            // head requests never have a body
            Some(0)
        } else {
            self.header("content-length")
                .and_then(|l| l.parse::<usize>().ok())
        };

        let stream = self.stream.expect("No reader in response?!");
        let unit = self.unit;
        let deadline = unit.as_ref().and_then(|u| u.deadline);
        let stream = DeadlineStream::new(stream, deadline);

        match (use_chunked, limit_bytes) {
            (true, _) => Box::new(PoolReturnRead::new(unit, ChunkDecoder::new(stream)))
                as Box<dyn Read + Send>,
            (false, Some(len)) => {
                Box::new(PoolReturnRead::new(unit, LimitedRead::new(stream, len)))
            }
            (false, None) => Box::new(stream),
        }
    }

    /// Turn this response into a String of the response body. By default uses `utf-8`,
    /// but can work with charset, see below.
    ///
    /// This is potentially memory inefficient for large bodies since the
    /// implementation first reads the reader to end into a `Vec<u8>` and then
    /// attempts to decode it using the charset.
    ///
    /// Example:
    ///
    /// ```
    /// # #[cfg(feature = "tls")] {
    /// let resp =
    ///     ureq::get("https://ureq.s3.eu-central-1.amazonaws.com/hello_world.json")
    ///         .call().unwrap();
    ///
    /// let text = resp.into_string().unwrap();
    ///
    /// assert!(text.contains("hello"));
    /// # }
    /// ```
    ///
    /// ## Charset support
    ///
    /// Requires feature `ureq = { version = "*", features = ["charset"] }`
    ///
    /// Attempts to respect the character encoding of the `Content-Type` header and
    /// falls back to `utf-8`.
    ///
    /// I.e. `Content-Length: text/plain; charset=iso-8859-1` would be decoded in latin-1.
    ///
    pub fn into_string(self) -> io::Result<String> {
        #[cfg(feature = "charset")]
        {
            let encoding = encoding_from_whatwg_label(self.charset())
                .or_else(|| encoding_from_whatwg_label(DEFAULT_CHARACTER_SET))
                .unwrap();
            let mut buf: Vec<u8> = vec![];
            self.into_reader().read_to_end(&mut buf)?;
            Ok(encoding.decode(&buf, DecoderTrap::Replace).unwrap())
        }
        #[cfg(not(feature = "charset"))]
        {
            let mut buf: Vec<u8> = vec![];
            self.into_reader().read_to_end(&mut buf)?;
            Ok(String::from_utf8_lossy(&buf).to_string())
        }
    }

    /// Turn this response into a (serde) JSON value of the response body.
    ///
    /// Requires feature `ureq = { version = "*", features = ["json"] }`
    ///
    /// Example:
    ///
    /// ```
    /// let resp =
    ///     ureq::get("http://ureq.s3.eu-central-1.amazonaws.com/hello_world.json")
    ///         .call().unwrap();
    ///
    /// let json = resp.into_json().unwrap();
    ///
    /// assert_eq!(json["hello"], "world");
    /// ```
    #[cfg(feature = "json")]
    pub fn into_json(self) -> io::Result<serde_json::Value> {
        use crate::stream::io_err_timeout;
        use std::error::Error;

        let reader = self.into_reader();
        serde_json::from_reader(reader).map_err(|e| {
            // This is to unify TimedOut io::Error in the API.
            // We make a clone of the original error since serde_json::Error doesn't
            // let us get the wrapped error instance back.
            if let Some(ioe) = e.source().and_then(|s| s.downcast_ref::<io::Error>()) {
                if ioe.kind() == ErrorKind::TimedOut {
                    return io_err_timeout(ioe.to_string());
                }
            }

            io::Error::new(
                ErrorKind::InvalidData,
                format!("Failed to read JSON: {}", e),
            )
        })
    }

    /// Turn the body of this response into a type implementing the (serde) Deserialize trait.
    ///
    /// Requires feature `ureq = { version = "*", features = ["json"] }`
    ///
    /// Example:
    ///
    /// ```
    /// # use serde::Deserialize;
    ///
    /// #[derive(Deserialize)]
    /// struct Hello {
    ///     hello: String,
    /// }
    ///
    /// let resp =
    ///     ureq::get("http://ureq.s3.eu-central-1.amazonaws.com/hello_world.json")
    ///         .call().unwrap();
    ///
    /// let json = resp.into_json_deserialize::<Hello>().unwrap();
    ///
    /// assert_eq!(json.hello, "world");
    /// ```
    #[cfg(feature = "json")]
    pub fn into_json_deserialize<T: DeserializeOwned>(self) -> io::Result<T> {
        let reader = self.into_reader();
        serde_json::from_reader(reader).map_err(|e| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("Failed to read JSON: {}", e),
            )
        })
    }

    /// Create a response from a Read trait impl.
    ///
    /// This is hopefully useful for unit tests.
    ///
    /// Example:
    ///
    /// use std::io::Cursor;
    ///
    /// let text = "HTTP/1.1 401 Authorization Required\r\n\r\nPlease log in\n";
    /// let read = Cursor::new(text.to_string().into_bytes());
    /// let resp = ureq::Response::do_from_read(read);
    ///
    /// assert_eq!(resp.status(), 401);
    pub(crate) fn do_from_read(mut reader: impl Read) -> Result<Response, Error> {
        //
        // HTTP/1.1 200 OK\r\n
        let status_line = read_next_line(&mut reader)?;

        let (index, status) = parse_status_line(status_line.as_str())?;

        let mut headers: Vec<Header> = Vec::new();
        loop {
            let line = read_next_line(&mut reader)?;
            if line.is_empty() {
                break;
            }
            if let Ok(header) = line.as_str().parse::<Header>() {
                headers.push(header);
            }
        }

        Ok(Response {
            url: None,
            status_line,
            index,
            status,
            headers,
            unit: None,
            stream: None,
            deadline: None,
        })
    }

    #[cfg(test)]
    pub fn to_write_vec(&self) -> Vec<u8> {
        self.stream.as_ref().unwrap().to_write_vec()
    }
}

/// parse a line like: HTTP/1.1 200 OK\r\n
fn parse_status_line(line: &str) -> Result<(ResponseStatusIndex, u16), Error> {
    //

    let mut split = line.splitn(3, ' ');

    let http_version = split.next().ok_or_else(|| Error::BadStatus)?;
    if http_version.len() < 5 {
        return Err(Error::BadStatus);
    }
    let index1 = http_version.len();

    let status = split.next().ok_or_else(|| Error::BadStatus)?;
    if status.len() < 2 {
        return Err(Error::BadStatus);
    }
    let index2 = index1 + status.len();

    let status = status.parse::<u16>().map_err(|_| Error::BadStatus)?;

    Ok((
        ResponseStatusIndex {
            http_version: index1,
            response_code: index2,
        },
        status,
    ))
}

impl FromStr for Response {
    type Err = Error;
    /// Parse a response from a string.
    ///
    /// Example:
    /// ```
    /// let s = "HTTP/1.1 200 OK\r\n\
    ///     X-Forwarded-For: 1.2.3.4\r\n\
    ///     Content-Type: text/plain\r\n\
    ///     \r\n\
    ///     Hello World!!!";
    /// let resp = s.parse::<ureq::Response>().unwrap();
    /// assert!(resp.has("X-Forwarded-For"));
    /// let body = resp.into_string().unwrap();
    /// assert_eq!(body, "Hello World!!!");
    /// ```
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes().to_owned();
        let mut cursor = Cursor::new(bytes);
        let mut resp = Self::do_from_read(&mut cursor)?;
        set_stream(&mut resp, "".into(), None, Stream::Cursor(cursor));
        Ok(resp)
    }
}

/// "Give away" Unit and Stream to the response.
///
/// *Internal API*
pub(crate) fn set_stream(resp: &mut Response, url: String, unit: Option<Unit>, stream: Stream) {
    resp.url = Some(url);
    if let Some(unit) = &unit {
        resp.deadline = unit.deadline;
    }
    resp.unit = unit;
    resp.stream = Some(stream);
}

fn read_next_line<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut prev_byte_was_cr = false;
    let mut one = [0_u8];

    loop {
        let amt = reader.read(&mut one[..])?;

        if amt == 0 {
            return Err(io::Error::new(
                ErrorKind::ConnectionAborted,
                "Unexpected EOF",
            ));
        }

        let byte = one[0];

        if byte == b'\n' && prev_byte_was_cr {
            buf.pop(); // removing the '\r'
            return String::from_utf8(buf)
                .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "Header is not in ASCII"));
        }

        prev_byte_was_cr = byte == b'\r';

        buf.push(byte);
    }
}

/// Limits a `Read` to a content size (as set by a "Content-Length" header).
struct LimitedRead<R> {
    reader: R,
    limit: usize,
    position: usize,
}

impl<R: Read> LimitedRead<R> {
    fn new(reader: R, limit: usize) -> Self {
        LimitedRead {
            reader,
            limit,
            position: 0,
        }
    }
}

impl<R: Read> Read for LimitedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.limit - self.position;
        if left == 0 {
            return Ok(0);
        }
        let from = if left < buf.len() {
            &mut buf[0..left]
        } else {
            buf
        };
        match self.reader.read(from) {
            // https://tools.ietf.org/html/rfc7230#page-33
            // If the sender closes the connection or
            // the recipient times out before the indicated number of octets are
            // received, the recipient MUST consider the message to be
            // incomplete and close the connection.
            Ok(0) => Err(io::Error::new(
                ErrorKind::InvalidData,
                "response body closed before all bytes were read",
            )),
            Ok(amount) => {
                self.position += amount;
                Ok(amount)
            }
            Err(e) => Err(e),
        }
    }
}

#[test]
fn short_read() {
    use std::io::Cursor;
    let mut lr = LimitedRead::new(Cursor::new(vec![b'a'; 3]), 10);
    let mut buf = vec![0; 1000];
    let result = lr.read_to_end(&mut buf);
    assert!(result.is_err());
}

impl<R: Read> From<LimitedRead<R>> for Stream
where
    Stream: From<R>,
{
    fn from(limited_read: LimitedRead<R>) -> Stream {
        limited_read.reader.into()
    }
}

/// Extract the charset from a "Content-Type" header.
///
/// "Content-Type: text/plain; charset=iso8859-1" -> "iso8859-1"
///
/// *Internal API*
pub(crate) fn charset_from_content_type(header: Option<&str>) -> &str {
    header
        .and_then(|header| {
            header.find(';').and_then(|semi| {
                (&header[semi + 1..])
                    .find('=')
                    .map(|equal| (&header[semi + equal + 2..]).trim())
            })
        })
        .unwrap_or(DEFAULT_CHARACTER_SET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_without_charset() {
        let s = "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 \r\n\
                 OK";
        let resp = s.parse::<Response>().unwrap();
        assert_eq!("application/json", resp.content_type());
    }

    #[test]
    fn content_type_with_charset() {
        let s = "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json; charset=iso-8859-4\r\n\
                 \r\n\
                 OK";
        let resp = s.parse::<Response>().unwrap();
        assert_eq!("application/json", resp.content_type());
    }

    #[test]
    fn content_type_default() {
        let s = "HTTP/1.1 200 OK\r\n\r\nOK";
        let resp = s.parse::<Response>().unwrap();
        assert_eq!("text/plain", resp.content_type());
    }

    #[test]
    fn charset() {
        let s = "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json; charset=iso-8859-4\r\n\
                 \r\n\
                 OK";
        let resp = s.parse::<Response>().unwrap();
        assert_eq!("iso-8859-4", resp.charset());
    }

    #[test]
    fn charset_default() {
        let s = "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 \r\n\
                 OK";
        let resp = s.parse::<Response>().unwrap();
        assert_eq!("utf-8", resp.charset());
    }

    #[test]
    fn chunked_transfer() {
        let s = "HTTP/1.1 200 OK\r\n\
                 Transfer-Encoding: Chunked\r\n\
                 \r\n\
                 3\r\n\
                 hel\r\n\
                 b\r\n\
                 lo world!!!\r\n\
                 0\r\n\
                 \r\n";
        let resp = s.parse::<Response>().unwrap();
        assert_eq!("hello world!!!", resp.into_string().unwrap());
    }

    #[test]
    #[cfg(feature = "json")]
    fn parse_simple_json() {
        let s = "HTTP/1.1 200 OK\r\n\
             \r\n\
             {\"hello\":\"world\"}";
        let resp = s.parse::<Response>().unwrap();
        let v = resp.into_json().unwrap();
        let compare = "{\"hello\":\"world\"}"
            .parse::<serde_json::Value>()
            .unwrap();
        assert_eq!(v, compare);
    }

    #[test]
    #[cfg(feature = "json")]
    fn parse_deserialize_json() {
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct Hello {
            hello: String,
        }

        let s = "HTTP/1.1 200 OK\r\n\
             \r\n\
             {\"hello\":\"world\"}";
        let resp = s.parse::<Response>().unwrap();
        let v = resp.into_json_deserialize::<Hello>().unwrap();
        assert_eq!(v.hello, "world");
    }

    #[test]
    fn parse_borked_header() {
        let s = "HTTP/1.1 BORKED\r\n".to_string();
        let err = s.parse::<Response>().unwrap_err();
        assert!(matches!(err, Error::BadStatus));
    }
}
