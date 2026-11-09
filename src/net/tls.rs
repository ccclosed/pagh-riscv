//! HTTPS over TLS 1.3 — **fail-closed server authentication** for the RISC-V
//! port, wired onto the kernel's own TCP client.
//!
//! Every handshake authenticates the peer before a single HTTP byte is sent:
//!
//!   * the server certificate chain is validated down to a committed trust
//!     anchor ([`super::ca_bundle::CA_BUNDLE`]) — signatures, `cA=TRUE` on
//!     every issuer, validity windows ([`super::tls_chain::verify_chain`]);
//!   * the connection target is authorized against the leaf's SAN entries
//!     (RFC 6125, DNS names and IP literals; no CN fallback)
//!     ([`super::tls_auth::authenticate_server`]);
//!   * the system clock is gated: an absent RTC refuses the handshake rather
//!     than validating against a bogus "now" ([`super::tls_chain::CLOCK_FLOOR`],
//!     [`crate::drivers::rtc`]);
//!   * the handshake's `CertificateVerify` signature is checked against the
//!     verified leaf key ([`super::tls_auth::verify_certificate_verify`]).
//!
//! Any failure aborts the handshake — there is no fallback path that keeps the
//! connection. The session RNG is the kernel entropy pool, and a pool that was
//! never seeded with `/chosen/rng-seed` refuses the fetch before a socket is
//! opened ([`crate::entropy`]): predictable randomness behind a TLS-looking
//! facade is worse than no TLS at all.
//!
//! ## How it is wired (kernel-only module)
//!
//! `embedded-tls` is async (`embedded-io-async`). There is no async runtime, so
//! three small pieces bridge it onto the kernel's own TCP client:
//!
//!   1. [`block_on`] — a minimal executor that polls a pinned future until it
//!      is `Ready`, sleeping one tick between polls so the timer and the NIC
//!      keep moving;
//!   2. [`TlsTransport`] — implements `embedded_io_async::Read`/`Write` over
//!      the active TCP connection ([`super::tcp`]), draining the socket in
//!      bounded non-blocking steps and never holding a stack lock across an
//!      `await`, with an inactivity budget so a stall returns an error instead
//!      of hanging;
//!   3. [`KernelRng`] — adapts the kernel entropy pool to the `RngCore +
//!      CryptoRng` traits `embedded-tls` requires.
//!
//! [`KernelProvider`] supplies both that RNG and [`KernelVerifier`], the
//! `TlsVerifier` implementation that adapts the handshake's borrowed
//! certificate entries and negotiated signature scheme onto the pure decision
//! layer in [`super::tls_auth`]. Because it always returns a verifier,
//! `embedded-tls`'s "no verifier ⇒ skip verification" path can never be taken.
//!
//! [`https_get`] ties them together: resolve → connect → handshake → HTTP GET →
//! collect the `Content-Length` body, reusing the pure
//! [`build_get_request`](super::http::build_get_request) /
//! [`parse_http_head`](super::http::parse_http_head) for all wire formatting.
//!
//! ## Where the diagnostics are
//!
//! The verifier reports every rejection on the serial line under the stable
//! marker `tls: verify FAILED cause=…` (`chain: …`, `hostname`, `clock`,
//! `leaf-key`, `no-hostname`) and every success as `tls: verified leaf=… …`.
//! `embedded-tls`'s own handshake messages are not logged; those lines, plus
//! the `stage=` prefixes on the fetch path, are the whole diagnostic surface
//! and what the live transcript is read for.
//!
//! ## Differences from the x86-64 sibling
//!
//! * the transport sits on [`super::tcp`] (a bounded, blocking `connect` plus
//!   non-blocking drains) instead of a `NET`-lock step loop;
//! * the clock is the goldfish RTC ([`crate::drivers::rtc`]), not CMOS;
//! * the RNG is the `/chosen/rng-seed` pool ([`crate::entropy`]), not
//!   RDSEED/RDRAND — see that module's docs for the honest security statement;
//! * the CA bundle carries two extra anchors (the SSL.com 2022 roots), because
//!   `example.com` — the port's live proof host — chains to one of them.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use embedded_io::ErrorKind;
use embedded_tls::{
    Aes128GcmSha256, CertificateEntryRef, CertificateRef, CertificateVerifyRef, CryptoProvider,
    SignatureScheme, TlsConfig, TlsConnection, TlsContext, TlsError, TlsVerifier,
};

use super::tcp;
use super::wire::Ipv4;

/// Default HTTPS port.
pub const HTTPS_PORT: u16 = 443;

/// Width of the handshake **transcript hash** the verifier consumes: the hash
/// of the negotiated CIPHER SUITE, [`Aes128GcmSha256`] ⇒ SHA-256 ⇒ 32.
///
/// NOT the scheme's hash width (RFC 8446 §4.4.3): `ecdsa_secp384r1_sha384`
/// signs a SHA-384 digest *of this 32-byte value*, so a SHA-384-scheme leaf is
/// verifiable on this suite. The compile-time assertion below keeps this
/// constant tied to the suite actually instantiated here.
const CIPHER_SUITE_HASH_LEN: usize = 32;
const _: () = assert!(
    CIPHER_SUITE_HASH_LEN
        == <<<Aes128GcmSha256 as embedded_tls::TlsCipherSuite>::Hash as sha2::digest::OutputSizeUser>::OutputSize as sha2::digest::typenum::Unsigned>::USIZE,
    "CIPHER_SUITE_HASH_LEN must match the negotiated cipher suite's hash width"
);

/// Inactivity (idle) timeout for the TLS transport (~20 s), re-armed on every
/// byte sent or received: a slow-but-progressing handshake or download is not
/// killed — only a genuine stall aborts.
const TLS_IDLE_TIMEOUT_TICKS: u64 = 20 * crate::arch::riscv64::timer::TICK_HZ as u64;
/// TLS record buffer size. 16 KiB + overhead is the safe maximum for any
/// TLS 1.3 record (`embedded-tls` documents 16640 as the cap).
const RECORD_BUF: usize = 16 * 1024 + 256;
/// Upper bound on the decrypted HTTP response we will buffer.
const MAX_TOTAL: usize = 8 * 1024 * 1024;
/// Application read chunk. `embedded-tls` returns at most one decrypted record
/// per `read`, so a full-record buffer avoids throttling the executor's polls.
const APP_READ_BUF: usize = 16 * 1024;

// ───────────────────────────── errors ─────────────────────────────

/// Why an HTTPS fetch did not produce a response body. Every variant is a
/// refusal; none of them is a partial success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The interface has no DHCP lease.
    NoNetwork,
    /// The kernel entropy pool has no seed — TLS is refused outright.
    NoEntropy,
    /// The machine has no wall clock, so certificate validity cannot be judged.
    NoClock,
    /// The host name did not resolve.
    Resolve,
    /// The TCP connection could not be established.
    Connect,
    /// The TLS handshake failed (see the serial log for the cause).
    Handshake,
    /// The server certificate was rejected: chain, SAN, clock or signature.
    Certificate(&'static str),
    /// A TLS record or TCP read failed mid-stream.
    Read,
    /// The response head arrived but was not `200`.
    Status(u16),
    /// The response had no `Content-Length`.
    UnknownLength,
    /// The stream ended before the declared body was complete.
    Incomplete,
    /// The body exceeded [`MAX_TOTAL`].
    TooLarge,
}

impl core::fmt::Display for FetchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FetchError::NoNetwork => f.write_str("no network lease"),
            FetchError::NoEntropy => f.write_str("no entropy seed (TLS refused)"),
            FetchError::NoClock => f.write_str("no wall clock (TLS refused)"),
            FetchError::Resolve => f.write_str("name resolution failed"),
            FetchError::Connect => f.write_str("TCP connect failed"),
            FetchError::Handshake => f.write_str("TLS handshake failed"),
            FetchError::Certificate(why) => write!(f, "certificate rejected ({why})"),
            FetchError::Read => f.write_str("TLS record read failed"),
            FetchError::Status(code) => write!(f, "HTTP status {code}"),
            FetchError::UnknownLength => f.write_str("no Content-Length"),
            FetchError::Incomplete => f.write_str("body incomplete"),
            FetchError::TooLarge => f.write_str("body too large"),
        }
    }
}

// ───────────────────── entropy-backed TLS RNG ─────────────────────

/// Adapter from the kernel entropy pool to the `rand_core` traits required by
/// `embedded-tls`.
///
/// [`https_get`] checks [`crate::entropy::is_available`] before opening a
/// socket, so the "unavailable" path below is unreachable on the only call
/// site; the trait's infallible methods still have to say something, and
/// panicking on a kernel invariant failure is the honest choice for a
/// `panic = "abort"` kernel (the alternative — substituting zeros — would be a
/// silent key-recovery hole).
struct KernelRng;

impl rand_core::RngCore for KernelRng {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0u8; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        if !crate::entropy::fill(dest) {
            panic!("TLS entropy pool became unavailable mid-handshake");
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        if crate::entropy::fill(dest) {
            Ok(())
        } else {
            // A non-zero code keeps the error constructible without `std`.
            Err(rand_core::Error::from(core::num::NonZeroU32::MIN))
        }
    }
}

impl rand_core::CryptoRng for KernelRng {}

// ─────────────────── certificate verification ───────────────────

/// Parsed trust anchors, built once per boot (see [`bundle_anchors`]).
static ANCHORS: crate::sync::Spinlock<Option<alloc::sync::Arc<Vec<TrustAnchor<'static>>>>> =
    crate::sync::Spinlock::new(None);

use super::tls_chain::TrustAnchor;

/// The configured trust-anchor list from the committed CA bundle
/// ([`super::ca_bundle::CA_BUNDLE`]), parsed lazily and cached for the whole
/// boot.
///
/// Each root is parsed with the kernel's own X.509 parser. An anchor that fails
/// to parse is SKIPPED with a diagnostic rather than aborting the whole
/// handshake: skipping narrows the trust set, which can only turn a would-be
/// accept into a reject — the fail-closed direction. An empty anchor set makes
/// every handshake fail with `ChainError::NoAnchor`, which is correct and is
/// reported once, at the moment it is discovered.
fn bundle_anchors() -> alloc::sync::Arc<Vec<TrustAnchor<'static>>> {
    let mut cache = ANCHORS.lock();
    if let Some(anchors) = cache.as_ref() {
        return anchors.clone();
    }

    let mut out = Vec::with_capacity(super::ca_bundle::CA_BUNDLE.len());
    for (label, der) in super::ca_bundle::CA_BUNDLE.iter() {
        match super::x509::parse_certificate(der) {
            Ok((cert, rest)) if rest.is_empty() => out.push(TrustAnchor {
                subject: cert.subject,
                key: cert.spki.key,
            }),
            _ => crate::kprintln!(
                "tls: trust anchor {label:?} in the CA bundle does not parse; skipping it \
                 (the bundle needs regeneration)"
            ),
        }
    }

    if out.is_empty() {
        crate::kprintln!(
            "tls: the committed CA bundle yielded no usable trust anchors - every HTTPS \
             handshake will be refused (ChainError::NoAnchor)"
        );
    }

    let anchors = alloc::sync::Arc::new(out);
    *cache = Some(anchors.clone());
    anchors
}

/// Completed server authentications, process-wide.
///
/// [`KernelVerifier::verify_certificate`] increments it only after the full
/// decision (chain → committed CA bundle, SAN hostname, validity + clock gate)
/// succeeded. [`https_get`] snapshots it before the handshake and requires the
/// value to have advanced before it will send a single application byte.
///
/// WHY THIS EXISTS ON TOP OF THE HANDSHAKE STATE MACHINE: a TLS 1.3 client must
/// reject a server `Finished` that was not preceded by `Certificate` and
/// `CertificateVerify` (RFC 8446 §4.4.2.4). `vendor/embedded-tls` did not
/// enforce that upstream, so a peer could omit both messages, still reach
/// `ApplicationData`, and never invoke this verifier at all — the whole
/// fail-closed story silently bypassed. The vendored state machine is patched
/// to enforce it (`connection.rs`), and this counter is the belt to that
/// braces: it makes "the handshake completed" and "a certificate was actually
/// verified" two separately checked facts at the call site, so a future
/// vendored bump cannot quietly restore the fail-open path.
static VERIFIED_HANDSHAKES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Why the most recent [`KernelVerifier`] rejection happened, as a short
/// static label (`chain`, `hostname`, `clock`, `no-hostname`, `leaf-key`,
/// `signature-scheme`, `signature`).
///
/// The handshake's own error (`TlsError::InvalidCertificate`) cannot say
/// *which* gate refused, and the shell's one-line summary should. `https_get`
/// clears this before a handshake and reads it only after a failed `open()`,
/// so it always describes the handshake it just ran; the detailed line for
/// the operator is still the `tls: verify FAILED cause=…` diagnostic printed
/// at the moment of refusal.
static VERIFY_FAILURE: crate::sync::Spinlock<Option<&'static str>> =
    crate::sync::Spinlock::new(None);

/// Record (and print) one verifier rejection cause.
fn note_verify_failure(cause: &'static str) {
    *VERIFY_FAILURE.lock() = Some(cause);
}

/// Snapshot of [`VERIFIED_HANDSHAKES`], taken before a handshake starts.
fn verified_handshakes() -> u64 {
    VERIFIED_HANDSHAKES.load(core::sync::atomic::Ordering::Acquire)
}

/// The `embedded-tls` verifier driving the pure decision layer
/// ([`super::tls_auth`]) from the handshake.
///
/// State is owned, not borrowed: the certificate entries arrive as borrows of
/// the handshake's record buffers, so the leaf key and the transcript hash are
/// copied at `verify_certificate` time and used by `verify_signature` in the
/// same handshake. A verifier that was never given a certificate cannot verify
/// a signature (`TlsError::InvalidCertificate`), so the ordering "certificate
/// first, then CertificateVerify" is enforced, not assumed.
#[derive(Default)]
struct KernelVerifier {
    /// Connection target from `TlsConfig::server_name` — always set by
    /// [`https_get`]; `None` fails closed inside `tls_auth`.
    host: Option<String>,
    /// Verified leaf key, set by `verify_certificate`.
    leaf_key: Option<super::tls_auth::LeafKey>,
    /// Handshake transcript hash at certificate time (the RFC 8446 §4.4.3
    /// signed message covers the transcript up to the certificate).
    transcript: Option<Vec<u8>>,
}

impl TlsVerifier<Aes128GcmSha256> for KernelVerifier {
    fn set_hostname_verification(&mut self, hostname: &str) -> Result<(), TlsError> {
        self.host = Some(String::from(hostname));
        Ok(())
    }

    fn verify_certificate(
        &mut self,
        transcript: &<Aes128GcmSha256 as embedded_tls::TlsCipherSuite>::Hash,
        cert: CertificateRef,
    ) -> Result<(), TlsError> {
        // Collect the raw DER of every entry. A non-X.509 entry (raw public
        // key) is outside the authenticated surface — reject rather than skip
        // it, since the peer would otherwise choose which entries count.
        let mut entries: Vec<&[u8]> = Vec::with_capacity(cert.entries.len());
        for entry in cert.entries.iter() {
            match entry {
                CertificateEntryRef::X509(der) => entries.push(der),
                CertificateEntryRef::RawPublicKey(_) => {
                    note_verify_failure("leaf-key");
                    crate::kprintln!(
                        "tls: verify FAILED cause=leaf-key (raw public key entry is not supported)"
                    );
                    return Err(TlsError::InvalidCertificate);
                }
            }
        }

        // WHICH clock the validity windows are judged against. `None` (no RTC)
        // becomes `0` inside the chain gate, which is below `CLOCK_FLOOR` and
        // therefore a `ChainError::Clock` reject — the fail-closed direction,
        // stated once here rather than folded into a default.
        let now = match crate::drivers::rtc::now_unix() {
            Some(seconds) => seconds as i64,
            None => {
                note_verify_failure("clock");
                crate::kprintln!("tls: verify FAILED cause=clock (no wall clock on this machine)");
                return Err(TlsError::InvalidCertificate);
            }
        };

        let anchors = bundle_anchors();
        match super::tls_auth::authenticate_server(&entries, self.host.as_deref(), &anchors, now) {
            Ok(auth) => {
                self.leaf_key = Some(auth.leaf_key);
                use sha2::Digest;
                self.transcript = Some(transcript.clone().finalize().to_vec());
                VERIFIED_HANDSHAKES.fetch_add(1, core::sync::atomic::Ordering::Release);
                // Evidence line: the leaf fingerprint lets the live transcript
                // be cross-checked against `openssl s_client` on the host, and
                // names exactly which certificate was authenticated.
                let mut fingerprint = [0u8; 32];
                let mut hasher = sha2::Sha256::new();
                hasher.update(entries[0]);
                fingerprint.copy_from_slice(&hasher.finalize());
                crate::kprintln!(
                    "tls: verified leaf=sha256:{} chain={} certs anchors={} host={} now={}",
                    hex(&fingerprint),
                    entries.len(),
                    anchors.len(),
                    self.host.as_deref().unwrap_or("?"),
                    now
                );
                Ok(())
            }
            Err(error) => {
                note_verify_failure(match error {
                    super::tls_auth::AuthError::Chain(_) => "chain",
                    super::tls_auth::AuthError::NoHostname => "no-hostname",
                    super::tls_auth::AuthError::HostnameMismatch => "hostname",
                    super::tls_auth::AuthError::UnsupportedLeafKey => "leaf-key",
                });
                match error {
                    super::tls_auth::AuthError::Chain(chain) => crate::kprintln!(
                        "tls: verify FAILED cause=chain ({chain:?}) - the server chain does not \
                         validate against the committed CA bundle"
                    ),
                    super::tls_auth::AuthError::NoHostname => crate::kprintln!(
                        "tls: verify FAILED cause=no-hostname (nothing to authorize the \
                         certificate against)"
                    ),
                    super::tls_auth::AuthError::HostnameMismatch => crate::kprintln!(
                        "tls: verify FAILED cause=hostname (the certificate does not authorize \
                         {:?} through its SAN entries)",
                        self.host.as_deref().unwrap_or("?")
                    ),
                    super::tls_auth::AuthError::UnsupportedLeafKey => crate::kprintln!(
                        "tls: verify FAILED cause=leaf-key (the leaf public key algorithm is not \
                         supported)"
                    ),
                }
                Err(TlsError::InvalidCertificate)
            }
        }
    }

    fn verify_signature(&mut self, verify: CertificateVerifyRef) -> Result<(), TlsError> {
        // Both facts must come from THIS handshake's verified certificate.
        let leaf_key = match self.leaf_key.as_ref() {
            Some(key) => key,
            None => return Err(TlsError::InvalidCertificate),
        };
        let hash = match self.transcript.as_ref() {
            Some(hash) => hash,
            None => return Err(TlsError::InvalidCertificate),
        };
        let scheme = tls13_scheme(verify.signature_scheme).ok_or_else(|| {
            note_verify_failure("signature-scheme");
            crate::kprintln!(
                "tls: verify FAILED cause=signature-scheme ({:?} is not accepted for TLS 1.3 \
                 CertificateVerify)",
                verify.signature_scheme
            );
            TlsError::InvalidSignatureScheme
        })?;
        super::tls_auth::verify_certificate_verify(
            scheme,
            CIPHER_SUITE_HASH_LEN,
            hash,
            verify.signature,
            leaf_key,
        )
        .map_err(|error| {
            note_verify_failure("signature");
            crate::kprintln!("tls: verify FAILED cause=signature (CertificateVerify: {error:?})");
            TlsError::InvalidSignature
        })?;
        crate::kprintln!("tls: CertificateVerify OK (server proved possession of the leaf key)");
        Ok(())
    }
}

/// Map the negotiated TLS `SignatureScheme` onto the verifier's own enum.
///
/// `None` for everything the pure layer does not implement — `rsa_pkcs1_*`
/// (banned in TLS 1.3: their presence is a downgrade marker) and
/// PSS-with-PSS-key (`rsa_pss_pss_*`, an SPKI family the X.509 layer rejects).
/// Everything else stays REACHABLE, including the SHA-384/512 schemes: their
/// signature digest is independent of the SHA-256 transcript hash this suite
/// negotiates, so a P-384 or RSA-PSS-384/512 leaf verifies here instead of
/// failing closed on a width check (`CIPHER_SUITE_HASH_LEN` explains that
/// distinction).
fn tls13_scheme(scheme: SignatureScheme) -> Option<super::tls_verify::Tls13Scheme> {
    use super::tls_verify::Tls13Scheme;
    match scheme {
        SignatureScheme::EcdsaSecp256r1Sha256 => Some(Tls13Scheme::EcdsaSecp256r1Sha256),
        SignatureScheme::EcdsaSecp384r1Sha384 => Some(Tls13Scheme::EcdsaSecp384r1Sha384),
        SignatureScheme::Ed25519 => Some(Tls13Scheme::Ed25519),
        SignatureScheme::RsaPssRsaeSha256 => Some(Tls13Scheme::RsaPssRsaeSha256),
        SignatureScheme::RsaPssRsaeSha384 => Some(Tls13Scheme::RsaPssRsaeSha384),
        SignatureScheme::RsaPssRsaeSha512 => Some(Tls13Scheme::RsaPssRsaeSha512),
        _ => None,
    }
}

/// The `embedded-tls` crypto provider: the kernel entropy pool plus the
/// fail-closed verifier. `verifier()` always returns `Ok`, so
/// `embedded-tls`'s "no verifier ⇒ skip certificate and signature
/// verification" branch cannot be reached on this path.
struct KernelProvider {
    rng: KernelRng,
    verifier: KernelVerifier,
}

impl CryptoProvider for KernelProvider {
    type CipherSuite = Aes128GcmSha256;
    type Signature = p256::ecdsa::DerSignature;

    fn rng(&mut self) -> impl rand_core::CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Self::CipherSuite>, TlsError> {
        Ok(&mut self.verifier)
    }
}

// ─────────────────────────── minimal executor ───────────────────────────

/// Build a no-op `RawWaker`: [`block_on`] re-polls unconditionally, so waking
/// is a no-op (the waker exists only to satisfy `Context`).
fn noop_raw_waker() -> RawWaker {
    fn no_op(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        noop_raw_waker()
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
    RawWaker::new(core::ptr::null(), &VTABLE)
}

/// Minimal single-future executor: poll `fut` to completion with a no-op waker,
/// sleeping one tick between `Pending` polls so the timer and the NIC advance.
/// The transport's inactivity budget guarantees this loop terminates (with an
/// error) rather than hanging on a stall.
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    // SAFETY: the vtable's clone/wake/drop are all no-ops over a null data
    // pointer, so the resulting `Waker` upholds the `RawWaker` contract
    // trivially.
    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            // `wfi` until the next tick: the transport only makes progress
            // when the NIC or the peer does, and a busy spin would fight the
            // polled stack for the hart.
            Poll::Pending => crate::arch::riscv64::timer::sleep_ticks(1),
        }
    }
}

// ────────────────────────── transport adapter ──────────────────────────

/// Raw bytes pulled off the TCP socket by the TLS transport, and how many
/// transport polls that took. Printed with the completion/diagnostic lines so a
/// stall can be attributed to a layer (socket, transport or record layer)
/// instead of looking like a silent hang.
static TLS_RAW_IN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TLS_READ_POLLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TLS_READ_PENDING: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn trace_reset() {
    use core::sync::atomic::Ordering::Relaxed;
    TLS_RAW_IN.store(0, Relaxed);
    TLS_READ_POLLS.store(0, Relaxed);
    TLS_READ_PENDING.store(0, Relaxed);
}

/// An `embedded-io[-async]` byte transport over the active TCP connection.
///
/// Each poll calls into [`super::tcp`], which pumps the NIC itself
/// ([`super::poll`] runs inside every receive/step). Reads drain as much as the
/// socket holds in one pass; writes go through the bounded, ACK-waiting
/// `send_id`. No lock is held across an `await` — the TCP module takes and
/// releases its own state lock inside each call — and an inactivity deadline
/// bounds stalls.
///
/// The deadline is armed **lazily, at the first `Pending` poll of a wait**, and
/// disarmed by every byte that moves. Arming it in `new()` would instead make
/// the budget a property of the session's whole lifetime: a userland session
/// that idles for a minute and then asks for a read would find its deadline long
/// past and fail instantly instead of waiting for the peer. With lazy arming,
/// every handshake and every `tls_read`/`tls_write` gets the full
/// [`TLS_IDLE_TIMEOUT_TICKS`] budget, and a stall inside one call still aborts.
struct TlsTransport {
    handle: u8,
    /// Tick at which the current wait gives up; meaningful while `waiting`.
    deadline: u64,
    /// A wait is in progress (the deadline has been armed for this call).
    waiting: bool,
}

impl TlsTransport {
    fn new(handle: u8) -> Self {
        TlsTransport {
            handle,
            deadline: 0,
            waiting: false,
        }
    }

    /// Observable progress: end the current wait and re-arm the budget.
    fn touch(&mut self) {
        self.deadline = crate::arch::riscv64::timer::ticks() + TLS_IDLE_TIMEOUT_TICKS;
        self.waiting = false;
    }

    /// Arm the budget for a wait that is starting. A no-op while one is already
    /// in progress, so repeated `Pending` polls of the same call share one
    /// deadline.
    fn begin_wait(&mut self) {
        if !self.waiting {
            self.deadline = crate::arch::riscv64::timer::ticks() + TLS_IDLE_TIMEOUT_TICKS;
            self.waiting = true;
        }
    }

    /// Return `true` once the current wait's budget has elapsed.
    fn timed_out(&self) -> bool {
        crate::arch::riscv64::timer::ticks() >= self.deadline
    }

    /// One non-blocking drain. `Ready(Ok(n>0))` with data, `Ready(Ok(0))` on a
    /// clean peer close (EOF), `Ready(Err)` on a dead socket/timeout, `Pending`
    /// (re-woken) otherwise.
    fn poll_read_impl(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, ErrorKind>> {
        use core::sync::atomic::Ordering::Relaxed;
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        TLS_READ_POLLS.fetch_add(1, Relaxed);

        let mut filled = 0usize;
        let mut eof = false;
        let mut reset = false;
        while filled < buf.len() {
            match tcp::recv_nonblocking_id(self.handle, &mut buf[filled..]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(tcp::Error::WouldBlock) => break,
                Err(tcp::Error::Reset) => {
                    reset = true;
                    break;
                }
                // Any other error is the socket saying it can no longer
                // deliver: treat it as end-of-stream and let the record layer
                // decide whether the body was complete.
                Err(_) => {
                    eof = true;
                    break;
                }
            }
        }

        if filled > 0 {
            TLS_RAW_IN.fetch_add(filled as u64, Relaxed);
            self.touch();
            return Poll::Ready(Ok(filled));
        }
        if reset {
            self.waiting = false;
            return Poll::Ready(Err(ErrorKind::ConnectionReset));
        }
        if eof {
            self.waiting = false;
            return Poll::Ready(Ok(0));
        }
        self.begin_wait();
        if self.timed_out() {
            self.waiting = false;
            return Poll::Ready(Err(ErrorKind::TimedOut));
        }
        TLS_READ_PENDING.fetch_add(1, Relaxed);
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    /// One bounded write. `Ready(Ok(n>0))` once the peer acknowledged bytes,
    /// `Ready(Err)` on a dead socket, `Pending` (re-woken) otherwise.
    fn poll_write_impl(
        &mut self,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, ErrorKind>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        match tcp::send_id(self.handle, buf) {
            Ok(0) => {
                self.waiting = false;
                Poll::Ready(Err(ErrorKind::BrokenPipe))
            }
            Ok(n) => {
                self.touch();
                Poll::Ready(Ok(n))
            }
            Err(tcp::Error::Closed) => {
                self.waiting = false;
                Poll::Ready(Err(ErrorKind::BrokenPipe))
            }
            Err(tcp::Error::Reset) => {
                self.waiting = false;
                Poll::Ready(Err(ErrorKind::ConnectionReset))
            }
            Err(_) => {
                self.begin_wait();
                if self.timed_out() {
                    self.waiting = false;
                    Poll::Ready(Err(ErrorKind::TimedOut))
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }
    }
}

impl embedded_io::ErrorType for TlsTransport {
    type Error = ErrorKind;
}

impl embedded_io_async::Read for TlsTransport {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(move |cx| self.poll_read_impl(cx, &mut buf[..])).await
    }
}

impl embedded_io_async::Write for TlsTransport {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        core::future::poll_fn(move |cx| self.poll_write_impl(cx, buf)).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        // Egress happens inside each write's bounded ACK wait; a no-op flush
        // is correct here.
        Ok(())
    }
}

// ───────────────────────────── https_get ─────────────────────────────

/// How the response body's end is determined once the head is parsed.
enum BodyMode {
    /// Classic `Content-Length`: the body is `length` bytes at `offset`.
    Length { offset: usize, length: u64 },
    /// `Transfer-Encoding: chunked`: an incremental framing decoder.
    Chunked(super::http::ChunkedDecoder),
}

/// Perform one HTTPS (TLS 1.3) `GET` of `https://host:port/path` and return the
/// `Content-Length`-sized response body.
///
/// **Authenticated:** the handshake fails unless the server's certificate
/// chain validates against the committed CA bundle, authorizes `host` through
/// its SAN entries, and proves possession of the leaf key with a valid TLS 1.3
/// `CertificateVerify` — with the clock gate applied (no RTC refuses the
/// connection). `port` is normally [`HTTPS_PORT`].
///
/// `host` may be a dotted-quad IPv4 literal or a hostname (resolved through
/// `net::resolve_name`); the request `Host:` header carries the original `host`
/// string; the response head is parsed with the shared pure
/// [`parse_http_head`](super::http::parse_http_head).
pub fn https_get(host: &str, port: u16, path: &str) -> Result<Vec<u8>, FetchError> {
    https_get_at(host, None, port, path)
}

/// As [`https_get`], but `address` pins the destination instead of resolving
/// `host` through DNS.
///
/// The TLS server name is still `host` (SNI and the SAN check), so this is the
/// shape a negative test needs: "talk to *this* server while claiming to be
/// *that* name" is exactly the case certificate verification exists to reject,
/// and it cannot be produced through DNS alone. `None` is the ordinary path and
/// is what [`https_get`] passes.
pub fn https_get_at(
    host: &str,
    address: Option<Ipv4>,
    port: u16,
    path: &str,
) -> Result<Vec<u8>, FetchError> {
    crate::kprintln!("tls: fetch host={host} port={port} path={path}");

    // Fail-closed preflight, before any socket exists.
    if !crate::entropy::is_available() {
        crate::kprintln!(
            "tls: FAILED stage=entropy cause=NoEntropy (the kernel entropy pool has no seed; \
             refusing to invent session keys)"
        );
        return Err(FetchError::NoEntropy);
    }
    if crate::drivers::rtc::now_unix().is_none() {
        crate::kprintln!(
            "tls: FAILED stage=clock cause=NoClock (no wall clock: certificate validity is \
             unjudgeable)"
        );
        return Err(FetchError::NoClock);
    }
    if super::config().is_none() {
        crate::kprintln!("tls: FAILED stage=preflight cause=NoNetwork (no DHCP lease)");
        return Err(FetchError::NoNetwork);
    }

    // Resolve the host (IPv4 literal or DNS), unless the caller pinned the
    // destination address.
    let address = match address {
        Some(pinned) => {
            crate::kprintln!("tls: address pinned by the caller -> {}", ipv4_text(pinned));
            pinned
        }
        None => match super::resolve_name(host) {
            Ok(address) => {
                crate::kprintln!("tls: resolve {host} -> {}", ipv4_text(address));
                address
            }
            Err(error) => {
                crate::kprintln!("tls: FAILED stage=resolve cause=Resolve ({error})");
                return Err(FetchError::Resolve);
            }
        },
    };

    let started = crate::arch::riscv64::timer::ticks();

    // The TCP client is a single active connection; `connect` completes the
    // three-way handshake (bounded) or fails.
    if let Err(error) = tcp::connect(address, port) {
        crate::kprintln!("tls: FAILED stage=connect cause=Connect ({error})");
        return Err(FetchError::Connect);
    }
    let Some(handle) = tcp::active_id() else {
        crate::kprintln!("tls: FAILED stage=connect cause=Connect (no active connection slot)");
        return Err(FetchError::Connect);
    };
    crate::kprintln!(
        "tls: tcp connected to {}:{} id={handle}",
        ipv4_text(address),
        port
    );

    let config = TlsConfig::new().with_server_name(host);
    // The record buffers are the one large allocation on this path; they live
    // on the heap, not the 16 KiB kernel stack.
    let mut read_buf = vec![0u8; RECORD_BUF];
    let mut write_buf = vec![0u8; RECORD_BUF];
    let transport = TlsTransport::new(handle);
    let mut tls: TlsConnection<TlsTransport, Aes128GcmSha256> =
        TlsConnection::new(transport, &mut read_buf[..], &mut write_buf[..]);

    let auth_before = verified_handshakes();
    *VERIFY_FAILURE.lock() = None;
    trace_reset();

    let result = block_on(async {
        // Handshake — server authenticated (chain + hostname + clock +
        // CertificateVerify) through `KernelProvider::verifier`.
        let context = TlsContext::new(
            &config,
            KernelProvider {
                rng: KernelRng,
                verifier: KernelVerifier::default(),
            },
        );
        tls.open(context).await.map_err(|error| {
            crate::kprintln!("tls: FAILED stage=handshake cause=Handshake ({error:?})");
            // A verifier rejection is a *certificate* failure, not a generic
            // handshake one: say which gate refused, so the operator (and the
            // shell's one-line summary) sees the decision rather than the
            // symptom.
            match *VERIFY_FAILURE.lock() {
                Some(cause) => FetchError::Certificate(cause),
                None => FetchError::Handshake,
            }
        })?;

        // Belt to the state machine's braces: refuse to move any application
        // byte unless a certificate was actually verified for THIS handshake.
        if verified_handshakes() == auth_before {
            crate::kprintln!(
                "tls: FAILED stage=verify cause=Certificate (handshake completed without a \
                 verified server certificate - refusing to continue)"
            );
            return Err(FetchError::Certificate("unverified"));
        }
        crate::kprintln!("tls: handshake OK (TLS 1.3, AES-128-GCM-SHA256, server authenticated)");

        // Send the HTTP/1.1 GET through the encrypted channel.
        let request = super::http::build_get_request(host, path);
        let mut sent = 0usize;
        while sent < request.len() {
            let n = match tls.write(&request[sent..]).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) => {
                    crate::kprintln!("tls: FAILED stage=write cause=Read ({error:?})");
                    return Err(FetchError::Read);
                }
            };
            sent += n;
        }
        tls.flush().await.map_err(|_| FetchError::Read)?;

        // Read + decrypt the response, collecting the body. Two framings are
        // understood: `Content-Length` (classic) and `Transfer-Encoding:
        // chunked` (what Cloudflare answers `example.com` with — no
        // `Content-Length` at all). Anything else is refused rather than
        // guessed at.
        let mut buf: Vec<u8> = Vec::new();
        let mut mode: Option<BodyMode> = None;
        let mut fed = 0usize;
        let mut tmp = vec![0u8; APP_READ_BUF];
        let mut read_error: Option<TlsError> = None;
        loop {
            let n = match tls.read(&mut tmp).await {
                Ok(0) => break, // clean EOF
                Ok(n) => n,
                Err(error) => {
                    read_error = Some(error);
                    break;
                }
            };
            buf.extend_from_slice(&tmp[..n]);
            if buf.len() > MAX_TOTAL {
                crate::kprintln!("tls: FAILED stage=body cause=TooLarge ({MAX_TOTAL} bytes cap)");
                return Err(FetchError::TooLarge);
            }

            if mode.is_none() {
                match super::http::parse_http_head(&buf) {
                    super::http::HeadParse::Need => {}
                    super::http::HeadParse::Malformed => {
                        crate::kprintln!(
                            "tls: FAILED stage=body cause=Incomplete (malformed head)"
                        );
                        return Err(FetchError::Incomplete);
                    }
                    super::http::HeadParse::Done {
                        status,
                        content_length,
                        body_off,
                        chunked,
                    } => {
                        if status != 200 {
                            crate::kprintln!("tls: FAILED stage=response cause=Status({status})");
                            return Err(FetchError::Status(status));
                        }
                        if let Some(length) = content_length {
                            crate::kprintln!(
                                "tls: response 200, content-length {length}, body starts at \
                                 {body_off}"
                            );
                            mode = Some(BodyMode::Length {
                                offset: body_off,
                                length,
                            });
                        } else if chunked {
                            crate::kprintln!(
                                "tls: response 200, transfer-encoding chunked, body starts at \
                                 {body_off}"
                            );
                            let mut decoder = super::http::ChunkedDecoder::new(MAX_TOTAL);
                            let progress = decoder.push(&buf[body_off..]);
                            fed = buf.len();
                            match progress {
                                super::http::ChunkProgress::Done => {
                                    return Ok(decoder.body().to_vec());
                                }
                                super::http::ChunkProgress::Malformed => {
                                    crate::kprintln!(
                                        "tls: FAILED stage=body cause=Incomplete (malformed \
                                         chunked framing)"
                                    );
                                    return Err(FetchError::Incomplete);
                                }
                                super::http::ChunkProgress::Need => {
                                    mode = Some(BodyMode::Chunked(decoder));
                                }
                            }
                        } else {
                            crate::kprintln!("tls: FAILED stage=response cause=UnknownLength");
                            return Err(FetchError::UnknownLength);
                        }
                    }
                }
            }

            match mode.as_mut() {
                Some(BodyMode::Length { offset, length }) => {
                    let received = (buf.len().saturating_sub(*offset)) as u64;
                    if received >= *length {
                        let end = *offset + *length as usize;
                        return Ok(buf[*offset..end].to_vec());
                    }
                }
                Some(BodyMode::Chunked(decoder)) => {
                    if buf.len() > fed {
                        let progress = decoder.push(&buf[fed..]);
                        fed = buf.len();
                        match progress {
                            super::http::ChunkProgress::Done => {
                                return Ok(decoder.body().to_vec());
                            }
                            super::http::ChunkProgress::Malformed => {
                                crate::kprintln!(
                                    "tls: FAILED stage=body cause=Incomplete (malformed chunked \
                                     framing)"
                                );
                                return Err(FetchError::Incomplete);
                            }
                            super::http::ChunkProgress::Need => {}
                        }
                    }
                }
                None => {}
            }
        }

        if let Some(error) = read_error {
            let (got, want) = match mode.as_ref() {
                Some(BodyMode::Length { offset, length }) => {
                    (buf.len().saturating_sub(*offset) as u64, *length)
                }
                Some(BodyMode::Chunked(decoder)) => {
                    (decoder.body().len() as u64, decoder.body().len() as u64)
                }
                None => (0, 0),
            };
            crate::kprintln!(
                "tls: FAILED stage=read cause=Read ({error:?}) after {got} of {want} body bytes \
                 ({} raw socket bytes, {} read polls)",
                TLS_RAW_IN.load(core::sync::atomic::Ordering::Relaxed),
                TLS_READ_POLLS.load(core::sync::atomic::Ordering::Relaxed)
            );
        }
        Err(FetchError::Incomplete)
    });

    // Release the socket whether the fetch succeeded or not; the TLS
    // connection is dropped first so its borrows of the record buffers end.
    let _tls_done: TlsConnection<TlsTransport, Aes128GcmSha256> = tls;
    tcp::close_id(handle);

    match result {
        Ok(body) => {
            let elapsed = crate::arch::riscv64::timer::ticks()
                .saturating_sub(started)
                .max(1);
            crate::kprintln!(
                "tls: fetch OK body={} bytes in {} ms raw_in={} read_polls={} pending={}",
                body.len(),
                elapsed * 10,
                TLS_RAW_IN.load(core::sync::atomic::Ordering::Relaxed),
                TLS_READ_POLLS.load(core::sync::atomic::Ordering::Relaxed),
                TLS_READ_PENDING.load(core::sync::atomic::Ordering::Relaxed),
            );
            Ok(body)
        }
        Err(error) => {
            crate::kprintln!("tls: FAILED cause={error:?}");
            Err(error)
        }
    }
}

// ───────────────── userland TLS sessions (syscalls 1100–1103) ─────────────────
//
// The shell's `https` path is one-shot by design: `https_get_at` owns the socket
// from `connect` to `close` and buffers the whole body. A userland program needs
// the other shape — a *session* it drives: connect a socket it owns, hand that
// socket to the kernel for the handshake, then read and write plaintext through
// the descriptor until it decides to close.
//
// That is what this section implements. A session is a `TlsConnection` plus the
// three things the one-shot path never needed: the record buffers (now owned, so
// they outlive the fetch), a plaintext window (so a small `read` cannot lose the
// rest of a record) and the release state (`eof`/`closed`).
//
// # Security shape
//
// `user_open` runs the **same** handshake as `https_get`: the same
// `KernelProvider`, whose `verifier()` always returns `KernelVerifier`, so
// `embedded-tls`'s "no verifier ⇒ skip verification" branch is unreachable; the
// same committed CA bundle, SAN authorization, clock gate and
// `CertificateVerify`; the same belt-and-braces check that
// [`VERIFIED_HANDSHAKES`] advanced during *this* handshake. There is no flag on
// `tls_connect` that relaxes any of it — the ABI defines `flags = 0` and answers
// `-EINVAL` for anything else, so a future "insecure" bit cannot be smuggled in
// without changing the documented contract.
//
// # Locking and the timer
//
// A session is *taken out* of the table for the duration of a blocking call and
// put back afterwards: the table lock is a spinlock (interrupts masked), and
// `block_on` sleeps on the timer tick, so holding it across a handshake would
// deadlock the boot. While a call is in flight its slot stays `used` with an
// empty payload, which is how a second, concurrent call on the same session gets
// `-EAGAIN` instead of a second connection to the same peer.
//
// The caller must run these with interrupts enabled when it is a syscall (the
// trap entry masks them); `syscall::with_network_interrupts` is that wrapper.

/// TLS sessions the kernel holds for userland at once.
///
/// A memory bound, not a policy: a session owns two 16 KiB record buffers and a
/// 16 KiB plaintext window until it is closed, so four sessions cap the feature
/// at ~200 KiB of heap. A program that needs a fifth closes one — `tls_close`
/// frees the buffers immediately, it does not pool them.
pub const MAX_USER_SESSIONS: usize = 4;

/// Plaintext window per session: one whole record. `embedded-tls` hands back at
/// most one decrypted record per `read`, so a window this wide means a
/// userland `read` never throws plaintext away, however small its buffer is.
const SESSION_WINDOW: usize = APP_READ_BUF;

/// Why a userland TLS operation did not complete. The syscall layer maps these
/// onto the ABI's errno set (`syscall::tls_errno`); nothing here is a partial
/// success — a short read/write is reported as `Ok` with a smaller count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// The descriptor is not a TLS session (or its slot is free).
    NoSession,
    /// Another thread of the same process is inside a call on this session.
    Busy,
    /// No free session slot.
    NoSpace,
    /// The entropy pool has no seed: refused before any socket work.
    NoEntropy,
    /// No wall clock: certificate validity windows cannot be judged.
    NoClock,
    /// The handshake failed (protocol, transport or timeout).
    Handshake,
    /// The server certificate was rejected (chain, SAN, clock or signature).
    Certificate,
    /// The session was shut down for writing (`tls_close`/`shutdown`).
    Closed,
    /// The peer reset the connection, or the stream ended without a
    /// `close_notify`.
    Reset,
    /// The inactivity budget expired.
    Timeout,
    /// A TLS record/protocol failure.
    Protocol,
    /// A transport failure.
    Io,
}

/// How a session looks to `poll(2)` right now, without blocking.
#[derive(Debug, Clone, Copy)]
pub struct SessionReady {
    /// Plaintext is buffered, or the socket has bytes (possibly a partial
    /// record), or the session reached end-of-stream (`read` returns 0).
    pub readable: bool,
    /// The session can accept application data.
    pub writable: bool,
    /// The peer sent `close_notify`, or the stream ended.
    pub eof: bool,
}

/// One userland TLS session.
///
/// SELF-REFERENTIAL BY CONSTRUCTION: `conn` borrows the heap blocks owned by
/// `read_buf`/`write_buf`. Two rules keep that sound and both are visible here:
/// the borrowed blocks are **boxed** (so the address of the data does not move
/// when this struct moves), and `conn` is declared **first** (so `Drop` runs it
/// before the boxes it borrows). No `Drop` impl and no `mem::swap` of the
/// buffers may be added without revisiting this.
struct UserSession {
    /// The connection; `None` only after `shutdown` consumed it via `close`.
    conn: Option<TlsConnection<'static, TlsTransport, Aes128GcmSha256>>,
    /// Record read buffer borrowed by `conn` (see the type-level note). Never
    /// read directly — it exists so the block `conn` borrows stays alive.
    #[allow(dead_code)]
    read_buf: Box<[u8]>,
    /// Record write buffer borrowed by `conn` (see the type-level note).
    #[allow(dead_code)]
    write_buf: Box<[u8]>,
    /// Decrypted, not yet delivered to userland (`window_at..window_len`).
    window: Box<[u8]>,
    /// Read cursor into `window`.
    window_at: usize,
    /// Valid bytes in `window`.
    window_len: usize,
    /// TCP slot this session encrypts onto; released with the session.
    tcp: u8,
    /// Host name the handshake authenticated (SNI + SAN). Diagnostics only.
    host: String,
    /// The peer's `close_notify` arrived, or the stream ended: reads return 0.
    eof: bool,
    /// The session was shut down for writing.
    closed: bool,
}

impl UserSession {
    /// Build a session over TCP slot `tcp_id` for `host`.
    fn new(tcp_id: u8, host: &str) -> Box<Self> {
        let mut read_buf = vec![0u8; RECORD_BUF].into_boxed_slice();
        let mut write_buf = vec![0u8; RECORD_BUF].into_boxed_slice();
        // SAFETY: the two slices are the heap blocks of `read_buf`/`write_buf`,
        // which are moved into the returned `UserSession` immediately below and
        // are never reallocated, moved or freed while the connection exists.
        // Extending their lifetime to `'static` is what lets the connection live
        // in the session table instead of on one stack frame; the field order
        // and the boxed allocation are what make the extension sound (see the
        // struct-level note).
        let read: &'static mut [u8] =
            unsafe { core::mem::transmute::<&mut [u8], &'static mut [u8]>(&mut read_buf[..]) };
        let write: &'static mut [u8] =
            unsafe { core::mem::transmute::<&mut [u8], &'static mut [u8]>(&mut write_buf[..]) };
        let conn = TlsConnection::new(TlsTransport::new(tcp_id), read, write);
        Box::new(UserSession {
            conn: Some(conn),
            read_buf,
            write_buf,
            window: vec![0u8; SESSION_WINDOW].into_boxed_slice(),
            window_at: 0,
            window_len: 0,
            tcp: tcp_id,
            host: String::from(host),
            eof: false,
            closed: false,
        })
    }
}

/// One row of the session table. `used` outlives the payload: a blocking call
/// takes the `Box` out and leaves the slot claimed, which is what stops a second
/// `tls_connect` from taking the same id while the first is in flight.
struct SessionSlot {
    used: bool,
    session: Option<Box<UserSession>>,
}

impl SessionSlot {
    const fn free() -> Self {
        SessionSlot {
            used: false,
            session: None,
        }
    }
}

static USER_SESSIONS: crate::sync::Spinlock<[SessionSlot; MAX_USER_SESSIONS]> =
    crate::sync::Spinlock::new([const { SessionSlot::free() }; MAX_USER_SESSIONS]);

/// Counters the selftest and the shell read as evidence that the path ran; also
/// what a stuck session would move. Relaxed: evidence, not synchronisation.
static SESSION_OPENS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SESSION_REFUSALS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SESSION_BYTES_OUT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SESSION_BYTES_IN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SESSION_CLOSES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// A snapshot of the userland session counters and the live session count.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionStats {
    /// Handshakes that completed and were published to a descriptor.
    pub opened: u64,
    /// Handshakes refused (preflight, protocol or certificate).
    pub refusals: u64,
    /// Plaintext bytes handed to `tls_write`.
    pub bytes_out: u64,
    /// Plaintext bytes returned by `tls_read`.
    pub bytes_in: u64,
    /// Sessions released (`tls_close`, `close`, or process teardown).
    pub closes: u64,
    /// Sessions live right now.
    pub live: usize,
}

/// Snapshot the session table's counters and occupancy.
pub fn session_stats() -> SessionStats {
    use core::sync::atomic::Ordering::Relaxed;
    let live = USER_SESSIONS.lock().iter().filter(|slot| slot.used).count();
    SessionStats {
        opened: SESSION_OPENS.load(Relaxed),
        refusals: SESSION_REFUSALS.load(Relaxed),
        bytes_out: SESSION_BYTES_OUT.load(Relaxed),
        bytes_in: SESSION_BYTES_IN.load(Relaxed),
        closes: SESSION_CLOSES.load(Relaxed),
        live,
    }
}

/// Claim a free session id, or `None` when all of them are in use.
fn reserve_session() -> Option<u8> {
    let mut table = USER_SESSIONS.lock();
    for (id, slot) in table.iter_mut().enumerate() {
        if !slot.used {
            slot.used = true;
            slot.session = None;
            return Some(id as u8);
        }
    }
    None
}

/// Publish a freshly handshaked session under its reserved id.
fn publish_session(id: u8, session: Box<UserSession>) {
    if let Some(slot) = USER_SESSIONS.lock().get_mut(id as usize) {
        slot.session = Some(session);
    }
}

/// Take the session out for a blocking call.
///
/// `Err(Busy)` when another call is already in flight (the slot is claimed but
/// its payload is out), `Err(NoSession)` when the id names no live session.
fn take_session(id: u8) -> Result<Box<UserSession>, SessionError> {
    let mut table = USER_SESSIONS.lock();
    let slot = table.get_mut(id as usize).ok_or(SessionError::NoSession)?;
    if !slot.used {
        return Err(SessionError::NoSession);
    }
    slot.session.take().ok_or(SessionError::Busy)
}

/// Put a session back after a blocking call.
///
/// A slot that was released while the call was in flight (`tls_close` from
/// another thread, or process teardown) is *not* resurrected: the session is
/// dropped, which frees its buffers and, through the caller, its socket.
fn put_session(id: u8, session: Box<UserSession>) {
    let mut table = USER_SESSIONS.lock();
    if let Some(slot) = table.get_mut(id as usize) {
        if slot.used {
            slot.session = Some(session);
            return;
        }
    }
    drop(session);
}

/// Free a session id. Idempotent: freeing a free id is a no-op.
fn release_session(id: u8) {
    if let Some(slot) = USER_SESSIONS.lock().get_mut(id as usize) {
        slot.used = false;
        slot.session = None;
    }
}

/// Whether `id` names a live session (used by `poll` to answer `POLLNVAL`).
pub fn session_live(id: u8) -> bool {
    USER_SESSIONS
        .lock()
        .get(id as usize)
        .is_some_and(|slot| slot.used)
}

/// Perform the fail-closed TLS 1.3 handshake on an already-connected TCP client
/// (`tcp_id`) and publish the session under a fresh id.
///
/// Everything the one-shot path guarantees holds here: the entropy pool must be
/// seeded (else [`SessionError::NoEntropy`]), the RTC must give a wall clock
/// (else [`SessionError::NoClock`]), the certificate chain must reach the
/// committed CA bundle, the SAN must authorize `host`, and `CertificateVerify`
/// must prove possession of the leaf key — plus the independent check that
/// [`VERIFIED_HANDSHAKES`] advanced during this handshake.
///
/// A refusal releases the reserved slot before returning, so a failed handshake
/// costs no session and no buffers.
pub fn user_open(tcp_id: u8, host: &str) -> Result<u8, SessionError> {
    // Fail-closed preflight, before a single record is built. The verifier
    // repeats the clock decision (a `None` clock fails the chain gate), but
    // saying it here names the refusal instead of reporting a chain error.
    if !crate::entropy::is_available() {
        SESSION_REFUSALS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        crate::kprintln!(
            "tls: user session host={host} REFUSED cause=NoEntropy (the kernel entropy pool has \
             no seed)"
        );
        return Err(SessionError::NoEntropy);
    }
    if crate::drivers::rtc::now_unix().is_none() {
        SESSION_REFUSALS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        crate::kprintln!(
            "tls: user session host={host} REFUSED cause=NoClock (no wall clock: certificate \
             validity is unjudgeable)"
        );
        return Err(SessionError::NoClock);
    }

    let Some(id) = reserve_session() else {
        return Err(SessionError::NoSpace);
    };
    let mut session = UserSession::new(tcp_id, host);

    let auth_before = verified_handshakes();
    *VERIFY_FAILURE.lock() = None;
    trace_reset();
    let started = crate::arch::riscv64::timer::ticks();

    let future = async {
        let config = TlsConfig::new().with_server_name(host);
        let context = TlsContext::new(
            &config,
            KernelProvider {
                rng: KernelRng,
                verifier: KernelVerifier::default(),
            },
        );
        let conn = session.conn.as_mut().ok_or(SessionError::Closed)?;
        conn.open(context).await.map_err(|error| {
            crate::kprintln!("tls: user session host={host} FAILED stage=handshake ({error:?})");
            match *VERIFY_FAILURE.lock() {
                Some(_) => SessionError::Certificate,
                None => SessionError::Handshake,
            }
        })?;

        // Belt to the state machine's braces, exactly as `https_get_at` does it:
        // refuse to expose a session unless a certificate was verified for THIS
        // handshake.
        if verified_handshakes() == auth_before {
            crate::kprintln!(
                "tls: user session host={host} FAILED stage=verify (handshake completed without \
                 a verified server certificate)"
            );
            return Err(SessionError::Certificate);
        }
        Ok(())
    };
    crate::kprintln!(
        "[stack] tls user_open future = {} bytes",
        core::mem::size_of_val(&future)
    );
    let result = block_on(future);

    match result {
        Ok(()) => {
            let elapsed = crate::arch::riscv64::timer::ticks()
                .saturating_sub(started)
                .max(1);
            crate::kprintln!(
                "tls: user session {id} open tcp={tcp_id} host={host} (TLS 1.3, \
                 AES-128-GCM-SHA256, server authenticated, {} ms)",
                elapsed * 10
            );
            SESSION_OPENS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            publish_session(id, session);
            Ok(id)
        }
        Err(error) => {
            SESSION_REFUSALS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::kprintln!("tls: user session host={host} refused ({error:?})");
            drop(session);
            release_session(id);
            Err(error)
        }
    }
}

/// Encrypt `data` and send it as TLS records. Blocks (bounded by the idle
/// budget) until the record layer accepted every byte.
///
/// A short count is `Ok`: the bytes the record layer took are on their way, and
/// reporting them as an error would make a caller re-send data the peer may
/// already have.
pub fn user_write(id: u8, data: &[u8]) -> Result<usize, SessionError> {
    if data.is_empty() {
        return Ok(0);
    }
    let mut session = take_session(id)?;
    let result = session_write(id, &mut session, data);
    put_session(id, session);
    result
}

fn session_write(id: u8, session: &mut UserSession, data: &[u8]) -> Result<usize, SessionError> {
    if session.closed || session.eof {
        return Err(SessionError::Closed);
    }
    let host = session.host.clone();
    let mut sent = 0usize;
    let future = async {
        let conn = session.conn.as_mut().ok_or(SessionError::Closed)?;
        while sent < data.len() {
            match conn.write(&data[sent..]).await {
                Ok(0) => return Err(SessionError::Closed),
                Ok(n) => sent += n,
                Err(error) => return Err(map_record_error(&error)),
            }
        }
        conn.flush().await.map_err(|error| map_record_error(&error))
    };
    crate::kprintln!(
        "[stack] tls write future = {} bytes",
        core::mem::size_of_val(&future)
    );
    let outcome = block_on(future);

    match outcome {
        Ok(()) => {
            SESSION_BYTES_OUT.fetch_add(sent as u64, core::sync::atomic::Ordering::Relaxed);
            Ok(sent)
        }
        Err(error) if sent > 0 => {
            crate::kprintln!("tls: session {id} write stopped after {sent} bytes ({error:?})");
            SESSION_BYTES_OUT.fetch_add(sent as u64, core::sync::atomic::Ordering::Relaxed);
            Ok(sent)
        }
        Err(error) => {
            crate::kprintln!("tls: session {id} write failed host={host} ({error:?})");
            Err(error)
        }
    }
}

/// Decrypt application data into `out`. Blocks (bounded by the idle budget)
/// until at least one byte is available; returns `Ok(0)` at end-of-stream —
/// a `close_notify` from the peer, or the stream ending.
pub fn user_read(id: u8, out: &mut [u8]) -> Result<usize, SessionError> {
    if out.is_empty() {
        return Ok(0);
    }
    let mut session = take_session(id)?;
    let result = session_read(id, &mut session, out);
    put_session(id, session);
    result
}

fn session_read(id: u8, session: &mut UserSession, out: &mut [u8]) -> Result<usize, SessionError> {
    // Buffered plaintext first: a second, smaller read must not block and must
    // not discard the rest of the record.
    if session.window_at < session.window_len {
        let available = &session.window[session.window_at..session.window_len];
        let take = available.len().min(out.len());
        out[..take].copy_from_slice(&available[..take]);
        session.window_at += take;
        SESSION_BYTES_IN.fetch_add(take as u64, core::sync::atomic::Ordering::Relaxed);
        return Ok(take);
    }
    if session.eof {
        return Ok(0);
    }

    // One blocking record. `embedded-tls` returns at most one decrypted record
    // per `read`, and the window is a full record wide, so nothing is lost.
    let UserSession {
        conn,
        window,
        window_at,
        window_len,
        eof,
        host,
        ..
    } = session;
    let host = host.as_str();
    let conn = conn.as_mut().ok_or(SessionError::Closed)?;
    let future = conn.read(&mut window[..]);
    crate::kprintln!(
        "[stack] tls read future = {} bytes",
        core::mem::size_of_val(&future)
    );
    match block_on(future) {
        Ok(0) => {
            *eof = true;
            Ok(0)
        }
        Ok(n) => {
            *window_at = 0;
            *window_len = n;
            let take = n.min(out.len());
            out[..take].copy_from_slice(&window[..take]);
            *window_at = take;
            SESSION_BYTES_IN.fetch_add(take as u64, core::sync::atomic::Ordering::Relaxed);
            Ok(take)
        }
        // A `close_notify` is the protocol's end-of-stream: clean EOF, not an
        // error. (`embedded-tls` reports it as an error because its own read
        // loop has no other way to say "the peer is done".)
        Err(TlsError::ConnectionClosed) => {
            crate::kprintln!("tls: session {id} peer sent close_notify (clean end of stream)");
            *eof = true;
            *window_at = 0;
            *window_len = 0;
            Ok(0)
        }
        Err(error) => {
            let mapped = map_record_error(&error);
            crate::kprintln!("tls: session {id} read failed host={host} ({error:?})");
            Err(mapped)
        }
    }
}

/// Whether a session has anything to report to `poll(2)` right now.
///
/// `readable` is deliberately "plaintext buffered **or** the socket has bytes":
/// a socket-level byte may be the first half of a record, and answering "not
/// readable" until a whole record arrived would make a `poll`-then-`read` loop
/// sleep through the only wakeup it gets. The price is that a `tls_read` after
/// `POLLIN` may still wait for the rest of the record — bounded by the idle
/// budget — and that is stated in the ABI document.
pub fn user_ready(id: u8) -> Option<SessionReady> {
    let mut table = USER_SESSIONS.lock();
    let slot = table.get_mut(id as usize)?;
    if !slot.used {
        return None;
    }
    match slot.session.as_ref() {
        Some(session) => Some(SessionReady {
            readable: session.window_at < session.window_len
                || tcp::readable_id(session.tcp)
                || session.eof,
            writable: !session.closed && !session.eof && !tcp::reset_id_state(session.tcp),
            eof: session.eof,
        }),
        // A call is in flight and holds the payload. Nothing about the record
        // layer is knowable without waiting for it, so report the conservative
        // answer: not readable, writable. The caller polls again when the
        // in-flight call returns.
        None => Some(SessionReady {
            readable: false,
            writable: true,
            eof: false,
        }),
    }
}

/// Send `close_notify` and stop writing, keeping the session (and its
/// descriptor) alive: later reads return 0, later writes are refused.
///
/// The connection is *consumed* by `embedded-tls`'s `close()`, so a session that
/// was shut down has no connection left — which is exactly the "both directions
/// close" contract the socket `shutdown` already documents.
pub fn user_shutdown(id: u8) -> Result<(), SessionError> {
    let mut session = take_session(id)?;
    let result = session_shutdown(id, &mut session);
    put_session(id, session);
    result
}

fn session_shutdown(id: u8, session: &mut UserSession) -> Result<(), SessionError> {
    if session.closed {
        return Ok(());
    }
    session.closed = true;
    session.window_at = 0;
    session.window_len = 0;
    if session.eof {
        return Ok(());
    }
    session.eof = true;
    if let Some(conn) = session.conn.take() {
        match block_on(conn.close()) {
            Ok(transport) => drop(transport),
            // The alert could not be delivered — the peer is gone or the socket
            // is dead. The session is still closed locally; say so on serial
            // rather than pretend the peer saw a clean shutdown.
            Err((transport, error)) => {
                drop(transport);
                crate::kprintln!(
                    "tls: session {id} close_notify could not be delivered ({error:?})"
                );
            }
        }
    }
    Ok(())
}

/// Release a session: free its buffers and its id. `notify` sends the
/// `close_notify` first (what `tls_close` does); without it the release is
/// immediate (what `close(2)` and process teardown do, because neither may block
/// on a peer).
pub fn user_close(id: u8, notify: bool) -> bool {
    let Ok(mut session) = take_session(id) else {
        // Either the id is free (nothing to do) or a call is in flight: free the
        // slot anyway. `put_session` sees the released slot and drops the
        // payload when the call returns.
        let existed = session_live(id);
        release_session(id);
        return existed;
    };
    if notify {
        let _ = session_shutdown(id, &mut session);
    }
    drop(session);
    release_session(id);
    SESSION_CLOSES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    true
}

/// Map an `embedded-tls` record error onto the ABI-facing [`SessionError`].
fn map_record_error(error: &TlsError) -> SessionError {
    match error {
        TlsError::Io(kind) => match kind {
            ErrorKind::TimedOut => SessionError::Timeout,
            ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::ConnectionAborted => {
                SessionError::Reset
            }
            _ => SessionError::Io,
        },
        // A stream that ends without `close_notify` is reported as `IoError`:
        // the truncation case RFC 8446 §6.1 says must not look like EOF.
        TlsError::IoError => SessionError::Reset,
        TlsError::InvalidCertificate => SessionError::Certificate,
        TlsError::MissingHandshake => SessionError::Closed,
        TlsError::ConnectionClosed => SessionError::Closed,
        _ => SessionError::Protocol,
    }
}

// ───────────────────────────── helpers ─────────────────────────────

/// Lower-case hex, the form the transcript and `openssl` both print.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Dotted-quad text for the diagnostics (no formatter state needed).
fn ipv4_text(address: Ipv4) -> String {
    let mut out = String::new();
    for (index, byte) in address.iter().enumerate() {
        if index > 0 {
            out.push('.');
        }
        let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{byte}"));
    }
    out
}
