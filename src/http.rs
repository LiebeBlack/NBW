//! http.rs — Native HTTP/1.1 client over a from-scratch TLS 1.2/1.3 client
//! built directly on Schannel (the Windows TLS provider). No curl, no
//! reqwest, no openssl.
//!
//! Pipeline per request:
//!   1. Caller runs the adblock verdict BEFORE any DNS/TCP is attempted.
//!   2. TCP connect through std `TcpStream` (system resolver cache).
//!   3. TLS handshake via SSPI/Schannel for `https://`, plaintext otherwise.
//!   4. HTTP/1.1 GET with a compact parser (status line, headers, chunked).
//!
//! Request headers mirror a modern Chromium/Edge profile (User-Agent,
//! `sec-ch-ua`, `sec-fetch-*`, `accept-language`) so complex platforms
//! (YouTube, Twitch) serve their standard HTML5/VP9 pages to the engine.
//! Content is requested uncompressed (`accept-encoding: identity`): the
//! engine stays tiny by design and the adblock pre-filter removes the vast
//! majority of bytes before they ever reach the socket.

#![allow(non_snake_case)]
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};

/// Chromium/Edge-compatible UA for maximal platform compatibility.
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0";

// ---------------------------------------------------------------------------
// Parsed URL
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(input: &str) -> Option<Url> {
        let input = input.trim();
        let (scheme, rest) = if let Some(r) = input.strip_prefix("https://") {
            ("https", r)
        } else if let Some(r) = input.strip_prefix("http://") {
            ("http", r)
        } else {
            ("https", input)
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h, p.parse::<u16>().ok()?)
            }
            _ => (authority, if scheme == "https" { 443 } else { 80 }),
        };
        Some(Url {
            scheme: scheme.to_string(),
            host: host.to_ascii_lowercase(),
            port,
            path: path.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Schannel FFI (minimal client surface)
// ---------------------------------------------------------------------------
mod schan {
    pub const SECPKG_CRED_OUTBOUND: u32 = 2;
    pub const SCH_CREDENTIALS_VERSION: u32 = 5;
    pub const SCH_USE_STRONG_CRYPTO: u32 = 0x0000_0040;
    pub const SCH_CRED_NO_DEFAULT_CREDS: u32 = 0x0000_0010;
    pub const ISC_REQ_REPLAY_DETECT: u32 = 4;
    pub const ISC_REQ_SEQUENCE_DETECT: u32 = 8;
    pub const ISC_REQ_CONFIDENTIALITY: u32 = 0x10;
    pub const ISC_REQ_ALLOCATE_MEMORY: u32 = 0x100;
    pub const ISC_REQ_STREAM: u32 = 0x8000;
    pub const SEC_E_OK: i32 = 0;
    pub const SEC_I_CONTINUE_NEEDED: i32 = 0x0009_0312;
    pub const SEC_I_INCOMPLETE_MESSAGE: i32 = 0x8009_0318u32 as i32;
    pub const SECBUFFER_VERSION: u32 = 0;
    pub const SECBUFFER_EMPTY: u32 = 0;
    pub const SECBUFFER_DATA: u32 = 1;
    pub const SECBUFFER_TOKEN: u32 = 2;
    pub const SECBUFFER_EXTRA: u32 = 5;
    pub const SECBUFFER_STREAM_TRAILER: u32 = 6;
    pub const SECBUFFER_STREAM_HEADER: u32 = 7;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct SecBuffer {
        pub cbBuffer: u32,
        pub BufferType: u32,
        pub pvBuffer: *mut u8,
    }

    #[repr(C)]
    pub struct SecBufferDesc {
        pub ulVersion: u32,
        pub cBuffers: u32,
        pub pBuffers: *mut SecBuffer,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct SecHandle {
        pub dwLower: usize,
        pub dwUpper: usize,
    }
    pub type CredHandle = SecHandle;

    #[repr(C)]
    pub struct TimeStamp {
        pub LowPart: u32,
        pub HighPart: i32,
    }

    #[repr(C)]
    pub struct SecPkgContext_StreamSizes {
        pub cbHeader: u32,
        pub cbTrailer: u32,
        pub cbMaximumMessage: u32,
        pub cBuffers: u32,
        pub cbBlockSize: u32,
    }

    /// SCH_CREDENTIALS (schannel.h): explicit TLS 1.2/1.3 strong-crypto
    /// credential descriptor. Field order matches the documented layout.
    #[repr(C)]
    pub struct SCH_CREDENTIALS {
        pub dwVersion: u32,
        pub dwCredFormat: u32,
        pub cCreds: u32,
        pub paCred: *mut core::ffi::c_void,
        pub hRootStore: isize,
        pub cMappers: u32,
        pub aphMappers: *mut core::ffi::c_void,
        pub dwSessionLifespanMsec: u32,
        pub dwFlags: u32,
        pub cTlsAlgos: u32,
        pub pTlsAlgos: *mut core::ffi::c_void,
    }

    #[link(name = "secur32")]
    extern "system" {
        pub fn AcquireCredentialsHandleW(
            pszPrincipal: *const u16,
            pszPackage: *const u16,
            fCredentialUse: u32,
            pvLogonID: *const core::ffi::c_void,
            pAuthData: *const core::ffi::c_void,
            pGetKeyFn: *const core::ffi::c_void,
            pvGetKeyArgument: *const core::ffi::c_void,
            phCredential: *mut CredHandle,
            ptsExpiry: *mut TimeStamp,
        ) -> i32;
        pub fn FreeCredentialsHandle(phCredential: *mut CredHandle) -> i32;
        pub fn DeleteSecurityContext(phContext: *mut SecHandle) -> i32;
        pub fn InitializeSecurityContextW(
            phCredential: *mut CredHandle,
            phContext: *mut SecHandle,
            pszTargetName: *const u16,
            fContextReq: u32,
            Reserved1: u32,
            TargetDataRep: u32,
            pInput: *mut SecBufferDesc,
            Reserved2: u32,
            phNewContext: *mut SecHandle,
            pOutput: *mut SecBufferDesc,
            pfContextAttr: *mut u32,
            ptsExpiry: *mut TimeStamp,
        ) -> i32;
        pub fn QueryContextAttributesW(
            phContext: *mut SecHandle,
            ulAttribute: u32,
            pBuffer: *mut core::ffi::c_void,
        ) -> i32;
        pub fn EncryptMessage(
            phContext: *mut SecHandle,
            fQOP: u32,
            pMessage: *mut SecBufferDesc,
            MessageSeqNo: u32,
        ) -> i32;
        pub fn DecryptMessage(
            phContext: *mut SecHandle,
            pMessage: *mut SecBufferDesc,
            MessageSeqNo: u32,
            pfQOP: *mut u32,
        ) -> i32;
        pub fn FreeContextBuffer(pvContextBuffer: *mut core::ffi::c_void) -> i32;
    }

    pub const SECPKG_ATTR_STREAM_SIZES: u32 = 4;

    pub fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(core::iter::once(0)).collect()
    }
}

// ---------------------------------------------------------------------------
// TLS stream
// ---------------------------------------------------------------------------
/// TLS stream over an established TCP connection using Schannel.
pub struct TlsStream {
    stream: TcpStream,
    ctx: schan::SecHandle,
    cred: schan::CredHandle,
    ctx_valid: bool,
    cred_valid: bool,
    sizes: schan::SecPkgContext_StreamSizes,
    /// Decrypted plaintext awaiting consumption.
    plaintext: Vec<u8>,
    plaintext_pos: usize,
    /// Ciphertext read from the wire but not yet decrypted.
    pending: Vec<u8>,
    /// TLS record sequence numbers (encrypt tx / decrypt rx). Schannel
    /// requires the per-record counter; a constant 0 breaks every record
    /// after the first.
    tx_seq: u32,
    rx_seq: u32,
}

impl TlsStream {
    /// Perform the full TLS handshake on `stream` for `host`.
    pub fn handshake(stream: TcpStream, host: &str) -> Result<TlsStream, String> {
        let mut ts = TlsStream {
            stream,
            ctx: schan::SecHandle { dwLower: 0, dwUpper: 0 },
            cred: schan::CredHandle { dwLower: 0, dwUpper: 0 },
            ctx_valid: false,
            cred_valid: false,
            sizes: schan::SecPkgContext_StreamSizes {
                cbHeader: 0,
                cbTrailer: 0,
                cbMaximumMessage: 0,
                cBuffers: 0,
                cbBlockSize: 0,
            },
            plaintext: Vec::with_capacity(16 * 1024),
            plaintext_pos: 0,
            pending: Vec::with_capacity(16 * 1024),
            tx_seq: 0,
            rx_seq: 0,
        };

        // SAFETY: all FFI pointers below reference valid locals; Schannel
        // contracts (buffer ownership, handle lifetimes) are followed and
        // every allocated token is released through FreeContextBuffer.
        unsafe {
            let pkg = schan::to_wide("Schannel");
            let mut expiry = schan::TimeStamp { LowPart: 0, HighPart: 0 };
            // Prefer the explicit strong-crypto credential descriptor
            // (TLS 1.2/1.3, no client cert). Certificate chain validation
            // stays ON: without SCH_CRED_MANUAL_CRED_VALIDATION, Schannel
            // verifies the server chain against the system trust store
            // itself, so a failed handshake (not silent acceptance) is the
            // outcome for an invalid certificate. Schannel builds that
            // reject the descriptor fall back to system-default outbound
            // credentials with identical validation semantics.
            let mut creds = schan::SCH_CREDENTIALS {
                dwVersion: schan::SCH_CREDENTIALS_VERSION,
                dwCredFormat: 0,
                cCreds: 0,
                paCred: core::ptr::null_mut(),
                hRootStore: 0,
                cMappers: 0,
                aphMappers: core::ptr::null_mut(),
                dwSessionLifespanMsec: 3_600_000,
                dwFlags: schan::SCH_USE_STRONG_CRYPTO | schan::SCH_CRED_NO_DEFAULT_CREDS,
                cTlsAlgos: 0,
                pTlsAlgos: core::ptr::null_mut(),
            };
            let st = schan::AcquireCredentialsHandleW(
                core::ptr::null(),
                pkg.as_ptr(),
                schan::SECPKG_CRED_OUTBOUND,
                core::ptr::null(),
                &creds as *const schan::SCH_CREDENTIALS as *const core::ffi::c_void,
                core::ptr::null(),
                core::ptr::null(),
                &mut ts.cred,
                &mut expiry,
            );
            if st != 0 {
                let st2 = schan::AcquireCredentialsHandleW(
                    core::ptr::null(),
                    pkg.as_ptr(),
                    schan::SECPKG_CRED_OUTBOUND,
                    core::ptr::null(),
                    core::ptr::null(),
                    core::ptr::null(),
                    core::ptr::null(),
                    &mut ts.cred,
                    &mut expiry,
                );
                if st2 != 0 {
                    return Err(format!(
                        "AcquireCredentialsHandleW failed: 0x{st:08X}/0x{st2:08X}"
                    ));
                }
            }
            ts.cred_valid = true;

            let target = schan::to_wide(host);
            let mut pending: Vec<u8> = Vec::new();
            let mut first = true;

            loop {
                let mut in_bufs = [
                    schan::SecBuffer {
                        cbBuffer: pending.len() as u32,
                        BufferType: schan::SECBUFFER_TOKEN,
                        pvBuffer: pending.as_mut_ptr(),
                    },
                    schan::SecBuffer {
                        cbBuffer: 0,
                        BufferType: schan::SECBUFFER_EMPTY,
                        pvBuffer: core::ptr::null_mut(),
                    },
                ];
                let mut in_desc = schan::SecBufferDesc {
                    ulVersion: schan::SECBUFFER_VERSION,
                    cBuffers: 2,
                    pBuffers: in_bufs.as_mut_ptr(),
                };
                let mut out_buf = [schan::SecBuffer {
                    cbBuffer: 0,
                    BufferType: schan::SECBUFFER_TOKEN,
                    pvBuffer: core::ptr::null_mut(),
                }];
                let mut out_desc = schan::SecBufferDesc {
                    ulVersion: schan::SECBUFFER_VERSION,
                    cBuffers: 1,
                    pBuffers: out_buf.as_mut_ptr(),
                };
                let mut attrs: u32 = 0;
                let mut new_expiry = schan::TimeStamp { LowPart: 0, HighPart: 0 };

                // First call: no context and no input — Schannel emits the
                // ClientHello. Later calls: continue the existing context.
                let in_desc_ptr = if first {
                    core::ptr::null_mut()
                } else {
                    &mut in_desc
                };
                let (ph_ctx, ph_new) = if ts.ctx_valid {
                    (&mut ts.ctx as *mut schan::SecHandle, &mut ts.ctx as *mut schan::SecHandle)
                } else {
                    (core::ptr::null_mut(), &mut ts.ctx as *mut schan::SecHandle)
                };

                let st = schan::InitializeSecurityContextW(
                    &mut ts.cred,
                    ph_ctx,
                    target.as_ptr(),
                    schan::ISC_REQ_REPLAY_DETECT
                        | schan::ISC_REQ_SEQUENCE_DETECT
                        | schan::ISC_REQ_CONFIDENTIALITY
                        | schan::ISC_REQ_ALLOCATE_MEMORY
                        | schan::ISC_REQ_STREAM,
                    0,
                    0,
                    in_desc_ptr,
                    0,
                    ph_new,
                    &mut out_desc,
                    &mut attrs,
                    &mut new_expiry,
                );
                if !ts.ctx_valid
                    && (st == schan::SEC_E_OK
                        || st == schan::SEC_I_CONTINUE_NEEDED
                        || st == schan::SEC_I_INCOMPLETE_MESSAGE)
                {
                    ts.ctx_valid = true;
                }
                first = false;

                // Flush any output token to the wire.
                if out_buf[0].cbBuffer > 0 && !out_buf[0].pvBuffer.is_null() {
                    let slice = std::slice::from_raw_parts(
                        out_buf[0].pvBuffer,
                        out_buf[0].cbBuffer as usize,
                    );
                    ts.stream.write_all(slice).map_err(|e| e.to_string())?;
                    ts.stream.flush().ok();
                    schan::FreeContextBuffer(out_buf[0].pvBuffer as *mut core::ffi::c_void);
                }

                if st == schan::SEC_E_OK {
                    // Handshake complete. Any unconsumed wire bytes are
                    // early application data (the server may pipeline the
                    // first response records with its Finished) — stash
                    // them or they are lost and the fetch hangs/truncates.
                    for b in in_bufs.iter() {
                        if b.BufferType == schan::SECBUFFER_EXTRA && b.cbBuffer > 0 {
                            let extra = b.cbBuffer as usize;
                            ts.pending = pending[pending.len() - extra..].to_vec();
                        }
                    }
                    break;
                } else if st == schan::SEC_I_INCOMPLETE_MESSAGE {
                    // Schannel needs more wire bytes to parse the record.
                    ts.read_wire_into(&mut pending)?;
                    continue;
                } else if st == schan::SEC_I_CONTINUE_NEEDED {
                    // Input token consumed; keep any EXTRA leftover.
                    let mut extra: usize = 0;
                    for b in in_bufs.iter() {
                        if b.BufferType == schan::SECBUFFER_EXTRA {
                            extra = b.cbBuffer as usize;
                        }
                    }
                    if extra > 0 {
                        let keep = pending[pending.len() - extra..].to_vec();
                        pending = keep;
                    } else {
                        pending.clear();
                    }
                    if pending.is_empty() {
                        ts.read_wire_into(&mut pending)?;
                    }
                } else {
                    return Err(format!("TLS handshake failed: 0x{st:08X}"));
                }
            }

            let st2 = schan::QueryContextAttributesW(
                &mut ts.ctx,
                schan::SECPKG_ATTR_STREAM_SIZES,
                &mut ts.sizes as *mut schan::SecPkgContext_StreamSizes as *mut core::ffi::c_void,
            );
            if st2 != 0 {
                return Err(format!("QueryContextAttributes(STREAM_SIZES): 0x{st2:08X}"));
            }
        }
        Ok(ts)
    }

    fn read_wire_into(&mut self, dst: &mut Vec<u8>) -> Result<(), String> {
        let mut tmp = [0u8; 16384];
        let n = self.stream.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("TLS handshake: connection closed by peer".into());
        }
        dst.extend_from_slice(&tmp[..n]);
        Ok(())
    }

    /// Decrypt ciphertext until plaintext is available, the peer closes the
    /// connection (returns false), or a fatal error occurs. Handles TLS 1.3
    /// post-handshake tickets (OK with no DATA) and partial records
    /// (INCOMPLETE_MESSAGE) without spinning.
    fn fill_decrypted(&mut self) -> Result<bool, String> {
        self.plaintext.clear();
        self.plaintext_pos = 0;
        let mut force_read = false;
        loop {
            if self.pending.is_empty() || force_read {
                let mut tmp = [0u8; 16384];
                let n = self.stream.read(&mut tmp).map_err(|e| e.to_string())?;
                if n == 0 {
                    return Ok(false);
                }
                self.pending.extend_from_slice(&tmp[..n]);
                force_read = false;
            }
            let mut data = std::mem::take(&mut self.pending);
            // SAFETY: buffer vector is owned; Schannel writes within it and
            // reports regions through the SecBuffer array we inspect below.
            let st = unsafe {
                let mut bufs = [
                    schan::SecBuffer {
                        cbBuffer: data.len() as u32,
                        BufferType: schan::SECBUFFER_DATA,
                        pvBuffer: data.as_mut_ptr(),
                    },
                    schan::SecBuffer {
                        cbBuffer: 0,
                        BufferType: schan::SECBUFFER_EMPTY,
                        pvBuffer: core::ptr::null_mut(),
                    },
                    schan::SecBuffer {
                        cbBuffer: 0,
                        BufferType: schan::SECBUFFER_EMPTY,
                        pvBuffer: core::ptr::null_mut(),
                    },
                    schan::SecBuffer {
                        cbBuffer: 0,
                        BufferType: schan::SECBUFFER_EMPTY,
                        pvBuffer: core::ptr::null_mut(),
                    },
                ];
                let mut desc = schan::SecBufferDesc {
                    ulVersion: schan::SECBUFFER_VERSION,
                    cBuffers: 4,
                    pBuffers: bufs.as_mut_ptr(),
                };
                let mut qop: u32 = 0;
                let rc = schan::DecryptMessage(&mut self.ctx, &mut desc, self.rx_seq, &mut qop);
                (rc, bufs)
            };
            let (rc, bufs) = st;
            let mut extra_len: usize = 0;
            for b in bufs.iter() {
                if b.BufferType == schan::SECBUFFER_EXTRA && b.cbBuffer > 0 {
                    extra_len = b.cbBuffer as usize;
                }
            }
            // Retain leftover ciphertext before `data` is consumed.
            if extra_len > 0 {
                let base = data.len() - extra_len;
                self.pending = data[base..].to_vec();
                data.truncate(base);
            } else {
                self.pending.clear();
            }
            if rc == schan::SEC_E_OK {
                self.rx_seq = self.rx_seq.wrapping_add(1);
                let mut got = false;
                for b in bufs.iter() {
                    if b.BufferType == schan::SECBUFFER_DATA && b.cbBuffer > 0 {
                        let start =
                            b.pvBuffer as usize - data.as_ptr() as usize;
                        self.plaintext
                            .extend_from_slice(&data[start..start + b.cbBuffer as usize]);
                        got = true;
                    }
                }
                if got {
                    return Ok(true);
                }
                // TLS 1.3 tickets or renegotiation: loop for real data.
                continue;
            } else if rc == schan::SEC_I_INCOMPLETE_MESSAGE {
                self.pending = data;
                force_read = true;
                continue;
            } else {
                self.pending = data;
                return Err(format!("DecryptMessage failed: 0x{:08X}", rc as u32));
            }
        }
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.plaintext_pos >= self.plaintext.len() {
            match self.fill_decrypted() {
                Ok(true) => {}
                Ok(false) => return Ok(0),
                Err(e) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                }
            }
        }
        let avail = self.plaintext.len() - self.plaintext_pos;
        let n = avail.min(buf.len());
        buf[..n].copy_from_slice(&self.plaintext[self.plaintext_pos..self.plaintext_pos + n]);
        self.plaintext_pos += n;
        Ok(n)
    }
}

impl Write for TlsStream {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.sizes.cbMaximumMessage == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "TLS stream sizes unavailable",
            ));
        }
        let head = self.sizes.cbHeader as usize;
        let trail = self.sizes.cbTrailer as usize;
        let max_msg = self.sizes.cbMaximumMessage as usize;
        let chunk = data.len().min(max_msg);
        let mut msg = vec![0u8; head + chunk + trail];
        msg[head..head + chunk].copy_from_slice(&data[..chunk]);
        // SAFETY: the four buffers describe disjoint regions of `msg`
        // exactly as EncryptMessage requires.
        let rc = unsafe {
            let mut bufs = [
                schan::SecBuffer {
                    cbBuffer: head as u32,
                    BufferType: schan::SECBUFFER_STREAM_HEADER,
                    pvBuffer: msg.as_mut_ptr(),
                },
                schan::SecBuffer {
                    cbBuffer: chunk as u32,
                    BufferType: schan::SECBUFFER_DATA,
                    pvBuffer: msg.as_mut_ptr().add(head),
                },
                schan::SecBuffer {
                    cbBuffer: trail as u32,
                    BufferType: schan::SECBUFFER_STREAM_TRAILER,
                    pvBuffer: msg.as_mut_ptr().add(head + chunk),
                },
                schan::SecBuffer {
                    cbBuffer: 0,
                    BufferType: schan::SECBUFFER_EMPTY,
                    pvBuffer: core::ptr::null_mut(),
                },
            ];
            let mut desc = schan::SecBufferDesc {
                ulVersion: schan::SECBUFFER_VERSION,
                cBuffers: 4,
                pBuffers: bufs.as_mut_ptr(),
            };
            schan::EncryptMessage(&mut self.ctx, 0, &mut desc, self.tx_seq)
        };
        if rc != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("EncryptMessage failed: 0x{rc:08X}"),
            ));
        }
        self.stream.write_all(&msg)?;
        self.stream.flush()?;
        self.tx_seq = self.tx_seq.wrapping_add(1);
        Ok(chunk)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

impl Drop for TlsStream {
    fn drop(&mut self) {
        // SAFETY: handles owned exclusively by this struct.
        unsafe {
            if self.ctx_valid {
                schan::DeleteSecurityContext(&mut self.ctx);
            }
            if self.cred_valid {
                schan::FreeCredentialsHandle(&mut self.cred);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP response
// ---------------------------------------------------------------------------
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Charset-aware decoding: UTF-8 by default, Latin-1 fallback.
    pub fn text(&self) -> String {
        let ct = self.header("content-type").unwrap_or("").to_ascii_lowercase();
        if ct.contains("charset=iso-8859-1") || ct.contains("charset=latin1") {
            return self.body.iter().map(|&b| b as char).collect();
        }
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Modern Chromium/Edge-style header set for maximal platform compat.
pub fn browser_headers(host: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Host", host.to_string()),
        (
            "User-Agent",
            UA.to_string(),
        ),
        (
            "sec-ch-ua",
            "\"Chromium\";v=\"126\", \"Microsoft Edge\";v=\"126\", \"Not=A?Brand\";v=\"24\""
                .to_string(),
        ),
        ("sec-ch-ua-mobile", "?0".to_string()),
        ("sec-ch-ua-platform", "\"Windows\"".to_string()),
        ("upgrade-insecure-requests", "1".to_string()),
        (
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"
                .to_string(),
        ),
        ("sec-fetch-dest", "document".to_string()),
        ("sec-fetch-mode", "navigate".to_string()),
        ("sec-fetch-site", "none".to_string()),
        ("sec-fetch-user", "?1".to_string()),
        ("accept-encoding", "identity".to_string()),
        (
            "accept-language",
            "en-US,en;q=0.9,es;q=0.8".to_string(),
        ),
    ]
}

/// Blocking fetch of a URL. The caller applies the adblock verdict first.
pub fn get(url: &str, extra_headers: &[(&str, &str)]) -> Result<HttpResponse, String> {
    get_impl(url, extra_headers, 0)
}

const MAX_REDIRECTS: usize = 5;

fn get_impl(
    url: &str,
    extra_headers: &[(&str, &str)],
    depth: usize,
) -> Result<HttpResponse, String> {
    let parsed = Url::parse(url).ok_or_else(|| "invalid URL".to_string())?;
    let stream = connect_with_timeout(&parsed)?;
    stream.set_nodelay(true).ok();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(15)))
        .ok();

    let mut request = format!("GET {} HTTP/1.1\r\n", parsed.path);
    for (k, v) in browser_headers(&parsed.host) {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    for (k, v) in extra_headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");

    let mut raw = Vec::with_capacity(64 * 1024);
    let mut buf = [0u8; 16384];
    if parsed.scheme == "https" {
        let mut tls = TlsStream::handshake(stream, &parsed.host)?;
        tls.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        tls.flush().ok();
        loop {
            match tls.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.len() > 8 * 1024 * 1024 {
                        break;
                    }
                }
                Err(e) => return Err(format!("tls read: {e}")),
            }
        }
    } else {
        let mut plain = stream;
        plain.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        plain.flush().ok();
        loop {
            match plain.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.len() > 8 * 1024 * 1024 {
                        break;
                    }
                }
                Err(e) => return Err(format!("read: {e}")),
            }
        }
    }
    let resp = parse_response(&raw)?;
    if depth < MAX_REDIRECTS && matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
        if let Some(loc) = resp.header("location").map(|l| l.to_string()) {
            let next = resolve_location(&loc, &parsed);
            return get_impl(&next, &[], depth + 1);
        }
    }
    Ok(resp)
}

/// TCP connect honoring a 10 s per-address timeout (multi-address hosts).
fn connect_with_timeout(parsed: &Url) -> Result<TcpStream, String> {
    let addrs: Vec<std::net::SocketAddr> = (parsed.host.as_str(), parsed.port)
        .to_socket_addrs()
        .map_err(|e| format!("dns: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err("dns: no addresses".into());
    }
    let mut last_err = String::from("connect failed");
    for a in addrs {
        match TcpStream::connect_timeout(&a, std::time::Duration::from_secs(10)) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = format!("connect {a}: {e}"),
        }
    }
    Err(last_err)
}

/// Join a Location header against the request URL.
fn resolve_location(loc: &str, base: &Url) -> String {
    if loc.contains("://") {
        return loc.to_string();
    }
    if let Some(rest) = loc.strip_prefix("//") {
        return format!("{}:{rest}", base.scheme);
    }
    if loc.starts_with('/') {
        return format!("{}://{}{}", base.scheme, base.host, loc);
    }
    // Relative path: strip the last path segment of the base.
    let dir = match base.path.rfind('/') {
        Some(i) => &base.path[..i + 1],
        None => "/",
    };
    format!("{}://{}{}{}", base.scheme, base.host, dir, loc)
}

/// Parse an HTTP/1.1 response including `Transfer-Encoding: chunked`.
pub fn parse_response(raw: &[u8]) -> Result<HttpResponse, String> {
    let split = find_header_end(raw).ok_or("malformed response: no header terminator")?;
    let head = String::from_utf8_lossy(&raw[..split.0]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.split(' ');
    let _ver = parts.next().unwrap_or("");
    let status: u16 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding")
            && v.to_ascii_lowercase().contains("chunked")
    });
    let body = if chunked {
        dechunk(&raw[split.1..])
    } else {
        raw[split.1..].to_vec()
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Find the `\r\n\r\n` boundary. Returns (head_end, body_start).
fn find_header_end(raw: &[u8]) -> Option<(usize, usize)> {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, i + 4))
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let Some(line_end) = find_crlf(data) else { break };
        let size_str = String::from_utf8_lossy(&data[..line_end]);
        let size_hex = size_str.split(';').next().unwrap_or("0").trim();
        let size = usize::from_str_radix(size_hex, 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        if start + size > data.len() {
            out.extend_from_slice(&data[start.min(data.len())..]);
            break;
        }
        out.extend_from_slice(&data[start..start + size]);
        data = &data[(start + size).min(data.len())..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        }
    }
    out
}

fn find_crlf(data: &[u8]) -> Option<usize> {
    data.windows(2).position(|w| w == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let u = Url::parse("https://example.com:8443/a/b?c=d").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 8443);
        assert_eq!(u.path, "/a/b?c=d");
        let u2 = Url::parse("http://foo.org").unwrap();
        assert_eq!(u2.port, 80);
        assert_eq!(u2.path, "/");
        let u3 = Url::parse("example.org/x").unwrap();
        assert_eq!(u3.scheme, "https");
        assert_eq!(u3.host, "example.org");
    }

    #[test]
    fn response_parsing_plain() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h1>hi</h1>";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.header("content-type"), Some("text/html"));
        assert_eq!(r.body, b"<h1>hi</h1>");
    }

    #[test]
    fn response_parsing_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.body, b"Wikipedia");
    }

    #[test]
    fn resolve_location_joins_paths() {
        let base = Url::parse("https://a.b/c/d/e").unwrap();
        assert_eq!(resolve_location("/x", &base), "https://a.b/x");
        assert_eq!(resolve_location("f", &base), "https://a.b/c/d/f");
        assert_eq!(resolve_location("https://q.r/z", &base), "https://q.r/z");
    }

    #[test]
    fn edge_compatible_headers_present() {
        let hs = browser_headers("example.com");
        assert!(hs.iter().any(|(k, _)| *k == "sec-fetch-mode"));
        assert!(hs.iter().any(|(k, _)| *k == "sec-ch-ua"));
        assert!(hs.iter().any(|(k, v)| *k == "Host" && v == "example.com"));
    }
}
