//! Native package installer (MVP): a `.ppkg` downloaded over plain HTTP,
//! verified against the repository index, then unpacked under `/mnt`.
//!
//! The wire format is specified in `pagh-programs/docs/package-format.md`; the
//! shell's `pkg` command is the only caller. This is deliberately not apt:
//!
//! * one flat `index` fetched over **plain HTTP/1.0** (the TCP client in
//!   `src/net/tcp.rs`), one ELF payload per package;
//! * no dependency solver, no Debian metadata, no compression, no OpenPGP;
//! * no TLS, so the trust anchor for now is the index's size + SHA-256 pair,
//!   which is integrity, not authentication — a repository that lies is not
//!   detected by this design, and the docs say so.
//!
//! # The safety order
//!
//! 1. `/mnt` must be a mounted directory, otherwise every install/remove/list
//!    path refuses with one line.
//! 2. `pkg install <name>` streams the package to `/mnt/pkg/<name>.ppkg` in
//!    4 KiB chunks, hashing the same bytes as they are written. The byte count
//!    may never exceed the index's `size`, and the final count + SHA-256 must
//!    match it, or the download is removed and nothing else is touched.
//! 3. Only after that gate is passed is the `PPKG0001` header read and the
//!    manifest parsed. The payload is copied from the package file to
//!    `/mnt/bin/<name>` (or `/mnt/bin/paghbox`), never held in a heap buffer.
//! 4. `/mnt/pkg/<name>.manifest` is written last, so a package with a manifest
//!    is a package that reached its last step.
//!
//! A digest or size mismatch therefore leaves no file under `/mnt/bin`; a
//! failure after the gate removes the target it was writing.
//!
//! # Storage
//!
//! The index is at most 64 entries of fixed-size arrays (`name[32]`,
//! `version[16]`, `sha[65]`, `size`, `url[96]`) in a static, and the parser
//! feeds lines into it as they arrive — no whole-index `Vec`. Paths under
//! `/mnt` are reached through `Arc<dyn VfsNode>` handles, so the installer adds
//! no path strings of its own.

use alloc::sync::Arc;
use core::fmt;

use crate::net::tcp;
use crate::net::wire::{parse_ipv4_str, Ipv4};
use crate::pkg::sha256::Sha256;
use crate::sync::Spinlock;
use crate::vfs::{self, VfsError, VfsNode};

pub mod sha256;

// ── fixed capacities (the format's, not an implementation detail) ──────────

/// Repository URL, stored verbatim (`pkg repo`). 160 bytes is the format's
/// limit and covers `http://10.0.2.2:8000/` many times over.
const REPO_CAPACITY: usize = 160;
/// Packages parsed from one index.
const MAX_PACKAGES: usize = 64;
const NAME_CAPACITY: usize = 32;
const VERSION_CAPACITY: usize = 16;
const SHA_CAPACITY: usize = 65;
const URL_CAPACITY: usize = 96;
/// Longest index line the parser accepts. The five stored fields occupy about
/// 250 bytes; the `kind`/`commands` columns are not stored, but a bundle can
/// list many commands, so the line buffer is generous (it is stack, not heap).
const INDEX_LINE_CAPACITY: usize = 2048;
/// Manifest bytes read into one stack buffer. The format has no explicit
/// ceiling; 2 KiB is generous for the keys the installer reads.
const MANIFEST_CAPACITY: usize = 2048;
const COMMANDS_CAPACITY: usize = 512;
/// Command names honoured from one `applet-bundle` manifest.
const MAX_COMMANDS: usize = 32;
/// A stored manifest is a small text file under `/mnt/pkg`.
const STORED_MANIFEST_CAPACITY: usize = 512;
/// Stream chunk: the same 4 KiB the shell's `cat`/`cp` use and the TCP client's
/// receive buffer holds.
const IO_CHUNK: usize = 4096;
/// Largest payload the installer will copy out of a package. `run` refuses an
/// ELF above 1 MiB, so 8 MiB leaves room for a future applet bundle without
/// letting a hostile header ask for an unbounded copy.
const MAX_PAYLOAD: u32 = 8 * 1024 * 1024;
/// `<repo path>` + `<index url>` after joining.
const PATH_CAPACITY: usize = 256;
const REQUEST_CAPACITY: usize = 320;
const HOST_CAPACITY: usize = 24;

const DEFAULT_REPO: &str = "http://10.0.2.2:8000/";

// ── public entry point (the shell's `pkg`) ─────────────────────────────────

/// Run one `pkg` subcommand. Errors are printed as one line and never unwind:
/// the shell treats this like any other command.
pub fn command<'a>(mut args: impl Iterator<Item = &'a str>) {
    let Some(subcommand) = args.next() else {
        usage();
        return;
    };
    match subcommand {
        "repo" => cmd_repo(args.next()),
        "index" => cmd_index(),
        "list" => cmd_list(),
        "install" => match args.next() {
            Some(name) => cmd_install(name),
            None => crate::kprintln!("pkg: usage: pkg install <name>"),
        },
        "remove" => match args.next() {
            Some(name) => cmd_remove(name),
            None => crate::kprintln!("pkg: usage: pkg remove <name>"),
        },
        "installed" => cmd_installed(),
        // The SHA-256 runtime known answers. Kept out of `help`: it is a
        // diagnostic, not a package operation, but it is how the in-QEMU check
        // reaches the same vectors as the cfg(test) unit test.
        "selftest" => {
            if sha256::selftest() {
                crate::kprintln!("pkg: sha256 selftest ok");
            } else {
                crate::kprintln!("pkg: sha256 selftest FAILED");
            }
        }
        other => crate::kprintln!("pkg: unknown subcommand '{}' (try 'pkg')", other),
    }
}

fn usage() {
    crate::kprintln!("pkg: usage:");
    crate::kprintln!("  pkg repo [URL]       show or set the repository (default {DEFAULT_REPO})");
    crate::kprintln!("  pkg index            fetch and cache <repo>/index over plain HTTP");
    crate::kprintln!("  pkg list             list index packages; [installed] marks /mnt ones");
    crate::kprintln!("  pkg install <name>   download, verify and install a package");
    crate::kprintln!("  pkg remove <name>    remove an installed package");
    crate::kprintln!("  pkg installed        list packages installed under /mnt");
}

// ── errors ─────────────────────────────────────────────────────────────────

/// Why a package operation failed. One variant per user-visible sentence; the
/// shell prints [`Display`] and returns to the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// `/mnt` is not a mounted directory.
    NotMounted,
    /// `pkg index` has not succeeded in this boot (or it failed).
    NoIndex,
    /// The name is not in the cached index.
    UnknownPackage,
    /// The name cannot be a package/file name (empty, too long, `/`, `.`).
    InvalidName,
    /// The URL is not `http://IP[:port]/path`.
    BadUrl,
    /// A scheme other than `http://` (including `https://`).
    UnsupportedScheme,
    /// An authority that is not a numeric IPv4 address (no DNS in this MVP).
    UnsupportedHost,
    /// The index line or its header is malformed.
    BadIndex,
    /// The HTTP status was not 200.
    Http(u16),
    /// The response did not have a complete status line + header block.
    BadResponse,
    /// More body bytes arrived than the index's size allows.
    TooLarge,
    /// The final byte count differs from the index's size.
    SizeMismatch,
    /// The streamed SHA-256 differs from the index's digest.
    DigestMismatch,
    /// The first eight bytes are not `PPKG0001`.
    BadMagic,
    /// The header's lengths do not add up to the package size, or a read came
    /// back short.
    BadPackage,
    /// The manifest is not UTF-8, misses a required key, or names another
    /// package than the one requested.
    BadManifest,
    /// The package is not installed (no stored manifest).
    NotInstalled,
    /// A VFS/filesystem operation failed.
    Vfs(VfsError),
    /// The network client failed (no lease, timeout, reset, ...).
    Network(tcp::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotMounted => f.write_str("/mnt is not mounted"),
            Error::NoIndex => f.write_str("no index cached (run 'pkg index')"),
            Error::UnknownPackage => f.write_str("not in the index"),
            Error::InvalidName => f.write_str("invalid package name"),
            Error::BadUrl => f.write_str("expected http://IP[:port]/path"),
            Error::UnsupportedScheme => {
                f.write_str("only plain http:// is supported (there is no TLS yet)")
            }
            Error::UnsupportedHost => {
                f.write_str("only numeric IPv4 hosts are supported (no DNS yet)")
            }
            Error::BadIndex => f.write_str("malformed index"),
            Error::Http(status) => write!(f, "server returned HTTP status {status}"),
            Error::BadResponse => f.write_str("malformed HTTP response"),
            Error::TooLarge => f.write_str("response is larger than the index entry"),
            Error::SizeMismatch => f.write_str("size does not match the index"),
            Error::DigestMismatch => f.write_str("SHA-256 does not match the index"),
            Error::BadMagic => f.write_str("not a .ppkg (bad magic)"),
            Error::BadPackage => f.write_str("package header is inconsistent"),
            Error::BadManifest => f.write_str("package manifest is invalid"),
            Error::NotInstalled => f.write_str("is not installed"),
            Error::Vfs(error) => f.write_str(error.as_str()),
            Error::Network(error) => write!(f, "{error}"),
        }
    }
}

impl From<VfsError> for Error {
    fn from(error: VfsError) -> Self {
        Error::Vfs(error)
    }
}

/// One error line, with the literal `/mnt` sentence the docs and tests grep.
fn report(operation: &str, error: Error) {
    match error {
        Error::NotMounted => crate::kprintln!("pkg: /mnt is not mounted"),
        other => crate::kprintln!("pkg: {}: {}", operation, other),
    }
}

// ── repository URL and HTTP ────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Repo {
    text: [u8; REPO_CAPACITY],
    len: usize,
}

impl Repo {
    const DEFAULT: Repo = Repo::from_str(DEFAULT_REPO);

    /// Copy a compile-time string into the fixed buffer (const-compatible).
    const fn from_str(text: &str) -> Repo {
        let bytes = text.as_bytes();
        let mut buffer = [0u8; REPO_CAPACITY];
        let mut index = 0;
        while index < bytes.len() {
            buffer[index] = bytes[index];
            index += 1;
        }
        Repo {
            text: buffer,
            len: bytes.len(),
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.text[..self.len]).unwrap_or("")
    }

    fn set(&mut self, text: &str) -> bool {
        let bytes = text.as_bytes();
        if bytes.len() > self.text.len() {
            return false;
        }
        self.text[..bytes.len()].copy_from_slice(bytes);
        self.len = bytes.len();
        true
    }
}

static REPO: Spinlock<Repo> = Spinlock::new(Repo::DEFAULT);

/// A parsed repository URL: who to connect to, what `Host` to send, and the
/// base path package URLs are relative to.
#[derive(Clone, Copy)]
struct Endpoint {
    ip: Ipv4,
    port: u16,
    host: [u8; HOST_CAPACITY],
    host_len: usize,
    base: [u8; PATH_CAPACITY],
    base_len: usize,
}

impl Endpoint {
    fn host(&self) -> &str {
        core::str::from_utf8(&self.host[..self.host_len]).unwrap_or("")
    }
}

/// Parse `http://IP[:port][/path]`. Only the shapes the MVP promises: a scheme,
/// a numeric IPv4 authority (optional port) and an optional path that is
/// normalised to a trailing `/`. `https://` is refused by name so the caller
/// never mistakes a failed download for a broken kernel.
fn parse_repo(url: &str) -> Result<Endpoint, Error> {
    let rest = url
        .strip_prefix("http://")
        .ok_or(Error::UnsupportedScheme)?;
    if rest.is_empty() {
        return Err(Error::BadUrl);
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(Error::BadUrl);
    }
    // The MVP is IPv4-only, so the first ':' separates the host from the port;
    // there are no bracketed IPv6 literals to get wrong.
    let (host, port) = match authority.find(':') {
        Some(index) => (&authority[..index], &authority[index + 1..]),
        None => (authority, "80"),
    };
    let ip = parse_ipv4_str(host).ok_or(Error::UnsupportedHost)?;
    let port: u16 = port.parse().map_err(|_| Error::BadUrl)?;
    if port == 0 {
        return Err(Error::BadUrl);
    }

    let mut endpoint = Endpoint {
        ip,
        port,
        host: [0; HOST_CAPACITY],
        host_len: 0,
        base: [0; PATH_CAPACITY],
        base_len: 0,
    };
    if authority.len() > endpoint.host.len() {
        return Err(Error::BadUrl);
    }
    endpoint.host[..authority.len()].copy_from_slice(authority.as_bytes());
    endpoint.host_len = authority.len();

    if path.len() >= endpoint.base.len() {
        return Err(Error::BadUrl);
    }
    endpoint.base[..path.len()].copy_from_slice(path.as_bytes());
    endpoint.base_len = path.len();
    if !path.ends_with('/') {
        endpoint.base[endpoint.base_len] = b'/';
        endpoint.base_len += 1;
    }
    Ok(endpoint)
}

/// Join the endpoint's base path and `relative` into one request path. A leading
/// `/` in `relative` is stripped on purpose: package URLs are repository-
/// relative, not host-absolute (`pkg/hello-1.0.0.ppkg`).
fn request_path(
    endpoint: &Endpoint,
    relative: &str,
    out: &mut [u8; PATH_CAPACITY],
) -> Result<usize, Error> {
    let mut length = endpoint.base_len;
    out[..length].copy_from_slice(&endpoint.base[..length]);
    let relative = relative.trim_start_matches('/');
    let bytes = relative.as_bytes();
    if length + bytes.len() >= out.len() {
        return Err(Error::BadUrl);
    }
    out[length..length + bytes.len()].copy_from_slice(bytes);
    length += bytes.len();
    core::str::from_utf8(&out[..length]).map_err(|_| Error::BadUrl)?;
    Ok(length)
}

fn append(out: &mut [u8], length: &mut usize, bytes: &[u8]) -> Result<(), Error> {
    if *length + bytes.len() > out.len() {
        return Err(Error::BadUrl);
    }
    out[*length..*length + bytes.len()].copy_from_slice(bytes);
    *length += bytes.len();
    Ok(())
}

fn build_request(
    endpoint: &Endpoint,
    path: &str,
    out: &mut [u8; REQUEST_CAPACITY],
) -> Result<usize, Error> {
    let mut length = 0;
    append(out, &mut length, b"GET ")?;
    append(out, &mut length, path.as_bytes())?;
    append(out, &mut length, b" HTTP/1.0\r\nHost: ")?;
    append(out, &mut length, endpoint.host().as_bytes())?;
    append(out, &mut length, b"\r\nConnection: close\r\n\r\n")?;
    Ok(length)
}

/// The HTTP response head, parsed as bytes arrive. Only two things matter:
/// the status line says `200`, and the body starts after the first empty line.
/// Fragmenting across `recv` chunks must not change either answer, so the
/// last four bytes are tracked in a window rather than re-scanning a buffer.
struct ResponseHead {
    status: [u8; 16],
    status_len: usize,
    line_done: bool,
    window: [u8; 4],
    window_len: usize,
    body_started: bool,
}

impl ResponseHead {
    fn new() -> Self {
        ResponseHead {
            status: [0; 16],
            status_len: 0,
            line_done: false,
            window: [0; 4],
            window_len: 0,
            body_started: false,
        }
    }

    /// Feed one chunk. Once the head is complete the rest (and every later
    /// chunk) goes to `sink`; before that, nothing is handed to the caller.
    fn feed<F>(&mut self, chunk: &[u8], sink: &mut F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> Result<(), Error>,
    {
        if self.body_started {
            return sink(chunk);
        }
        let mut body = chunk.len();
        for (index, byte) in chunk.iter().enumerate() {
            if !self.line_done {
                if *byte == b'\n' {
                    self.line_done = true;
                } else if *byte != b'\r' && self.status_len < self.status.len() {
                    self.status[self.status_len] = *byte;
                    self.status_len += 1;
                }
            }
            if self.window_len < 4 {
                self.window[self.window_len] = *byte;
                self.window_len += 1;
            } else {
                self.window[0] = self.window[1];
                self.window[1] = self.window[2];
                self.window[2] = self.window[3];
                self.window[3] = *byte;
            }
            if self.window_len == 4 && self.window == *b"\r\n\r\n" {
                self.body_started = true;
                body = index + 1;
                break;
            }
        }
        if !self.body_started {
            return Ok(());
        }
        if !self.line_done {
            return Err(Error::BadResponse);
        }
        let status = parse_status(&self.status[..self.status_len]).ok_or(Error::BadResponse)?;
        if status != 200 {
            return Err(Error::Http(status));
        }
        sink(&chunk[body..])
    }
}

/// `HTTP/1.x <code> ...` -> the three-digit code. Strict about the shape: a
/// response that is not a status line is a malformed response, not a fallback.
fn parse_status(line: &[u8]) -> Option<u16> {
    if line.len() < 12 || !line.starts_with(b"HTTP/") {
        return None;
    }
    let space = line.iter().position(|byte| *byte == b' ')?;
    let code = line.get(space + 1..space + 4)?;
    if !code.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut value = 0u16;
    for byte in code {
        value = value * 10 + u16::from(byte - b'0');
    }
    Some(value)
}

/// GET `path` from the repository and hand the body to `sink` in pieces.
///
/// The caller owns the sink's semantics (hash + write, or parse lines); this
/// function owns the connection lifecycle: every path closes the socket, and a
/// failed step is returned instead of retried.
fn http_get<F>(endpoint: &Endpoint, path: &str, mut sink: F) -> Result<(), Error>
where
    F: FnMut(&[u8]) -> Result<(), Error>,
{
    let mut request = [0u8; REQUEST_CAPACITY];
    let request_len = build_request(endpoint, path, &mut request)?;

    tcp::connect(endpoint.ip, endpoint.port).map_err(Error::Network)?;
    let result = exchange(&request[..request_len], &mut sink);
    // `close` is safe even after a reset (it clears the socket); the FIN only
    // leaves when the connection is still established.
    tcp::close();
    result
}

fn exchange<F>(request: &[u8], sink: &mut F) -> Result<(), Error>
where
    F: FnMut(&[u8]) -> Result<(), Error>,
{
    tcp::send(request).map_err(Error::Network)?;
    let mut head = ResponseHead::new();
    let mut buffer = [0u8; IO_CHUNK];
    loop {
        match tcp::recv(&mut buffer) {
            Ok(0) => break,
            Ok(read) => head.feed(&buffer[..read], sink)?,
            Err(error) => return Err(Error::Network(error)),
        }
    }
    if !head.body_started {
        return Err(Error::BadResponse);
    }
    Ok(())
}

// ── the index ──────────────────────────────────────────────────────────────

/// One index line, in fixed arrays. `sha` keeps the 64 hex characters and a NUL
/// slot (`sha[65]` in the format), so comparisons are byte comparisons.
#[derive(Clone, Copy)]
struct Entry {
    name: [u8; NAME_CAPACITY],
    name_len: u8,
    version: [u8; VERSION_CAPACITY],
    version_len: u8,
    sha: [u8; SHA_CAPACITY],
    size: u32,
    url: [u8; URL_CAPACITY],
    url_len: u8,
}

impl Entry {
    const EMPTY: Entry = Entry {
        name: [0; NAME_CAPACITY],
        name_len: 0,
        version: [0; VERSION_CAPACITY],
        version_len: 0,
        sha: [0; SHA_CAPACITY],
        size: 0,
        url: [0; URL_CAPACITY],
        url_len: 0,
    };

    fn name(&self) -> &str {
        text(&self.name[..self.name_len as usize])
    }

    fn version(&self) -> &str {
        text(&self.version[..self.version_len as usize])
    }

    fn sha(&self) -> &str {
        text(&self.sha[..64])
    }

    fn url(&self) -> &str {
        text(&self.url[..self.url_len as usize])
    }
}

struct Index {
    loaded: bool,
    count: usize,
    entries: [Entry; MAX_PACKAGES],
}

impl Index {
    const EMPTY: Index = Index {
        loaded: false,
        count: 0,
        entries: [Entry::EMPTY; MAX_PACKAGES],
    };
}

static INDEX: Spinlock<Index> = Spinlock::new(Index::EMPTY);

fn text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("")
}

fn copy_field(dst: &mut [u8], src: &[u8]) -> Result<u8, Error> {
    if src.len() > dst.len() {
        return Err(Error::BadIndex);
    }
    dst[..src.len()].copy_from_slice(src);
    Ok(src.len() as u8)
}

/// Parse one index line into an [`Entry`]. The `kind` and `commands` columns are
/// validated for shape only: the manifest, not the index, is what the installer
/// acts on, and the digest gate is what makes the manifest trustworthy.
fn parse_entry(line: &[u8]) -> Result<Entry, Error> {
    let mut fields = line.split(|byte| *byte == b'\t');
    let name = fields.next().ok_or(Error::BadIndex)?;
    let version = fields.next().ok_or(Error::BadIndex)?;
    let sha = fields.next().ok_or(Error::BadIndex)?;
    let size = fields.next().ok_or(Error::BadIndex)?;
    let _kind = fields.next().ok_or(Error::BadIndex)?;
    let _commands = fields.next().ok_or(Error::BadIndex)?;
    let url = fields.next().ok_or(Error::BadIndex)?;
    if fields.next().is_some() {
        return Err(Error::BadIndex);
    }
    if name.is_empty() || version.is_empty() || url.is_empty() || sha.len() != 64 {
        return Err(Error::BadIndex);
    }
    if !sha.iter().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::BadIndex);
    }
    if core::str::from_utf8(name).is_err() || core::str::from_utf8(url).is_err() {
        return Err(Error::BadIndex);
    }
    if name.contains(&b'/') || name == b"." || name == b".." {
        return Err(Error::BadIndex);
    }
    let size = text(size).parse::<u32>().map_err(|_| Error::BadIndex)?;

    let mut entry = Entry::EMPTY;
    entry.name_len = copy_field(&mut entry.name, name)?;
    entry.version_len = copy_field(&mut entry.version, version)?;
    entry.sha[..64].copy_from_slice(sha);
    entry.url_len = copy_field(&mut entry.url, url)?;
    entry.size = size;
    Ok(entry)
}

/// Streaming line parser: the index never exists as a whole buffer.
struct IndexParser {
    line: [u8; INDEX_LINE_CAPACITY],
    length: usize,
    header_seen: bool,
}

impl IndexParser {
    fn new() -> Self {
        IndexParser {
            line: [0; INDEX_LINE_CAPACITY],
            length: 0,
            header_seen: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<(), Error> {
        for byte in bytes {
            if *byte == b'\n' {
                self.end_line()?;
            } else {
                if self.length == self.line.len() {
                    return Err(Error::BadIndex);
                }
                self.line[self.length] = *byte;
                self.length += 1;
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), Error> {
        if self.length > 0 {
            self.end_line()?;
        }
        if !self.header_seen {
            return Err(Error::BadIndex);
        }
        Ok(())
    }

    fn end_line(&mut self) -> Result<(), Error> {
        let mut line = &self.line[..self.length];
        self.length = 0;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() {
            return Ok(());
        }
        if !self.header_seen {
            if line != b"PPKGINDEX 1" {
                return Err(Error::BadIndex);
            }
            self.header_seen = true;
            return Ok(());
        }
        let entry = parse_entry(line)?;
        let mut index = INDEX.lock();
        // "at most 64": extra lines are ignored rather than growing anything.
        let slot = index.count;
        if slot < MAX_PACKAGES {
            index.entries[slot] = entry;
            index.count = slot + 1;
        }
        Ok(())
    }
}

fn cached_entry(name: &str) -> Result<Entry, Error> {
    let index = INDEX.lock();
    if !index.loaded {
        return Err(Error::NoIndex);
    }
    for entry in &index.entries[..index.count] {
        if entry.name() == name {
            return Ok(*entry);
        }
    }
    Err(Error::UnknownPackage)
}

// ── commands ───────────────────────────────────────────────────────────────

fn cmd_repo(argument: Option<&str>) {
    let Some(url) = argument else {
        let repo = *REPO.lock();
        crate::kprintln!("pkg: repo {}", repo.as_str());
        return;
    };
    if url.len() > REPO_CAPACITY {
        crate::kprintln!("pkg: repo: URL is too long ({} bytes max)", REPO_CAPACITY);
        return;
    }
    if let Err(error) = parse_repo(url) {
        report("repo", error);
        return;
    }
    {
        let mut repo = REPO.lock();
        if !repo.set(url) {
            drop(repo);
            report("repo", Error::BadUrl);
            return;
        }
    }
    let repo = *REPO.lock();
    crate::kprintln!("pkg: repo {}", repo.as_str());
}

fn cmd_index() {
    // Copy the repository under a short lock: the HTTP exchange must not run
    // with a spinlock held.
    let repo = *REPO.lock();
    let endpoint = match parse_repo(repo.as_str()) {
        Ok(endpoint) => endpoint,
        Err(error) => return report("index", error),
    };

    // A failed refresh must not leave half of the old index looking valid.
    {
        let mut index = INDEX.lock();
        index.loaded = false;
        index.count = 0;
    }

    // The index lives at `<repo>/index`, so it goes through the same path join
    // as a package URL: a repository served under a prefix must work too.
    let mut path = [0u8; PATH_CAPACITY];
    let path_len = match request_path(&endpoint, "index", &mut path) {
        Ok(length) => length,
        Err(error) => return report("index", error),
    };
    let path = text(&path[..path_len]);

    let mut parser = IndexParser::new();
    let result =
        http_get(&endpoint, path, |chunk| parser.feed(chunk)).and_then(|()| parser.finish());
    match result {
        Ok(()) => {
            let count = {
                let mut index = INDEX.lock();
                index.loaded = true;
                index.count
            };
            crate::kprintln!("pkg: index {} package(s)", count);
        }
        Err(error) => {
            let mut index = INDEX.lock();
            index.loaded = false;
            index.count = 0;
            report("index", error);
        }
    }
}

fn cmd_list() {
    let mount = match mount_node() {
        Ok(node) => node,
        Err(error) => return report("list", error),
    };
    let loaded = {
        let index = INDEX.lock();
        if !index.loaded {
            return report("list", Error::NoIndex);
        }
        index.count
    };
    if loaded == 0 {
        crate::kprintln!("pkg: 0 package(s) in the index");
        return;
    }
    let packages = match mount.lookup("pkg") {
        Ok(node) => Some(node),
        Err(_) => None,
    };
    let mut name_buffer = [0u8; 48];
    for index in 0..loaded {
        let entry = {
            let table = INDEX.lock();
            table.entries[index]
        };
        let installed = match &packages {
            Some(directory) => match manifest_file_name(entry.name(), &mut name_buffer) {
                Ok(name) => directory.lookup(name).is_ok(),
                Err(_) => false,
            },
            None => false,
        };
        if installed {
            crate::kprintln!(
                "{} {} {} B [installed]",
                entry.name(),
                entry.version(),
                entry.size
            );
        } else {
            crate::kprintln!("{} {} {} B", entry.name(), entry.version(), entry.size);
        }
    }
}

fn cmd_install(name: &str) {
    match install(name) {
        Ok(()) => {}
        Err(error) => report("install", error),
    }
}

fn install(name: &str) -> Result<(), Error> {
    if !valid_name(name) {
        return Err(Error::InvalidName);
    }
    let mount = mount_node()?;
    let entry = cached_entry(name)?;
    // The repository can change between `pkg index` and `pkg install`; the URL
    // is read fresh so `pkg repo` is honoured without re-fetching the index.
    let repo = *REPO.lock();
    let endpoint = parse_repo(repo.as_str())?;

    let package_dir = ensure_dir(&mount, "pkg")?;
    let bin_dir = ensure_dir(&mount, "bin")?;

    let mut package_name = [0u8; 48];
    let package_name = suffixed_name(name, ".ppkg", &mut package_name)?;
    let package = open_for_write(&package_dir, package_name)?;

    let mut path = [0u8; PATH_CAPACITY];
    let path_len = request_path(&endpoint, entry.url(), &mut path)?;
    let path = text(&path[..path_len]);

    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let downloaded = http_get(&endpoint, path, |chunk| {
        let chunk_len = chunk.len() as u64;
        if written + chunk_len > entry.size as u64 {
            return Err(Error::TooLarge);
        }
        let mut offset = 0usize;
        while offset < chunk.len() {
            let wrote = package
                .write(written + offset as u64, &chunk[offset..])
                .map_err(Error::Vfs)?;
            if wrote == 0 {
                return Err(Error::Vfs(VfsError::IoError));
            }
            offset += wrote;
        }
        hasher.update(chunk);
        written += chunk_len;
        Ok(())
    });
    if let Err(error) = downloaded {
        let _ = package_dir.remove(package_name);
        return Err(error);
    }
    if written != entry.size as u64 {
        let _ = package_dir.remove(package_name);
        return Err(Error::SizeMismatch);
    }

    let digest = hasher.finalize();
    let mut hex = [0u8; 64];
    sha256::hex(&digest, &mut hex);
    if entry.sha().as_bytes() != &hex[..] {
        let _ = package_dir.remove(package_name);
        return Err(Error::DigestMismatch);
    }

    // The file on disk is the package the index described; parse and install
    // from it. Anything that fails from here on leaves the `.ppkg` removed and
    // the target under `/mnt/bin` cleaned up by `install_verified`.
    let package = match package_dir.lookup(package_name) {
        Ok(node) => node,
        Err(error) => {
            let _ = package_dir.remove(package_name);
            return Err(Error::Vfs(error));
        }
    };
    let result = install_verified(&entry, &package, &package_dir, &bin_dir);
    if result.is_err() {
        let _ = package_dir.remove(package_name);
    }
    result
}

/// Install from a size/digest-verified package file.
fn install_verified(
    entry: &Entry,
    package: &Arc<dyn VfsNode>,
    package_dir: &Arc<dyn VfsNode>,
    bin_dir: &Arc<dyn VfsNode>,
) -> Result<(), Error> {
    let mut header = [0u8; 16];
    read_exact(package, 0, &mut header)?;
    if &header[..8] != b"PPKG0001" {
        return Err(Error::BadMagic);
    }
    let manifest_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    let payload_len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    if manifest_len as usize > MANIFEST_CAPACITY || payload_len > MAX_PAYLOAD {
        return Err(Error::BadPackage);
    }
    let total = 16u64
        .checked_add(manifest_len as u64)
        .and_then(|value| value.checked_add(payload_len as u64))
        .ok_or(Error::BadPackage)?;
    if total != entry.size as u64 || package.size() != entry.size as u64 {
        return Err(Error::BadPackage);
    }

    let mut manifest_bytes = [0u8; MANIFEST_CAPACITY];
    let manifest_bytes = &mut manifest_bytes[..manifest_len as usize];
    read_exact(package, 16, manifest_bytes)?;
    let manifest = Manifest::parse(manifest_bytes)?;
    if manifest.name() != entry.name() || !valid_name(manifest.name()) {
        return Err(Error::BadManifest);
    }

    let payload_offset = 16 + manifest_len as u64;
    let result = match manifest.kind {
        Kind::App => install_app(&manifest, package, payload_offset, payload_len, bin_dir),
        Kind::AppletBundle => {
            install_bundle(&manifest, package, payload_offset, payload_len, bin_dir)
        }
    };
    if result.is_err() {
        // A failed install must not leave a half-written program behind. The
        // package file itself is removed by the caller.
        let _ = bin_dir.remove(manifest.name());
        if manifest.kind == Kind::AppletBundle {
            remove_commands(&manifest, bin_dir);
            let _ = bin_dir.remove("paghbox");
        }
    }
    result?;

    let mut stored = [0u8; STORED_MANIFEST_CAPACITY];
    let stored_len = manifest.render(&mut stored)?;
    let mut file_name = [0u8; 48];
    let file_name = manifest_file_name(manifest.name(), &mut file_name)?;
    let node = open_for_write(package_dir, file_name)?;
    write_all(&node, 0, &stored[..stored_len])?;
    crate::kprintln!("pkg: installed {} {}", manifest.name(), manifest.version());
    Ok(())
}

fn install_app(
    manifest: &Manifest,
    package: &Arc<dyn VfsNode>,
    payload_offset: u64,
    payload_len: u32,
    bin_dir: &Arc<dyn VfsNode>,
) -> Result<(), Error> {
    let target = open_for_write(bin_dir, manifest.name())?;
    copy_payload(package, payload_offset, payload_len as u64, &target)?;
    if target.size() != payload_len as u64 {
        return Err(Error::BadPackage);
    }
    Ok(())
}

fn install_bundle(
    manifest: &Manifest,
    package: &Arc<dyn VfsNode>,
    payload_offset: u64,
    payload_len: u32,
    bin_dir: &Arc<dyn VfsNode>,
) -> Result<(), Error> {
    let target = open_for_write(bin_dir, "paghbox")?;
    copy_payload(package, payload_offset, payload_len as u64, &target)?;
    if target.size() != payload_len as u64 {
        return Err(Error::BadPackage);
    }
    let mut count = 0;
    for command in manifest.commands() {
        count += 1;
        // `paghbox` is the bundle's own payload; a link with that name would
        // replace the program with a self-referential symlink.
        if count > MAX_COMMANDS || !valid_name(command) || command == "paghbox" {
            return Err(Error::BadManifest);
        }
        // Reinstalling replaces the old link instead of failing on it.
        let _ = bin_dir.remove(command);
        bin_dir
            .create_symlink(command, b"paghbox")
            .map_err(Error::Vfs)?;
    }
    if count == 0 {
        return Err(Error::BadManifest);
    }
    Ok(())
}

// ── remove / installed ────────────────────────────────────────────────────

fn cmd_remove(name: &str) {
    match remove(name) {
        Ok(()) => {}
        Err(error) => report("remove", error),
    }
}

fn remove(name: &str) -> Result<(), Error> {
    if !valid_name(name) {
        return Err(Error::InvalidName);
    }
    let mount = mount_node()?;
    let package_dir = match mount.lookup("pkg") {
        Ok(node) => node,
        Err(VfsError::NotFound) => return Err(Error::NotInstalled),
        Err(error) => return Err(Error::Vfs(error)),
    };
    let mut file_name = [0u8; 48];
    let file_name = manifest_file_name(name, &mut file_name)?;
    let manifest_node = match package_dir.lookup(file_name) {
        Ok(node) => node,
        Err(VfsError::NotFound) => return Err(Error::NotInstalled),
        Err(error) => return Err(Error::Vfs(error)),
    };
    let mut buffer = [0u8; STORED_MANIFEST_CAPACITY];
    let length = read_node(&manifest_node, &mut buffer)?;
    let manifest = Manifest::parse(&buffer[..length])?;
    if manifest.name() != name {
        return Err(Error::BadManifest);
    }

    let bin_dir = ensure_dir(&mount, "bin")?;
    match manifest.kind {
        Kind::App => remove_if_present(&bin_dir, manifest.name())?,
        Kind::AppletBundle => {
            remove_commands(&manifest, &bin_dir);
            remove_if_present(&bin_dir, "paghbox")?;
        }
    }
    // Then the cached `.ppkg`, and the manifest last: a manifest that still
    // exists is an install that still exists.
    let mut package_name = [0u8; 48];
    let package_name = suffixed_name(manifest.name(), ".ppkg", &mut package_name)?;
    remove_if_present(&package_dir, package_name)?;
    remove_if_present(&package_dir, file_name)?;
    crate::kprintln!("pkg: removed {}", manifest.name());
    Ok(())
}

fn cmd_installed() {
    match installed() {
        Ok(()) => {}
        Err(error) => report("installed", error),
    }
}

fn installed() -> Result<(), Error> {
    let mount = mount_node()?;
    let package_dir = match mount.lookup("pkg") {
        Ok(node) => node,
        Err(VfsError::NotFound) => {
            crate::kprintln!("pkg: 0 package(s) installed");
            return Ok(());
        }
        Err(error) => return Err(Error::Vfs(error)),
    };
    let children = package_dir.readdir()?;
    let mut count = 0usize;
    for child in children.iter() {
        if child.is_directory() {
            continue;
        }
        let Some(name) = child.name().strip_suffix(".manifest") else {
            continue;
        };
        let mut buffer = [0u8; STORED_MANIFEST_CAPACITY];
        match read_node(child, &mut buffer) {
            Ok(length) => match Manifest::parse(&buffer[..length]) {
                Ok(manifest) => {
                    count += 1;
                    crate::kprintln!("pkg: {} {}", manifest.name(), manifest.version());
                }
                Err(_) => crate::kprintln!("pkg: {}: bad manifest, skipped", name),
            },
            Err(error) => crate::kprintln!("pkg: {}: {}, skipped", name, error),
        }
    }
    crate::kprintln!("pkg: {} package(s) installed", count);
    Ok(())
}

fn remove_commands(manifest: &Manifest, bin_dir: &Arc<dyn VfsNode>) {
    let mut count = 0;
    for command in manifest.commands() {
        count += 1;
        if count > MAX_COMMANDS || !valid_name(command) {
            break;
        }
        let _ = bin_dir.remove(command);
    }
}

fn remove_if_present(dir: &Arc<dyn VfsNode>, name: &str) -> Result<(), Error> {
    match dir.remove(name) {
        Ok(()) => Ok(()),
        Err(VfsError::NotFound) => Ok(()),
        Err(error) => Err(Error::Vfs(error)),
    }
}

// ── the manifest ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    App,
    AppletBundle,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::App => "app",
            Kind::AppletBundle => "applet-bundle",
        }
    }
}

/// The five manifest keys the installer reads; `arch`, `entry`, `description`
/// and the payload's own `sha256` are informational and ignored here.
#[derive(Clone, Copy)]
struct Manifest {
    name: [u8; NAME_CAPACITY],
    name_len: usize,
    version: [u8; VERSION_CAPACITY],
    version_len: usize,
    kind: Kind,
    commands: [u8; COMMANDS_CAPACITY],
    commands_len: usize,
}

impl Manifest {
    const EMPTY: Manifest = Manifest {
        name: [0; NAME_CAPACITY],
        name_len: 0,
        version: [0; VERSION_CAPACITY],
        version_len: 0,
        kind: Kind::App,
        commands: [0; COMMANDS_CAPACITY],
        commands_len: 0,
    };

    fn parse(bytes: &[u8]) -> Result<Manifest, Error> {
        let text = core::str::from_utf8(bytes).map_err(|_| Error::BadManifest)?;
        let mut manifest = Manifest::EMPTY;
        let mut kind_seen = false;
        for line in text.split('\n') {
            let line = line.trim_end_matches('\r').trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some(separator) = line.find('=') else {
                continue;
            };
            let key = line[..separator].trim();
            let value = line[separator + 1..].trim();
            match key {
                "name" => manifest.name_len = store(&mut manifest.name, value)?,
                "version" => manifest.version_len = store(&mut manifest.version, value)?,
                "kind" => {
                    manifest.kind = match value {
                        "app" => Kind::App,
                        "applet-bundle" => Kind::AppletBundle,
                        _ => return Err(Error::BadManifest),
                    };
                    kind_seen = true;
                }
                "commands" => manifest.commands_len = store(&mut manifest.commands, value)?,
                // Informational keys in the format: present but not acted on.
                _ => {}
            }
        }
        if manifest.name_len == 0 || manifest.version_len == 0 || !kind_seen {
            return Err(Error::BadManifest);
        }
        Ok(manifest)
    }

    fn name(&self) -> &str {
        text(&self.name[..self.name_len])
    }

    fn version(&self) -> &str {
        text(&self.version[..self.version_len])
    }

    fn commands(&self) -> impl Iterator<Item = &str> {
        self.commands_text()
            .split(',')
            .map(str::trim)
            .filter(|command| !command.is_empty())
    }

    fn commands_text(&self) -> &str {
        text(&self.commands[..self.commands_len])
    }

    /// The stored manifest: a normalized subset that `pkg remove` can act on
    /// without keeping the package file around.
    fn render(&self, out: &mut [u8; STORED_MANIFEST_CAPACITY]) -> Result<usize, Error> {
        let mut length = 0;
        append(out, &mut length, b"name=")?;
        append(out, &mut length, self.name().as_bytes())?;
        append(out, &mut length, b"\nversion=")?;
        append(out, &mut length, self.version().as_bytes())?;
        append(out, &mut length, b"\nkind=")?;
        append(out, &mut length, self.kind.as_str().as_bytes())?;
        append(out, &mut length, b"\ncommands=")?;
        append(out, &mut length, self.commands_text().as_bytes())?;
        append(out, &mut length, b"\n")?;
        Ok(length)
    }
}

fn store(dst: &mut [u8], value: &str) -> Result<usize, Error> {
    let bytes = value.as_bytes();
    if bytes.len() > dst.len() {
        return Err(Error::BadManifest);
    }
    dst[..bytes.len()].copy_from_slice(bytes);
    Ok(bytes.len())
}

// ── VFS plumbing ───────────────────────────────────────────────────────────

/// The mounted filesystem root, or the one error every install/list path uses.
fn mount_node() -> Result<Arc<dyn VfsNode>, Error> {
    match vfs::lookup_path("/mnt") {
        Ok(node) if node.is_directory() => Ok(node),
        _ => Err(Error::NotMounted),
    }
}

fn ensure_dir(parent: &Arc<dyn VfsNode>, name: &str) -> Result<Arc<dyn VfsNode>, Error> {
    match parent.lookup(name) {
        Ok(node) => {
            if node.is_directory() {
                Ok(node)
            } else {
                Err(Error::Vfs(VfsError::NotADirectory))
            }
        }
        Err(VfsError::NotFound) => parent.create_dir(name).map_err(Error::Vfs),
        Err(error) => Err(Error::Vfs(error)),
    }
}

/// Open a file for streaming: truncate an existing regular file, or create one.
fn open_for_write(dir: &Arc<dyn VfsNode>, name: &str) -> Result<Arc<dyn VfsNode>, Error> {
    match dir.lookup(name) {
        Ok(node) => {
            if node.is_directory() {
                return Err(Error::Vfs(VfsError::IsADirectory));
            }
            node.truncate(0).map_err(Error::Vfs)?;
            Ok(node)
        }
        Err(VfsError::NotFound) => dir.create_file(name).map_err(Error::Vfs),
        Err(error) => Err(Error::Vfs(error)),
    }
}

fn read_exact(node: &Arc<dyn VfsNode>, offset: u64, buffer: &mut [u8]) -> Result<(), Error> {
    let mut done = 0usize;
    while done < buffer.len() {
        match node.read(offset + done as u64, &mut buffer[done..]) {
            Ok(0) => return Err(Error::BadPackage),
            Ok(read) => done += read,
            Err(error) => return Err(Error::Vfs(error)),
        }
    }
    Ok(())
}

fn read_node(node: &Arc<dyn VfsNode>, buffer: &mut [u8]) -> Result<usize, Error> {
    let mut total = 0usize;
    loop {
        if total == buffer.len() {
            return Err(Error::BadManifest);
        }
        match node.read(total as u64, &mut buffer[total..]) {
            Ok(0) => break,
            Ok(read) => total += read,
            Err(error) => return Err(Error::Vfs(error)),
        }
    }
    Ok(total)
}

fn write_all(node: &Arc<dyn VfsNode>, offset: u64, bytes: &[u8]) -> Result<(), Error> {
    let mut done = 0usize;
    while done < bytes.len() {
        let wrote = node
            .write(offset + done as u64, &bytes[done..])
            .map_err(Error::Vfs)?;
        if wrote == 0 {
            return Err(Error::Vfs(VfsError::IoError));
        }
        done += wrote;
    }
    Ok(())
}

/// Copy `length` bytes from the verified package file to the install target.
/// The loop is bounded by the header's payload length and by `MAX_PAYLOAD`
/// (checked by the caller), so a truncated file is an error, not a short copy.
fn copy_payload(
    source: &Arc<dyn VfsNode>,
    source_offset: u64,
    length: u64,
    target: &Arc<dyn VfsNode>,
) -> Result<(), Error> {
    let mut buffer = [0u8; IO_CHUNK];
    let mut done = 0u64;
    while done < length {
        let want = core::cmp::min(IO_CHUNK as u64, length - done) as usize;
        let read = source
            .read(source_offset + done, &mut buffer[..want])
            .map_err(Error::Vfs)?;
        if read == 0 {
            return Err(Error::BadPackage);
        }
        write_all(target, done, &buffer[..read])?;
        done += read as u64;
    }
    Ok(())
}

// ── names ──────────────────────────────────────────────────────────────────

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= NAME_CAPACITY
        && !bytes.contains(&b'/')
        && name != "."
        && name != ".."
}

fn suffixed_name<'a>(name: &str, suffix: &str, out: &'a mut [u8; 48]) -> Result<&'a str, Error> {
    let name = name.as_bytes();
    let suffix = suffix.as_bytes();
    if name.len() + suffix.len() > out.len() {
        return Err(Error::InvalidName);
    }
    out[..name.len()].copy_from_slice(name);
    out[name.len()..name.len() + suffix.len()].copy_from_slice(suffix);
    core::str::from_utf8(&out[..name.len() + suffix.len()]).map_err(|_| Error::InvalidName)
}

fn manifest_file_name<'a>(name: &str, out: &'a mut [u8; 48]) -> Result<&'a str, Error> {
    suffixed_name(name, ".manifest", out)
}
