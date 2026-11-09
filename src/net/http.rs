//! Pure HTTP/1.1 request building and response-head parsing.
//!
//! This task (7.1) implements only the **pure** core of the `Package_Fetcher`
//! (design component 7): allocation-aware but socket-free logic that builds a
//! `GET` request and parses an HTTP response head. It is `core` + `alloc` only —
//! no socket types, no globals, no hardware — so it compiles into both the
//! kernel (`crate::net::http`) and the `host-tests` crate via `#[path]` include,
//! letting properties P20/P21 exercise the same source (R11.6).
//!
//! The effectful `fetch_deb` shell that pumps a real TCP socket and calls
//! [`parse_http_head`] lands in task 14.1.
//!
//! All parsing is hardened against adversarial input: every index is bounds
//! checked, slicing uses half-open ranges derived from found positions, and no
//! path can panic or read past `buf`.
#![allow(dead_code)]

use alloc::vec::Vec;

/// Outcome of parsing the head (status line + headers) of an HTTP response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadParse {
    /// The buffer does not yet contain a complete head (no `CRLFCRLF`
    /// terminator). The caller should read more bytes and retry.
    Need,
    /// A complete, well-formed head was parsed.
    Done {
        /// The numeric HTTP status code from the status line.
        status: u16,
        /// The parsed `Content-Length`, if a well-formed header was present.
        ///
        /// `Some(n)` only when a `Content-Length` header exists and its value
        /// parses as a non-negative integer; `None` when the header is absent or
        /// present but unparseable (R8.3 decides what to do with `None`).
        content_length: Option<u64>,
        /// Byte index just past the `CRLFCRLF` terminator — where the body
        /// begins within the buffer.
        body_off: usize,
        /// The head carried `Transfer-Encoding: chunked` (additive field, see
        /// [`ChunkedDecoder`]): the body length is framed by chunk headers, not
        /// by `Content-Length`. Real HTTPS origins use it — `example.com`
        /// behind Cloudflare answers a plain `GET` with chunked framing and no
        /// `Content-Length` at all — so a fetcher that only understands
        /// `Content-Length` cannot read the response it just authenticated.
        chunked: bool,
    },
    /// A head terminator was present but the status line was not well-formed.
    Malformed,
}

/// The CRLFCRLF sequence that terminates an HTTP message head.
const CRLFCRLF: &[u8] = b"\r\n\r\n";

/// Parse the head of an HTTP/1.x response from `buf` (R8.2, R8.3, R8.5).
///
/// Scans for the `CRLFCRLF` head terminator. If it is absent the head is
/// incomplete and [`HeadParse::Need`] is returned. Otherwise the status line is
/// parsed (`HTTP/1.x <3-digit code> ...`); a status line that does not begin with
/// `HTTP/` or whose code is not exactly three ASCII digits yields
/// [`HeadParse::Malformed`]. Header lines are parsed case-insensitively; a
/// `Content-Length` whose value parses as a non-negative integer sets
/// `content_length = Some(n)`, otherwise it stays `None`. `body_off` is the index
/// immediately following the terminator.
///
/// The function never indexes past `buf` and never panics on any input.
pub fn parse_http_head(buf: &[u8]) -> HeadParse {
    let term = match find_subslice(buf, CRLFCRLF) {
        Some(i) => i,
        None => return HeadParse::Need,
    };
    let body_off = term + CRLFCRLF.len();

    // The head is everything before the terminating CRLFCRLF. Individual lines
    // are separated by CRLF; the head itself contains no trailing blank line.
    let head = &buf[..term];
    let mut lines = SplitCrlf::new(head);

    let status_line = match lines.next() {
        Some(line) => line,
        None => return HeadParse::Malformed,
    };
    let status = match parse_status_code(status_line) {
        Some(code) => code,
        None => return HeadParse::Malformed,
    };

    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    for line in lines {
        if let Some((name, value)) = split_header(line) {
            if eq_ascii_ci(trim_ascii(name), b"content-length") {
                // Present-but-unparseable leaves content_length as None.
                content_length = parse_u64(trim_ascii(value));
            } else if eq_ascii_ci(trim_ascii(name), b"transfer-encoding") {
                // The coding list is comma-separated and case-insensitive;
                // only the `chunked` coding changes how the body is framed.
                chunked |= header_lists_token(trim_ascii(value), b"chunked");
            }
        }
    }

    HeadParse::Done {
        status,
        content_length,
        body_off,
        chunked,
    }
}

/// Whether a comma-separated header value names `token` (ASCII,
/// case-insensitive, surrounding spaces ignored).
fn header_lists_token(value: &[u8], token: &[u8]) -> bool {
    for part in value.split(|byte| *byte == b',') {
        if eq_ascii_ci(trim_ascii(part), token) {
            return true;
        }
    }
    false
}

// ──────────────────── chunked transfer decoding ────────────────────

/// Progress of a [`ChunkedDecoder`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChunkProgress {
    /// More body bytes are needed before the message is complete.
    Need,
    /// The terminating zero-length chunk (and any trailer) was consumed.
    Done,
    /// The chunked framing is malformed, or the body exceeded the decoder's
    /// cap: the body must be discarded, never returned as a short success.
    Malformed,
}

/// Maximum length of one chunk-size or trailer line. A longer line is a
/// framing error rather than an excuse to buffer unboundedly.
const CHUNK_LINE_MAX: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChunkState {
    /// Reading a chunk-size line (`<hex>[;ext]CRLF`).
    Size,
    /// Copying `remaining` bytes of chunk data.
    Data,
    /// Expecting the CRLF that terminates chunk data.
    DataCrlf,
    /// Reading trailer lines until the empty one.
    Trailer,
    /// The message is complete.
    Done,
    /// The framing is broken; the decoder stays failed.
    Failed,
}

/// Incremental `Transfer-Encoding: chunked` decoder (RFC 9112 §7.1).
///
/// Pure `core` + `alloc`, panic-free, bounded: [`ChunkedDecoder::push`] consumes
/// raw body bytes as they arrive off the socket and appends decoded payload to
/// an internal buffer capped at the `max` given to [`ChunkedDecoder::new`]. A
/// malformed size line, a missing CRLF, or a body over the cap all latch
/// [`ChunkProgress::Malformed`], so a truncated or lying stream can never look
/// like a complete body.
///
/// The decoded body is exposed by [`ChunkedDecoder::body`]; the zero-length
/// chunk and any trailer must both be seen before [`ChunkProgress::Done`], so
/// "the TLS stream ended" is never confused with "the message ended".
pub struct ChunkedDecoder {
    state: ChunkState,
    /// Bytes still to copy in [`ChunkState::Data`].
    remaining: usize,
    /// Current size/trailer line (CRLF and all).
    line: Vec<u8>,
    /// Decoded payload.
    decoded: Vec<u8>,
    /// Hard cap on `decoded`.
    max: usize,
}

impl ChunkedDecoder {
    /// A decoder that refuses to decode more than `max` payload bytes.
    pub fn new(max: usize) -> Self {
        ChunkedDecoder {
            state: ChunkState::Size,
            remaining: 0,
            line: Vec::new(),
            decoded: Vec::new(),
            max,
        }
    }

    /// The payload decoded so far (only complete once [`push`](Self::push)
    /// returned [`ChunkProgress::Done`]).
    pub fn body(&self) -> &[u8] {
        &self.decoded
    }

    /// Consume the next raw body bytes.
    pub fn push(&mut self, mut bytes: &[u8]) -> ChunkProgress {
        loop {
            match self.state {
                ChunkState::Failed => return ChunkProgress::Malformed,
                ChunkState::Done => return ChunkProgress::Done,
                ChunkState::Data => {
                    let take = self.remaining.min(bytes.len());
                    if self.decoded.len().saturating_add(take) > self.max {
                        self.state = ChunkState::Failed;
                        return ChunkProgress::Malformed;
                    }
                    self.decoded.extend_from_slice(&bytes[..take]);
                    self.remaining -= take;
                    bytes = &bytes[take..];
                    if self.remaining == 0 {
                        self.state = ChunkState::DataCrlf;
                        self.line.clear();
                    }
                    if bytes.is_empty() {
                        return ChunkProgress::Need;
                    }
                }
                ChunkState::DataCrlf => {
                    while self.line.len() < 2 && !bytes.is_empty() {
                        self.line.push(bytes[0]);
                        bytes = &bytes[1..];
                    }
                    if self.line.len() < 2 {
                        return ChunkProgress::Need;
                    }
                    if self.line[..2] != *b"\r\n" {
                        self.state = ChunkState::Failed;
                        return ChunkProgress::Malformed;
                    }
                    self.line.clear();
                    self.state = ChunkState::Size;
                }
                ChunkState::Size | ChunkState::Trailer => {
                    let mut consumed = 0usize;
                    let mut complete = false;
                    while consumed < bytes.len() {
                        self.line.push(bytes[consumed]);
                        consumed += 1;
                        if self.line.len() > CHUNK_LINE_MAX {
                            self.state = ChunkState::Failed;
                            return ChunkProgress::Malformed;
                        }
                        if self.line.ends_with(b"\r\n") {
                            complete = true;
                            break;
                        }
                    }
                    bytes = &bytes[consumed..];
                    if !complete {
                        return ChunkProgress::Need;
                    }
                    let line_len = self.line.len();
                    let content = &self.line[..line_len - 2];
                    if self.state == ChunkState::Size {
                        match parse_hex_size(content) {
                            // The zero-length chunk ends the body; the trailer
                            // (at least the final CRLF) still has to be read.
                            Some(0) => self.state = ChunkState::Trailer,
                            Some(size) => {
                                self.remaining = size;
                                self.state = ChunkState::Data;
                            }
                            None => {
                                self.state = ChunkState::Failed;
                                return ChunkProgress::Malformed;
                            }
                        }
                    } else if content.is_empty() {
                        self.state = ChunkState::Done;
                        return ChunkProgress::Done;
                    }
                    self.line.clear();
                }
            }
        }
    }
}

/// Parse the hexadecimal size of a chunk-size line, ignoring `;ext` extensions
/// and surrounding whitespace. `None` for anything that is not a non-empty run
/// of hex digits inside `usize`.
fn parse_hex_size(line: &[u8]) -> Option<usize> {
    let line = match line.iter().position(|byte| *byte == b';') {
        Some(index) => &line[..index],
        None => line,
    };
    let line = trim_ascii(line);
    if line.is_empty() {
        return None;
    }
    let mut value: usize = 0;
    for byte in line {
        let digit = match byte {
            b'0'..=b'9' => (byte - b'0') as usize,
            b'a'..=b'f' => (byte - b'a') as usize + 10,
            b'A'..=b'F' => (byte - b'A') as usize + 10,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit)?;
    }
    Some(value)
}

/// Build an HTTP/1.1 `GET` request for `path` on `host` (R8.1, Property 21).
///
/// Produces exactly:
///
/// ```text
/// GET <path> HTTP/1.1\r\n
/// Host: <host>\r\n
/// Connection: close\r\n
/// \r\n
/// ```
///
/// The result always begins with `GET <path> HTTP/1.1\r\n`, contains
/// `Host: <host>\r\n`, and ends with a blank line.
pub fn build_get_request(host: &str, path: &str) -> Vec<u8> {
    let mut req = Vec::new();
    req.extend_from_slice(b"GET ");
    req.extend_from_slice(path.as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\n");
    req.extend_from_slice(b"Host: ");
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(b"\r\n");
    req.extend_from_slice(b"Connection: close\r\n");
    req.extend_from_slice(b"\r\n");
    req
}

// ---------------------------------------------------------------------------
// Pure parsing helpers (bounds-checked, panic-free)
// ---------------------------------------------------------------------------

/// Return the index of the first occurrence of `needle` within `haystack`, or
/// `None`. Empty `needle` is treated as not found.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let n = needle.len();
    if n == 0 || haystack.len() < n {
        return None;
    }
    let mut i = 0;
    while i + n <= haystack.len() {
        if &haystack[i..i + n] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Iterator over CRLF-separated lines of a head slice. A bare trailing CRLF (no
/// following bytes) yields no extra empty line; intermediate empty lines are
/// preserved but never occur in a well-formed head before the terminator.
struct SplitCrlf<'a> {
    rest: &'a [u8],
    done: bool,
}

impl<'a> SplitCrlf<'a> {
    fn new(s: &'a [u8]) -> Self {
        SplitCrlf {
            rest: s,
            done: s.is_empty(),
        }
    }
}

impl<'a> Iterator for SplitCrlf<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.done {
            return None;
        }
        match find_subslice(self.rest, b"\r\n") {
            Some(i) => {
                let line = &self.rest[..i];
                self.rest = &self.rest[i + 2..];
                if self.rest.is_empty() {
                    self.done = true;
                }
                Some(line)
            }
            None => {
                self.done = true;
                Some(self.rest)
            }
        }
    }
}

/// Parse the numeric status code from a status line of the form
/// `HTTP/1.x <code> [reason]`. Returns `None` unless the line begins with
/// `HTTP/` and the code is exactly three ASCII digits.
fn parse_status_code(line: &[u8]) -> Option<u16> {
    if !line.starts_with(b"HTTP/") {
        return None;
    }
    // The status code follows the first space.
    let sp = line.iter().position(|&b| b == b' ')?;
    let rest = &line[sp + 1..];
    // The code token ends at the next space or end of line.
    let code_end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    let code = &rest[..code_end];
    if code.len() != 3 || !code.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value =
        (code[0] - b'0') as u16 * 100 + (code[1] - b'0') as u16 * 10 + (code[2] - b'0') as u16;
    Some(value)
}

/// Split a header line into `(name, value)` at the first colon, or `None` if the
/// line has no colon.
fn split_header(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&b| b == b':')?;
    let name = &line[..colon];
    let value = &line[colon + 1..];
    Some((name, value))
}

/// Trim leading and trailing ASCII spaces and tabs from a byte slice.
fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if *first == b' ' || *first == b'\t' {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if *last == b' ' || *last == b'\t' {
            s = rest;
        } else {
            break;
        }
    }
    s
}

/// Case-insensitive ASCII byte-slice equality.
fn eq_ascii_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Parse a non-negative decimal integer. Returns `None` for an empty slice, any
/// non-digit byte (including sign characters), or on `u64` overflow.
fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut acc: u64 = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(acc)
}
