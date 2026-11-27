// This file is derived from OpenSSH, modified: a Rust port of parts of hostfile.c,
// hostfile.h, sshkey.c, ssh-ed25519.c, ssh-ed25519-sk.c, ssh-mldsa-eddsa.c, ssh-ecdsa.c,
// ssh-ecdsa-sk.c, ssh-rsa.c, sshbuf-getput-basic.c, sshbuf-getput-crypto.c, sshbuf-misc.c,
// sshbuf.h, sshkey.h, ssh2.h, crypto_api.h, misc.c and sshconnect.c of OpenSSH portable 10.6p2
// (commit
// d449e662b6b8b8cb20e852d8084c269af1c5e8f1). The notices of those files follow as written
// there; LICENSE-OpenSSH has the OpenSSH licence.
//
// hostfile.c:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  * Functions for manipulating the known hosts files.
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  *
//  *
//  * Copyright (c) 1999, 2000 Markus Friedl.  All rights reserved.
//  * Copyright (c) 1999 Niels Provos.  All rights reserved.
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
//  * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//  * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//  * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
//  * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//  * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//  * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//  * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//  * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//  * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//  */
//
// hostfile.h:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  */
//
// sshkey.c:
// /*
//  * Copyright (c) 2000, 2001 Markus Friedl.  All rights reserved.
//  * Copyright (c) 2008 Alexander von Gernler.  All rights reserved.
//  * Copyright (c) 2010,2011 Damien Miller.  All rights reserved.
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
//  * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//  * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//  * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
//  * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//  * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//  * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//  * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//  * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//  * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//  */
//
// ssh-ed25519.c:
// /*
//  * Copyright (c) 2013 Markus Friedl <markus@openbsd.org>
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//
// ssh-ed25519-sk.c:
// /*
//  * Copyright (c) 2019 Markus Friedl.  All rights reserved.
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//
// ssh-mldsa-eddsa.c:
// /*
//  * Copyright (c) 2026 Damien Miller <djm@mindrot.org>
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//
// ssh-ecdsa.c:
// /*
//  * Copyright (c) 2000 Markus Friedl.  All rights reserved.
//  * Copyright (c) 2010 Damien Miller.  All rights reserved.
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
//  * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//  * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//  * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
//  * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//  * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//  * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//  * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//  * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//  * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//  */
//
// ssh-ecdsa-sk.c:
// /*
//  * Copyright (c) 2000 Markus Friedl.  All rights reserved.
//  * Copyright (c) 2010 Damien Miller.  All rights reserved.
//  * Copyright (c) 2019 Google Inc.  All rights reserved.
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
//  * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//  * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//  * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
//  * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//  * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//  * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//  * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//  * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//  * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//  */
//
// ssh-rsa.c:
// /*
//  * Copyright (c) 2000, 2003 Markus Friedl <markus@openbsd.org>
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//
// sshbuf-getput-basic.c:
// /*
//  * Copyright (c) 2011 Damien Miller
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//
// sshbuf-getput-crypto.c:
// /*
//  * Copyright (c) 2011 Damien Miller
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//
// sshbuf-misc.c:
// /*
//  * Copyright (c) 2011 Damien Miller
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//
// sshbuf.h:
// /*
//  * Copyright (c) 2011 Damien Miller
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//
// sshkey.h:
// /*
//  * Copyright (c) 2000, 2001 Markus Friedl.  All rights reserved.
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
//  * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//  * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//  * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
//  * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//  * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//  * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//  * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//  * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//  * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//  */
//
// ssh2.h:
// /*
//  * Copyright (c) 2000 Markus Friedl.  All rights reserved.
//  *
//  * Redistribution and use in source and binary forms, with or without
//  * modification, are permitted provided that the following conditions
//  * are met:
//  * 1. Redistributions of source code must retain the above copyright
//  *    notice, this list of conditions and the following disclaimer.
//  * 2. Redistributions in binary form must reproduce the above copyright
//  *    notice, this list of conditions and the following disclaimer in the
//  *    documentation and/or other materials provided with the distribution.
//  *
//  * THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
//  * IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
//  * OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
//  * IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT,
//  * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
//  * NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//  * DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//  * THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//  * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
//  * THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//  */
//
// crypto_api.h:
// /*
//  * Assembled from generated headers and source files by Markus Friedl.
//  * Placed in the public domain.
//  */
//
// misc.c:
// /*
//  * Copyright (c) 2000 Markus Friedl.  All rights reserved.
//  * Copyright (c) 2005-2020 Damien Miller.  All rights reserved.
//  * Copyright (c) 2004 Henning Brauer <henning@openbsd.org>
//  *
//  * Permission to use, copy, modify, and distribute this software for any
//  * purpose with or without fee is hereby granted, provided that the above
//  * copyright notice and this permission notice appear in all copies.
//  *
//  * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
//  * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
//  * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
//  * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
//  * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
//  * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
//  * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//  */
//
// sshconnect.c:
// /*
//  * Author: Tatu Ylonen <ylo@cs.hut.fi>
//  * Copyright (c) 1995 Tatu Ylonen <ylo@cs.hut.fi>, Espoo, Finland
//  *                    All rights reserved
//  * Code to connect to a remote host, and to perform the client side of the
//  * login (authentication) dialog.
//  *
//  * As far as I am concerned, the code I have written for this software
//  * can be used freely for any purpose.  Any derived versions of this
//  * software must be clearly marked as such, and if the derived work is
//  * incompatible with the protocol description in the RFC file, it must be
//  * called by a name other than "ssh" or "Secure Shell".
//  */
//! known_hosts files and the host key verdict (docs/design/ssh-backends.md "호스트 키 판정").
//!
//! [`KnownHosts::load`] reads the files, [`KnownHosts::entries`] gives the lines that name a
//! host the way `hostkeys_foreach_file` (`hostfile.c:789`-`:963`) reads them with
//! `HKF_WANT_MATCH`, and [`KnownHosts::check`] judges a host key the way
//! `check_hostkeys_by_key_or_type` (`hostfile.c:349`-`:405`) does with the keys `ssh` loads
//! (`load_hostkeys_file` and `load_hostkeys`, `hostfile.c:265`-`:298`).
//!
//! A key is read as `sshkey_read` (`sshkey.c:1298`-`:1370`) does: the type name, the base64
//! blob and the blob (`sshkey_from_blob_internal`, the `deserialize_public` of each key type,
//! and for a certificate the fields of `cert_parse`). The checks that need the curve
//! arithmetic or a signature are not made: an ECDSA point is checked for its form and length
//! but not for being on the curve (`EC_POINT_oct2point`, `sshkey_ec_validate_public`), and the
//! CA signature of a certificate is not verified (`sshkey_verify` in `cert_parse`). Such a
//! key reads as a key here and is invalid to `ssh` (docs/design/ssh-backends.md "OpenSSH 해석
//! 이식 크레이트" has which way each case moves the verdict). Keys are compared by type and
//! public part, as `sshkey_equal` and `sshkey_equal_public` (`sshkey.c:821`-`:844`) do.
//!
//! `@cert-authority` lines are never a reason to accept a key (docs/design/ssh-backends.md
//! "호스트 키 판정"): they are reported as [`HostKeyVerdict::DistrustedCaLine`] when nothing
//! else names the host.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit as _, Mac as _};

use crate::matching::{ListMatch, match_hostname_bytes};
use crate::misc::is_c_space;
use crate::readconf::DEFAULT_SSH_PORT;

/// `HASH_MAGIC` (`hostfile.h:59`).
const HASH_MAGIC: &[u8] = b"|1|";
/// `HASH_DELIM` (`hostfile.h:60`).
const HASH_DELIM: u8 = b'|';
/// `CA_MARKER` (`hostfile.h:62`).
const CA_MARKER: &[u8] = b"@cert-authority";
/// `REVOKE_MARKER` (`hostfile.h:63`).
const REVOKE_MARKER: &[u8] = b"@revoked";
/// `char marker[32]` of `check_markers` (`hostfile.c:183`): a marker is shorter.
const MARKER_BUFFER: usize = 32;
/// `char ktype[128]` of `hostkeys_foreach_file` (`hostfile.c:793`): a type name is shorter.
const KEY_TYPE_BUFFER: usize = 128;
/// The longest base64 salt `extract_salt` takes (`hostfile.c:92`).
const SALT_BASE64_MAX: usize = 1024;
/// `ssh_hmac_bytes(SSH_DIGEST_SHA1)`: the length of the salt and of the hash.
const SHA1_BYTES: usize = 20;
/// `ED25519_PK_SZ` (`sshkey.h:141`, `crypto_sign_ed25519_PUBLICKEYBYTES` of `crypto_api.h:60`).
const ED25519_PUBLIC_BYTES: usize = 32;
/// `MLDSA44_ED25519_PK_SZ` (`crypto_api.h:187`: 1312 bytes of ML-DSA-44 and 32 of Ed25519).
const MLDSA44_ED25519_PUBLIC_BYTES: usize = 1344;
/// `SSHBUF_MAX_BIGNUM` (`sshbuf.h:41`), in bytes.
const MAX_BIGNUM_BYTES: usize = 2048;
/// `SSHBUF_MAX_ECPOINT` (`sshbuf.h:42`).
const MAX_EC_POINT_BYTES: usize = 133;
/// `POINT_CONVERSION_UNCOMPRESSED`: the only point form `get_ec` takes
/// (`sshbuf-getput-crypto.c:62`-`:64`).
const EC_POINT_UNCOMPRESSED: u8 = 0x04;
/// The sign bit of the first byte of an SSH `mpint`.
const MPINT_SIGN_BIT: u8 = 0x80;
/// `SSH_RSA_MINIMUM_MODULUS_SIZE` (`sshkey.h:47`), in bits.
const RSA_MINIMUM_MODULUS_BITS: usize = 1024;
/// Bits in a byte.
const BITS_PER_BYTE: usize = 8;
/// The `uint32` length before an SSH string.
const STRING_LENGTH_BYTES: usize = 4;
/// An SSH `uint32` (`sshbuf_get_u32`).
const U32_BYTES: usize = 4;
/// An SSH `uint64` (`sshbuf_get_u64`).
const U64_BYTES: usize = 8;
/// `SSHKEY_CERT_MAX_PRINCIPALS` (`sshkey.h:96`).
const CERT_MAX_PRINCIPALS: usize = 256;
/// `SSH2_CERT_TYPE_USER` (`ssh2.h:179`).
const CERT_TYPE_USER: u32 = 1;
/// `SSH2_CERT_TYPE_HOST` (`ssh2.h:180`).
const CERT_TYPE_HOST: u32 = 2;
/// `SSHBUF_SIZE_MAX` (`sshbuf.h:39`): the largest buffer, so the largest decoded key blob
/// (`sshbuf_b64tod`, `sshbuf-misc.c:183`-`:203`).
const SSHBUF_SIZE_MAX: usize = 0x0800_0000;

/// The NIST curves of ECDSA keys (`sshkey_curve_name_to_nid`, `sshkey.c:569`-`:580`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Curve {
    P256,
    P384,
    P521,
}

impl Curve {
    /// The curve name in a key blob.
    fn name(self) -> &'static [u8] {
        match self {
            Curve::P256 => b"nistp256",
            Curve::P384 => b"nistp384",
            Curve::P521 => b"nistp521",
        }
    }

    /// The bytes of one coordinate: an uncompressed point is `0x04`, x and y.
    fn field_bytes(self) -> usize {
        const P256: usize = 32;
        const P384: usize = 48;
        const P521: usize = 66;
        match self {
            Curve::P256 => P256,
            Curve::P384 => P384,
            Curve::P521 => P521,
        }
    }
}

/// The plain key type (`KEY_*` without the certificate, with the curve of ECDSA).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ed25519,
    Ed25519Sk,
    MlDsa44Ed25519,
    Ecdsa(Curve),
    /// `KEY_ECDSA_SK`, always on P-256.
    EcdsaSk,
    Rsa,
}

/// One entry of `keyimpls[]` (`sshkey.c:120`): the name, `shortname`, type and `cert`.
struct KeyImpl {
    name: &'static [u8],
    short: &'static str,
    kind: Kind,
    cert: bool,
}

const fn imp(name: &'static [u8], short: &'static str, kind: Kind, cert: bool) -> KeyImpl {
    KeyImpl {
        name,
        short,
        kind,
        cert,
    }
}

/// `keyimpls[]` of a build with security keys, ML-DSA and OpenSSL, in its order
/// (`ssh-ed25519.c:289`-`:310`, `ssh-ed25519-sk.c:251`-`:272`, `ssh-mldsa-eddsa.c:465`-`:486`,
/// `ssh-ecdsa.c:507`-`:576`, `ssh-ecdsa-sk.c:444`-`:489`, `ssh-rsa.c:594`-`:665`).
const KEY_IMPLS: &[KeyImpl] = &[
    imp(b"ssh-ed25519", "ED25519", Kind::Ed25519, false),
    imp(
        b"ssh-ed25519-cert-v01@openssh.com",
        "ED25519-CERT",
        Kind::Ed25519,
        true,
    ),
    imp(
        b"sk-ssh-ed25519@openssh.com",
        "ED25519-SK",
        Kind::Ed25519Sk,
        false,
    ),
    imp(
        b"sk-ssh-ed25519-cert-v01@openssh.com",
        "ED25519-SK-CERT",
        Kind::Ed25519Sk,
        true,
    ),
    imp(
        b"ssh-mldsa44-ed25519",
        "MLDSA44-ED25519",
        Kind::MlDsa44Ed25519,
        false,
    ),
    imp(
        b"ssh-mldsa44-ed25519-cert",
        "MLDSA44-ED25519-CERT",
        Kind::MlDsa44Ed25519,
        true,
    ),
    imp(
        b"ecdsa-sha2-nistp256",
        "ECDSA",
        Kind::Ecdsa(Curve::P256),
        false,
    ),
    imp(
        b"ecdsa-sha2-nistp256-cert-v01@openssh.com",
        "ECDSA-CERT",
        Kind::Ecdsa(Curve::P256),
        true,
    ),
    imp(
        b"ecdsa-sha2-nistp384",
        "ECDSA",
        Kind::Ecdsa(Curve::P384),
        false,
    ),
    imp(
        b"ecdsa-sha2-nistp384-cert-v01@openssh.com",
        "ECDSA-CERT",
        Kind::Ecdsa(Curve::P384),
        true,
    ),
    imp(
        b"ecdsa-sha2-nistp521",
        "ECDSA",
        Kind::Ecdsa(Curve::P521),
        false,
    ),
    imp(
        b"ecdsa-sha2-nistp521-cert-v01@openssh.com",
        "ECDSA-CERT",
        Kind::Ecdsa(Curve::P521),
        true,
    ),
    imp(
        b"sk-ecdsa-sha2-nistp256@openssh.com",
        "ECDSA-SK",
        Kind::EcdsaSk,
        false,
    ),
    imp(
        b"sk-ecdsa-sha2-nistp256-cert-v01@openssh.com",
        "ECDSA-SK-CERT",
        Kind::EcdsaSk,
        true,
    ),
    imp(
        b"webauthn-sk-ecdsa-sha2-nistp256@openssh.com",
        "ECDSA-SK",
        Kind::EcdsaSk,
        false,
    ),
    imp(
        b"webauthn-sk-ecdsa-sha2-nistp256-cert-v01@openssh.com",
        "ECDSA-SK-CERT",
        Kind::EcdsaSk,
        true,
    ),
    imp(b"ssh-rsa", "RSA", Kind::Rsa, false),
    imp(b"ssh-rsa-cert-v01@openssh.com", "RSA-CERT", Kind::Rsa, true),
    imp(b"rsa-sha2-256", "RSA", Kind::Rsa, false),
    imp(
        b"rsa-sha2-256-cert-v01@openssh.com",
        "RSA-CERT",
        Kind::Rsa,
        true,
    ),
    imp(b"rsa-sha2-512", "RSA", Kind::Rsa, false),
    imp(
        b"rsa-sha2-512-cert-v01@openssh.com",
        "RSA-CERT",
        Kind::Rsa,
        true,
    ),
];

/// `sshkey_type_from_name` and `peek_type_nid` (`sshkey.c:248`-`:252`, `:1277`-`:1295`): the
/// full names only.
fn impl_by_name(name: &[u8]) -> Option<&'static KeyImpl> {
    KEY_IMPLS.iter().find(|i| i.name == name)
}

/// A public key read from a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    kind: Kind,
    cert: bool,
    /// `shortname` of its type (`sshkey_type`).
    short: &'static str,
    /// The public fields as `deserialize_public` keeps them (an `mpint` without its leading
    /// zero bytes).
    public: Vec<Vec<u8>>,
    /// The whole blob (a certificate is compared by it, `cert_compare`).
    blob: Vec<u8>,
    /// The CA key of a certificate (`signature_key`).
    ca: Option<Box<Key>>,
}

impl Key {
    /// `sshkey_equal`: the same type and public part, and for a certificate the same
    /// certificate.
    fn equal(&self, other: &Key) -> bool {
        self.kind == other.kind
            && self.cert == other.cert
            && self.public == other.public
            && (!self.cert || self.blob == other.blob)
    }

    /// `sshkey_equal_public`: the same plain type and public part (a certificate and its
    /// plain key are equal).
    fn equal_public(&self, other: &Key) -> bool {
        self.kind == other.kind && self.public == other.public
    }
}

/// The reader of a blob (`sshbuf_get_*`).
struct Buf<'a>(&'a [u8]);

impl<'a> Buf<'a> {
    /// `n` bytes (`sshbuf_get_u32`, `sshbuf_get_u64`).
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (value, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(value)
    }

    /// `sshbuf_get_string_direct`.
    fn string(&mut self) -> Option<&'a [u8]> {
        let (len, rest) = self.0.split_at_checked(STRING_LENGTH_BYTES)?;
        let len = u32::from_be_bytes(len.try_into().ok()?);
        let (value, rest) = rest.split_at_checked(usize::try_from(len).ok()?)?;
        self.0 = rest;
        Some(value)
    }

    /// `sshbuf_get_cstring`: a NUL only at the end, which the C string then drops.
    fn cstring(&mut self) -> Option<&'a [u8]> {
        let value = self.string()?;
        match value.iter().position(|c| *c == b'\0') {
            Some(at) if at + 1 < value.len() => None,
            Some(at) => value.get(..at),
            None => Some(value),
        }
    }

    /// `sshbuf_get_bignum2_bytes_direct` (`sshbuf-getput-basic.c:597`-`:631`): not negative,
    /// not longer than `SSHBUF_MAX_BIGNUM` (one more byte when it is a leading zero), the
    /// leading zero bytes trimmed.
    fn bignum(&mut self) -> Option<&'a [u8]> {
        let value = self.string()?;
        let first = value.first().copied();
        if first.is_some_and(|c| c & MPINT_SIGN_BIT != b'\0') {
            return None;
        }
        if value.len() > MAX_BIGNUM_BYTES + 1
            || (value.len() == MAX_BIGNUM_BYTES + 1 && first != Some(b'\0'))
        {
            return None;
        }
        let start = value
            .iter()
            .position(|c| *c != b'\0')
            .unwrap_or(value.len());
        value.get(start..)
    }
}

/// `ssh_ed25519_deserialize_public` (`ssh-ed25519.c:102`-`:118`).
fn ed25519(b: &mut Buf<'_>) -> Option<Vec<u8>> {
    let pk = b.string()?;
    (pk.len() == ED25519_PUBLIC_BYTES).then(|| pk.to_vec())
}

/// `ssh_ecdsa_deserialize_public` (`ssh-ecdsa.c:224`-`:275`) without
/// `sshkey_ec_validate_public`: the curve name must be the curve of the type, and the point
/// is `get_ec` (`sshbuf-getput-crypto.c:57`-`:68`): uncompressed and of the length
/// `EC_POINT_oct2point` takes for the curve.
fn ecdsa(b: &mut Buf<'_>, curve: Curve) -> Option<Vec<Vec<u8>>> {
    let name = b.cstring()?;
    if name != curve.name() {
        return None;
    }
    let point = b.string()?;
    if point.is_empty() || point.len() > MAX_EC_POINT_BYTES {
        return None;
    }
    if point.first() != Some(&EC_POINT_UNCOMPRESSED) || point.len() != 1 + 2 * curve.field_bytes() {
        return None;
    }
    Some(vec![name.to_vec(), point.to_vec()])
}

/// The bits of a trimmed unsigned big-endian number (`BN_num_bits`).
fn bits(n: &[u8]) -> usize {
    n.first().map_or(Default::default(), |c| {
        (n.len() - 1) * BITS_PER_BYTE
            + (BITS_PER_BYTE - usize::try_from(c.leading_zeros()).unwrap_or(BITS_PER_BYTE))
    })
}

/// `sshkey_from_blob_internal` with certificates allowed (`sshkey.c:2004`-`:2077`), with
/// [`cert_body`] for a certificate.
fn parse_blob(blob: &[u8]) -> Option<Key> {
    parse_blob_internal(blob, true)
}

/// `sshkey_from_blob_internal`; `allow_cert` as there (a CA key is read without it).
fn parse_blob_internal(blob: &[u8], allow_cert: bool) -> Option<Key> {
    let mut b = Buf(blob);
    let name = b.cstring()?;
    let imp = impl_by_name(name)?;
    if imp.cert && !allow_cert {
        return None;
    }
    if imp.cert {
        // The nonce before every certificate.
        b.string()?;
    }
    let public = match imp.kind {
        Kind::Ed25519 => vec![ed25519(&mut b)?],
        // `ssh_ed25519_sk_deserialize_public` (`ssh-ed25519-sk.c:96`-`:107`) and
        // `sshkey_deserialize_sk` (`sshkey.c:1995`-`:2002`): the application.
        Kind::Ed25519Sk => vec![ed25519(&mut b)?, b.cstring()?.to_vec()],
        // `ssh_mldsa44_ed25519_deserialize_public` (`ssh-mldsa-eddsa.c:278`-`:294`).
        Kind::MlDsa44Ed25519 => {
            let pk = b.string()?;
            if pk.len() != MLDSA44_ED25519_PUBLIC_BYTES {
                return None;
            }
            vec![pk.to_vec()]
        }
        Kind::Ecdsa(curve) => ecdsa(&mut b, curve)?,
        // `ssh_ecdsa_sk_deserialize_public` (`ssh-ecdsa-sk.c:113`-`:124`).
        Kind::EcdsaSk => {
            let mut v = ecdsa(&mut b, Curve::P256)?;
            v.push(b.cstring()?.to_vec());
            v
        }
        // `ssh_rsa_deserialize_public` (`ssh-rsa.c:192`-`:229`) and
        // `sshkey_check_rsa_length` (`sshkey.c:1454`-`:1469`).
        Kind::Rsa => {
            let e = b.bignum()?;
            let n = b.bignum()?;
            if bits(n) < RSA_MINIMUM_MODULUS_BITS {
                return None;
            }
            vec![e.to_vec(), n.to_vec()]
        }
    };
    let ca = if imp.cert {
        Some(Box::new(cert_body(&mut b)?))
    } else {
        None
    };
    if !b.0.is_empty() {
        return None;
    }
    Some(Key {
        kind: imp.kind,
        cert: imp.cert,
        short: imp.short,
        public,
        blob: blob.to_vec(),
        ca,
    })
}

/// `cert_parse` (`sshkey.c:1854`-`:1993`) up to the signature check: the body fields, the
/// principals (at most [`CERT_MAX_PRINCIPALS`]), the critical options and extensions as
/// pairs of strings, the CA key (a key that is not a certificate), the certificate type
/// (user or host) and the signature type name. `sshkey_verify` of the CA signature
/// (`:1976`) is not made: that needs the signature algorithms, and the CA is not trusted
/// here (docs/design/ssh-backends.md "호스트 키 판정"). The CA key is returned.
fn cert_body(b: &mut Buf<'_>) -> Option<Key> {
    b.take(U64_BYTES)?;
    let kind = u32::from_be_bytes(b.take(U32_BYTES)?.try_into().ok()?);
    b.cstring()?;
    let principals = b.string()?;
    b.take(U64_BYTES)?;
    b.take(U64_BYTES)?;
    let critical = b.string()?;
    let extensions = b.string()?;
    b.string()?;
    let ca = b.string()?;
    let signature = b.string()?;
    // `sshkey_get_sigtype` (`sshkey.c:2119`-`:2142`).
    Buf(signature).cstring()?;
    let ca = parse_blob_internal(ca, false)?;
    if ca.cert {
        return None;
    }
    if kind != CERT_TYPE_USER && kind != CERT_TYPE_HOST {
        return None;
    }
    let mut p = Buf(principals);
    for () in std::iter::repeat_n((), CERT_MAX_PRINCIPALS) {
        if p.0.is_empty() {
            break;
        }
        p.cstring()?;
    }
    if !p.0.is_empty() {
        return None;
    }
    for section in [critical, extensions] {
        let mut s = Buf(section);
        while !s.0.is_empty() {
            s.string()?;
            s.string()?;
        }
    }
    Some(ca)
}

/// `b64_pton`: whitespace (`isspace`) anywhere is skipped, the padding is required and the
/// bits after the last byte must be zero.
fn b64_pton(text: &[u8]) -> Option<Vec<u8>> {
    let kept: Vec<u8> = text.iter().copied().filter(|c| !is_c_space(*c)).collect();
    STANDARD.decode(kept).ok()
}

/// `sshbuf_b64tod`: [`b64_pton`] into a buffer, which takes at most [`SSHBUF_SIZE_MAX`]
/// bytes.
fn b64_to_buffer(text: &[u8]) -> Option<Vec<u8>> {
    b64_pton(text).filter(|d| fits_buffer(d.len()))
}

/// Whether `len` bytes fit an `sshbuf` ([`SSHBUF_SIZE_MAX`]).
fn fits_buffer(len: usize) -> bool {
    len <= SSHBUF_SIZE_MAX
}

/// `extract_salt` (`hostfile.c:68`-`:113`).
fn extract_salt(name: &[u8]) -> Option<Vec<u8>> {
    let rest = name.strip_prefix(HASH_MAGIC)?;
    let end = rest.iter().position(|c| *c == HASH_DELIM)?;
    let encoded = rest.get(..end)?;
    if encoded.is_empty() || encoded.len() > SALT_BASE64_MAX {
        return None;
    }
    let salt = b64_pton(encoded)?;
    (salt.len() == SHA1_BYTES).then_some(salt)
}

/// `host_hash` with the salt of a known_hosts name (`hostfile.c:115`-`:150`): `|1|`, the
/// base64 salt, `|` and the base64 HMAC-SHA1 of the host under that salt.
fn host_hash(host: &str, name: &[u8]) -> Option<String> {
    let salt = extract_salt(name)?;
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&salt).ok()?;
    mac.update(host.as_bytes());
    let hash = mac.finalize().into_bytes();
    Some(format!(
        "|1|{}|{}",
        STANDARD.encode(&salt),
        STANDARD.encode(hash)
    ))
}

/// `match_maybe_hashed` (`hostfile.c:769`-`:787`): `None` is a bad hash (`-1`).
fn host_matches(host: &str, names: &[u8]) -> Option<bool> {
    if names.first() == Some(&HASH_DELIM) {
        let hashed = host_hash(host, names)?;
        return Some(hashed.as_bytes() == names);
    }
    // The host field is matched as bytes, as `match_hostname` sees it in C.
    Some(match_hostname_bytes(host, names) == ListMatch::Positive)
}

fn is_blank(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

fn skip_blank(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|c| !is_blank(*c)).unwrap_or(s.len());
    s.get(start..).unwrap_or_default()
}

/// `strcspn(s, " \t")`.
fn word_end(s: &[u8]) -> usize {
    s.iter().position(|c| is_blank(*c)).unwrap_or(s.len())
}

/// The marker of a known_hosts line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Marker {
    /// No marker: the line gives a key of the host.
    None,
    /// `@revoked`: the key is revoked for the hosts of the line.
    Revoked,
    /// `@cert-authority`: the key is a certificate authority for the hosts of the line. This
    /// crate does not trust such lines.
    CertAuthority,
}

/// `check_markers` (`hostfile.c:180`-`:213`): `None` is `MRK_ERROR`. A marker ends at the
/// first space of the line, or the first tab when there is no space.
fn check_markers(mut cp: &[u8]) -> Option<(Marker, &[u8])> {
    let mut marker = Marker::None;
    while cp.first() == Some(&b'@') {
        if marker != Marker::None {
            return None;
        }
        let sp = cp
            .iter()
            .position(|c| *c == b' ')
            .or_else(|| cp.iter().position(|c| *c == b'\t'))?;
        if sp <= 1 || sp >= MARKER_BUFFER {
            return None;
        }
        marker = match cp.get(..sp)? {
            CA_MARKER => Marker::CertAuthority,
            REVOKE_MARKER => Marker::Revoked,
            _ => return None,
        };
        cp = skip_blank(cp.get(sp..)?);
    }
    Some((marker, cp))
}

/// The key of a known_hosts line as `ssh` reads it (`sshkey_read`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LineKey {
    /// The key blob.
    pub blob: Vec<u8>,
    /// The type as `sshkey_type` names it (`ED25519`, `ECDSA`, `RSA`, `ED25519-CERT`, ...).
    pub short_type: &'static str,
    /// The text after the key.
    pub comment: String,
}

/// A known_hosts line whose host field names the host of a query.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostKeyEntry {
    /// The file.
    pub file: PathBuf,
    /// The line, from 1.
    pub line: usize,
    /// The marker.
    pub marker: Marker,
    /// The key type name when it is a known one with something after it: the lines
    /// `ssh-keygen -F` lists (`hostfile.c:915`-`:945`). `None` for a line `ssh-keygen -F`
    /// skips as invalid.
    pub key_type: Option<String>,
    /// The key when it reads as a key: the lines `ssh` uses and `ssh-keygen -l -F` lists
    /// (`hostfile.c:899`-`:914`). `None` for a line they skip as invalid.
    pub key: Option<LineKey>,
}

/// `hostfile_read_key` and `sshkey_read` (`hostfile.c:157`-`:178`, `sshkey.c:1298`-`:1370`).
fn read_key(raw: &[u8]) -> Option<(Key, String)> {
    let cp = skip_blank(raw);
    let space = cp.iter().position(|c| is_blank(*c))?;
    let imp = impl_by_name(cp.get(..space)?)?;
    let cp = skip_blank(cp.get(space..)?);
    if cp.is_empty() {
        return None;
    }
    let end = word_end(cp);
    let key = parse_blob(&b64_to_buffer(cp.get(..end)?)?)?;
    if key.kind != imp.kind || key.cert != imp.cert {
        return None;
    }
    let comment = skip_blank(cp.get(end..)?);
    Some((key, String::from_utf8_lossy(comment).into_owned()))
}

/// The key type check of `hostkeys_foreach_file` without `HKF_WANT_PARSE_KEY`
/// (`hostfile.c:915`-`:945`).
fn type_name(raw: &[u8]) -> Option<String> {
    let l = raw.iter().position(|c| is_blank(*c))?;
    if l <= 1 || l >= KEY_TYPE_BUFFER {
        return None;
    }
    let ktype = raw.get(..l)?;
    impl_by_name(ktype)?;
    let after = skip_blank(raw.get(l..)?);
    if after.is_empty() || after.first() == Some(&b'#') {
        return None;
    }
    Some(String::from_utf8_lossy(ktype).into_owned())
}

/// One line of `hostkeys_foreach_file` with `HKF_WANT_MATCH` (`hostfile.c:806`-`:958`):
/// `None` when the line is not an entry for `host`.
fn scan(line: &[u8], host: &str) -> Option<Scanned> {
    let cp = skip_blank(line);
    if cp.is_empty() || cp.first() == Some(&b'#') {
        return None;
    }
    let (marker, cp) = check_markers(cp)?;
    let end = cp.iter().position(|c| is_blank(*c))?;
    let hosts = cp.get(..end)?;
    if !host_matches(host, hosts)? {
        return None;
    }
    // "truncated before key type": an invalid line, not an entry.
    let raw = skip_blank(cp.get(end + 1..)?);
    if raw.is_empty() || raw.first() == Some(&b'#') {
        return None;
    }
    Some(Scanned {
        marker,
        key_type: type_name(raw),
        key: read_key(raw),
    })
}

/// A line of [`scan`]: the marker, the key type name `ssh-keygen -F` sees and the key with
/// its comment that `ssh` reads.
struct Scanned {
    marker: Marker,
    key_type: Option<String>,
    key: Option<(Key, String)>,
}

/// The lines of a file as `getline` gives them, each cut at its first NUL (the C string),
/// numbered from 1.
fn lines(text: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    text.split_inclusive(|c| *c == b'\n')
        .map(|l| {
            let end = l
                .iter()
                .position(|c| *c == b'\n' || *c == b'\0')
                .unwrap_or(l.len());
            l.get(..end).unwrap_or_default()
        })
        .zip(1..)
        .map(|(l, n)| (n, l))
}

/// The lines of a file up to its end or the first read error: `None` when it cannot be
/// opened.
fn read_lines(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    Some(read_lines_from(std::io::BufReader::new(file)))
}

/// The lines of `reader` up to its end or the first read error. The bytes read before the
/// error are a last line, as glibc `getline` returns them with the error flag set and fails
/// only on the next call (`getdelim`: it stops at the failed refill and returns what it has).
fn read_lines_from(mut reader: impl std::io::BufRead) -> Vec<u8> {
    let mut text = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line);
        text.extend_from_slice(&line);
        match read {
            Ok(_) if !line.is_empty() => {}
            _ => break,
        }
    }
    text
}

/// The name a host key is looked up by in known_hosts files.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostKeyQuery {
    /// The name, compared as `ssh` gives it (patterns ignore case, hashed names do not).
    pub name: String,
    /// The name [`KnownHosts::check`] looks up again when `name` gives `HOST_NEW`: the host
    /// without its port (`sshconnect.c:1181`-`:1193`). [`HostKeyQuery::for_host`] sets it for
    /// a port other than 0 and 22 without `HostKeyAlias`.
    pub without_port: Option<String>,
}

/// `put_host_port` (`misc.c:801`-`:812`): the host alone for port 0 and 22, else
/// `[host]:port`.
fn put_host_port(host: &str, port: u16) -> String {
    if port == u16::MIN || port == DEFAULT_SSH_PORT {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

impl HostKeyQuery {
    /// The name `ssh` looks a host's key up by (`get_hostfile_hostname_ipaddr`,
    /// `sshconnect.c:648`-`:700`): `HostKeyAlias` when there is one, else the host with the
    /// port. `host` is the final host name ([`crate::Resolution::host`]).
    pub fn for_host(host: &str, port: u16, host_key_alias: Option<&str>) -> HostKeyQuery {
        let ported = port != u16::MIN && port != DEFAULT_SSH_PORT;
        HostKeyQuery {
            name: host_key_alias.map_or_else(|| put_host_port(host, port), str::to_string),
            without_port: (host_key_alias.is_none() && ported).then(|| host.to_string()),
        }
    }

    /// The name of the address check of `CheckHostIP` (`sshconnect.c:674`-`:684`): the
    /// numeric address as `getnameinfo` writes it, with the port.
    pub fn for_address(address: &str, port: u16) -> HostKeyQuery {
        HostKeyQuery {
            name: put_host_port(address, port),
            without_port: None,
        }
    }
}

/// The verdict of [`KnownHosts::check`] (`HostStatus` of `check_key_in_hostkeys`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostKeyVerdict {
    /// `HOST_OK`: a line without a marker has the key.
    Match {
        /// The file of that line.
        file: PathBuf,
        /// The line.
        line: usize,
    },
    /// `HOST_CHANGED`: lines without a marker name the host and none has the key, whatever
    /// their key types. The place is the last of them.
    Mismatch {
        /// The file of that line.
        file: PathBuf,
        /// The line.
        line: usize,
    },
    /// `HOST_REVOKED`: an `@revoked` line for the host has the key. This comes before the
    /// others.
    Revoked {
        /// The file of the first such line.
        file: PathBuf,
        /// The line.
        line: usize,
    },
    /// `HOST_NEW`: no line without a marker names the host.
    NotFound,
    /// `HOST_NEW` where `@cert-authority` lines name the host: OpenSSH would check a host
    /// certificate against them, this crate does not trust them (docs/design/ssh-backends.md
    /// "호스트 키 판정"). The host is unknown.
    DistrustedCaLine {
        /// The file of the first such line.
        file: PathBuf,
        /// The line.
        line: usize,
    },
}

/// The known_hosts files of a connection, read once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownHosts {
    files: Vec<(PathBuf, Vec<u8>)>,
}

impl KnownHosts {
    /// Read the files in order (`UserKnownHostsFile` then `GlobalKnownHostsFile`,
    /// [`crate::Resolution::known_hosts_files`]). A file that cannot be opened is skipped, as
    /// `load_hostkeys` skips a file it cannot open (`hostfile.c:291`-`:294`). A file is read
    /// line by line as `getline` reads it (`hostfile.c:806`): a read error ends the file and
    /// keeps the lines read before it, with the bytes of a line it cut as a last line.
    pub fn load(files: &[PathBuf]) -> KnownHosts {
        KnownHosts {
            files: files
                .iter()
                .filter_map(|p| read_lines(p).map(|text| (p.clone(), text)))
                .collect(),
        }
    }

    /// The files that were opened.
    pub fn files(&self) -> impl Iterator<Item = &Path> {
        self.files.iter().map(|(p, _)| p.as_path())
    }

    fn scanned(&self, query: &HostKeyQuery) -> Vec<(HostKeyEntry, Option<Key>)> {
        let mut out = Vec::new();
        for (path, text) in &self.files {
            for (line, l) in lines(text) {
                if let Some(Scanned {
                    marker,
                    key_type,
                    key,
                }) = scan(l, &query.name)
                {
                    let (key, parsed) = match key {
                        Some((k, comment)) => (
                            Some(LineKey {
                                blob: k.blob.clone(),
                                short_type: k.short,
                                comment,
                            }),
                            Some(k),
                        ),
                        None => (None, None),
                    };
                    out.push((
                        HostKeyEntry {
                            file: path.clone(),
                            line,
                            marker,
                            key_type,
                            key,
                        },
                        parsed,
                    ));
                }
            }
        }
        out
    }

    /// The lines whose host field names `query`, in file and line order: a pattern list
    /// (`match_hostname`) or a hashed name (`|1|`). Lines with a bad marker, no key part or a
    /// bad hash are not entries (`hostfile.c:833`-`:896`).
    pub fn entries(&self, query: &HostKeyQuery) -> Vec<HostKeyEntry> {
        self.scanned(query).into_iter().map(|(e, _)| e).collect()
    }

    /// Judge the key blob a server sent for `query` (`check_hostkeys_by_key_or_type`,
    /// `hostfile.c:349`-`:405`, over the lines `ssh` loads). A blob that does not read as a
    /// key equals no line. A certificate is not checked against `@cert-authority` lines
    /// (they are not trusted): it is checked for revocation by its public key and by its CA
    /// key, as `check_key_not_revoked` does (`hostfile.c:315`-`:332`), and then judged as its
    /// plain key, as `ssh` does when no authority accepts it
    /// (`sshconnect.c:1474`-`:1486`). When the verdict is `HOST_NEW` ([`HostKeyVerdict::NotFound`]
    /// or [`HostKeyVerdict::DistrustedCaLine`]) and the query has
    /// [`HostKeyQuery::without_port`], that name is judged too and its
    /// [`HostKeyVerdict::Match`] is taken; any other verdict of it leaves the first
    /// (`sshconnect.c:1181`-`:1193`). Nothing is written.
    ///
    /// The server key blob must be one the transport has already read as a key and whose
    /// signature it has checked. This function does not check that an ECDSA point is on its
    /// curve or that the CA signature of a certificate is valid (docs/design/ssh-backends.md
    /// "OpenSSH 해석 이식 크레이트").
    ///
    /// This is [`KnownHosts::check_with_address`] without an address: the verdict when
    /// `CheckHostIP` does not apply.
    pub fn check(&self, query: &HostKeyQuery, key_blob: &[u8]) -> HostKeyVerdict {
        self.check_with_address(query, None, key_blob)
    }

    /// [`KnownHosts::check`] when `CheckHostIP` applies: `address` is the numeric address the
    /// host was reached at (as [`HostKeyQuery::for_address`] takes it). `ssh` makes the
    /// look-up without the port by calling `check_host_key` again with port 0
    /// (`sshconnect.c:1181`-`:1193`), so that call also checks the address without its port
    /// (`sshconnect.c:1036`-`:1037`, `:1058`-`:1068`) and fails when the address names the host with
    /// another key (`HOST_CHANGED`, `sshconnect.c:1429`-`:1454`). Here the [`HostKeyVerdict::Match`]
    /// of the look-up without the port is taken only when the address without its port does
    /// not give [`HostKeyVerdict::Mismatch`]; otherwise the first verdict stays. `ssh` passes
    /// that case when `StrictHostKeyChecking` is `no` (it only warns), so this rejects more
    /// there. A certificate is not checked by address, as `ssh` does not load the address
    /// lines for one (`sshconnect.c:1058`). The address check of the name with the port is
    /// the caller's, with [`HostKeyQuery::for_address`].
    pub fn check_with_address(
        &self,
        query: &HostKeyQuery,
        address: Option<&str>,
        key_blob: &[u8],
    ) -> HostKeyVerdict {
        let verdict = self.check_name(&query.name, key_blob);
        if let (HostKeyVerdict::NotFound | HostKeyVerdict::DistrustedCaLine { .. }, Some(name)) =
            (&verdict, &query.without_port)
        {
            let again = self.check_name(name, key_blob);
            let is_cert = parse_blob(key_blob).is_some_and(|k| k.cert);
            let address_differs = !is_cert
                && address.is_some_and(|a| {
                    matches!(
                        self.check_name(&put_host_port(a, u16::MIN), key_blob),
                        HostKeyVerdict::Mismatch { .. }
                    )
                });
            if matches!(again, HostKeyVerdict::Match { .. }) && !address_differs {
                return again;
            }
        }
        verdict
    }

    /// [`KnownHosts::check`] for one name.
    fn check_name(&self, name: &str, key_blob: &[u8]) -> HostKeyVerdict {
        let query = HostKeyQuery {
            name: name.to_string(),
            without_port: None,
        };
        let query = &query;
        let lines: Vec<(HostKeyEntry, Key)> = self
            .scanned(query)
            .into_iter()
            .filter_map(|(e, k)| k.map(|k| (e, k)))
            .collect();
        let key = parse_blob(key_blob);
        // `sshkey_drop_cert`: a certificate without its certificate part.
        let plain = key.as_ref().map(|k| Key {
            cert: false,
            ..k.clone()
        });
        let mut verdict = HostKeyVerdict::NotFound;
        for (e, k) in lines.iter().filter(|(e, _)| e.marker == Marker::None) {
            let place = (e.file.clone(), e.line);
            if plain.as_ref().is_some_and(|key| key.equal(k)) {
                verdict = HostKeyVerdict::Match {
                    file: place.0,
                    line: place.1,
                };
                break;
            }
            verdict = HostKeyVerdict::Mismatch {
                file: place.0,
                line: place.1,
            };
        }
        // `check_key_not_revoked` (`hostfile.c:315`-`:332`): an `@revoked` line with the key,
        // or for a certificate with its CA key (`:325`-`:328`); the first such line.
        if let Some(key) = &key
            && let Some((e, _)) = lines.iter().find(|(e, k)| {
                e.marker == Marker::Revoked
                    && (key.equal_public(k) || key.ca.as_ref().is_some_and(|ca| ca.equal_public(k)))
            })
        {
            return HostKeyVerdict::Revoked {
                file: e.file.clone(),
                line: e.line,
            };
        }
        if verdict == HostKeyVerdict::NotFound
            && let Some((e, _)) = lines
                .iter()
                .find(|(e, _)| e.marker == Marker::CertAuthority)
        {
            return HostKeyVerdict::DistrustedCaLine {
                file: e.file.clone(),
                line: e.line,
            };
        }
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(v: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(v.len()).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(v);
        out
    }

    fn blob(parts: &[&[u8]]) -> Vec<u8> {
        parts.iter().flat_map(|p| string(p)).collect()
    }

    fn ed25519_blob(seed: u8) -> Vec<u8> {
        blob(&[b"ssh-ed25519", &[seed; 32]])
    }

    /// An Ed25519 host certificate of the key `pk` as `cert_parse` reads it: `body` replaces
    /// the fields after the key when given.
    fn cert_with(pk: u8, body: Option<Vec<u8>>) -> Vec<u8> {
        let ca = ed25519_blob(0xca);
        let body = body.unwrap_or_else(|| {
            let mut b = Vec::new();
            b.extend_from_slice(&1u64.to_be_bytes());
            b.extend_from_slice(&CERT_TYPE_HOST.to_be_bytes());
            b.extend(string(b"id"));
            b.extend(string(&blob(&[b"host.example"])));
            b.extend_from_slice(&0u64.to_be_bytes());
            b.extend_from_slice(&u64::MAX.to_be_bytes());
            b.extend(string(b""));
            b.extend(string(&blob(&[b"ext", b""])));
            b.extend(string(b""));
            b.extend(string(&ca));
            b.extend(string(&blob(&[b"ssh-ed25519", &[0; 64]])));
            b
        });
        let mut out = blob(&[b"ssh-ed25519-cert-v01@openssh.com", b"nonce", &[pk; 32]]);
        out.extend(body);
        out
    }

    fn cert(pk: u8) -> Vec<u8> {
        cert_with(pk, None)
    }

    fn line(kind: &str, b: &[u8]) -> String {
        format!("{kind} {}", STANDARD.encode(b))
    }

    fn rsa_blob(bits: usize) -> Vec<u8> {
        let mut n = vec![0u8];
        n.push(0x80);
        n.extend(std::iter::repeat_n(0x11u8, bits / 8 - 1));
        blob(&[b"ssh-rsa", &[0x01, 0x00, 0x01], &n])
    }

    fn ec_blob(curve: &[u8], kind: &[u8], field: usize) -> Vec<u8> {
        let mut point = vec![0x04];
        point.extend(std::iter::repeat_n(0x22u8, 2 * field));
        blob(&[kind, curve, &point])
    }

    fn hosts(text: &str) -> (tempdir::Dir, KnownHosts) {
        let dir = tempdir::Dir::new();
        let path = dir.0.join("known_hosts");
        std::fs::write(&path, text).unwrap();
        let kh = KnownHosts::load(&[path]);
        (dir, kh)
    }

    mod tempdir {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        pub(super) struct Dir(pub(super) std::path::PathBuf);
        impl Dir {
            pub(super) fn new() -> Dir {
                let p = std::env::temp_dir().join(format!(
                    "tasty-net-ssh-config-hostfile-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::SeqCst)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                if let Err(e) = std::fs::remove_dir_all(&self.0) {
                    eprintln!("remove {}: {e}", self.0.display());
                }
            }
        }
    }

    fn q(name: &str) -> HostKeyQuery {
        HostKeyQuery {
            name: name.to_string(),
            without_port: None,
        }
    }

    #[test]
    fn hashed_names_use_hmac_sha1() {
        // HMAC-SHA1 under the salt 0x00..0x13 of "host.example", computed outside this crate
        // (Python hmac).
        let name = b"|1|AAECAwQFBgcICQoLDA0ODxAREhM=|IZ1zV0swCfFUvwvqRtiJbYJ0/LA=";
        assert_eq!(
            host_hash("host.example", name).as_deref(),
            Some(std::str::from_utf8(name).unwrap())
        );
        assert_eq!(host_matches("host.example", name), Some(true));
        assert_eq!(host_matches("Host.example", name), Some(false));
        // A bad salt is a bad hash.
        assert_eq!(host_matches("host.example", b"|1|AAEC|x"), None);
        assert_eq!(host_matches("host.example", b"|2|AAEC|x"), None);
        assert_eq!(host_matches("host.example", b"|1|"), None);
    }

    #[test]
    fn markers_end_at_the_first_space() {
        assert_eq!(
            check_markers(b"@revoked * k").map(|(m, r)| (m, r.to_vec())),
            Some((Marker::Revoked, b"* k".to_vec()))
        );
        assert_eq!(
            check_markers(b"@cert-authority\t\t* k").map(|(m, _)| m),
            None,
            "the first space is after the host field"
        );
        assert_eq!(
            check_markers(b"@cert-authority\t*").map(|(m, _)| m),
            Some(Marker::CertAuthority),
            "with no space the first tab ends it"
        );
        assert_eq!(check_markers(b"@revoked @revoked * k"), None);
        assert_eq!(check_markers(b"@other * k"), None);
        assert_eq!(check_markers(b"@ * k"), None);
    }

    #[test]
    fn keys_read_as_sshkey_read() {
        let ed = ed25519_blob(7);
        assert!(read_key(line("ssh-ed25519", &ed).as_bytes()).is_some());
        assert_eq!(
            read_key(format!("{} the comment ", line("ssh-ed25519", &ed)).as_bytes())
                .map(|(_, c)| c),
            Some("the comment ".to_string())
        );
        // The line type and the blob type must be the same key type.
        assert!(read_key(line("ssh-rsa", &ed).as_bytes()).is_none());
        let rsa = rsa_blob(2048);
        assert!(read_key(line("rsa-sha2-256", &rsa).as_bytes()).is_some());
        assert!(read_key(line("ssh-rsa", &rsa_blob(1016)).as_bytes()).is_none());
        let p256 = ec_blob(b"nistp256", b"ecdsa-sha2-nistp256", 32);
        assert!(read_key(line("ecdsa-sha2-nistp256", &p256).as_bytes()).is_some());
        assert!(read_key(line("ecdsa-sha2-nistp384", &p256).as_bytes()).is_none());
        let wrong_curve = ec_blob(b"nistp384", b"ecdsa-sha2-nistp256", 32);
        assert!(read_key(line("ecdsa-sha2-nistp256", &wrong_curve).as_bytes()).is_none());
        let short_point = ec_blob(b"nistp256", b"ecdsa-sha2-nistp256", 31);
        assert!(read_key(line("ecdsa-sha2-nistp256", &short_point).as_bytes()).is_none());
        // Trailing bytes, a short key, bad base64, no blob.
        let mut long = ed.clone();
        long.push(0);
        assert!(read_key(line("ssh-ed25519", &long).as_bytes()).is_none());
        assert!(
            read_key(line("ssh-ed25519", &blob(&[b"ssh-ed25519", &[1; 31]])).as_bytes()).is_none()
        );
        assert!(read_key(b"ssh-ed25519 AAAA!").is_none());
        assert!(read_key(b"ssh-ed25519").is_none());
        assert!(read_key(b"ssh-ed25519 ").is_none());
        // `b64_pton` skips a carriage return (a CRLF file).
        assert!(read_key(format!("{}\r", line("ssh-ed25519", &ed)).as_bytes()).is_some());
        // A negative mpint.
        let negative = blob(&[b"ssh-rsa", &[0x81], &[0x7f; 256]]);
        assert!(read_key(line("ssh-rsa", &negative).as_bytes()).is_none());
    }

    #[test]
    fn type_names_as_ssh_keygen_f() {
        assert_eq!(type_name(b"ssh-ed25519 x").as_deref(), Some("ssh-ed25519"));
        assert_eq!(type_name(b"ssh-ed25519 #x"), None);
        assert_eq!(type_name(b"ssh-ed25519"), None);
        assert_eq!(type_name(b"ssh-dss AAAA"), None);
        assert_eq!(type_name(b"1024 35 1"), None);
    }

    #[test]
    fn verdicts_follow_check_hostkeys_by_key_or_type() {
        let a = ed25519_blob(1);
        let b = ed25519_blob(2);
        let c = ec_blob(b"nistp256", b"ecdsa-sha2-nistp256", 32);
        let text = format!(
            "host.example {}\nhost.example {}\n@revoked * {}\nother.example {}\n\
             @cert-authority ca.example {}\nboth.example {}\n@cert-authority both.example {}\n",
            line("ssh-ed25519", &a),
            line("ssh-ed25519", &b),
            line("ssh-ed25519", &b),
            line("ssh-ed25519", &a),
            line("ssh-ed25519", &a),
            line("ssh-ed25519", &a),
            line("ssh-ed25519", &a),
        );
        let (dir, kh) = hosts(&text);
        let file = dir.0.join("known_hosts");
        let at = |line| (file.clone(), line);
        // The first line has the key.
        assert_eq!(
            kh.check(&q("host.example"), &a),
            HostKeyVerdict::Match {
                file: at(1).0,
                line: 1
            }
        );
        // The revoked key, though line 2 has it.
        assert_eq!(
            kh.check(&q("host.example"), &b),
            HostKeyVerdict::Revoked {
                file: at(3).0,
                line: 3
            }
        );
        // Another key type is a changed key, not a new host; the place is the last line.
        assert_eq!(
            kh.check(&q("host.example"), &c),
            HostKeyVerdict::Mismatch {
                file: at(2).0,
                line: 2
            }
        );
        assert_eq!(kh.check(&q("new.example"), &c), HostKeyVerdict::NotFound);
        // Revocation also applies to an unknown host.
        assert_eq!(
            kh.check(&q("new.example"), &b),
            HostKeyVerdict::Revoked {
                file: at(3).0,
                line: 3
            }
        );
        // Only an @cert-authority line: an unknown host, reported.
        assert_eq!(
            kh.check(&q("ca.example"), &a),
            HostKeyVerdict::DistrustedCaLine {
                file: at(5).0,
                line: 5
            }
        );
        assert_eq!(
            kh.check(&q("both.example"), &a),
            HostKeyVerdict::Match {
                file: at(6).0,
                line: 6
            }
        );
        // A blob that is not a key equals nothing.
        assert_eq!(
            kh.check(&q("other.example"), b"junk"),
            HostKeyVerdict::Mismatch {
                file: at(4).0,
                line: 4
            }
        );
    }

    #[test]
    fn a_certificate_is_judged_as_its_plain_key() {
        let a = ed25519_blob(5);
        let text = format!(
            "@cert-authority ca.example {k}\nhost.example {k}\nca.example {}\n",
            line("ssh-ed25519-cert-v01@openssh.com", &cert(5)),
            k = line("ssh-ed25519", &a)
        );
        let (dir, kh) = hosts(&text);
        let file = dir.0.join("known_hosts");
        assert_eq!(
            kh.check(&q("host.example"), &cert(5)),
            HostKeyVerdict::Match {
                file: file.clone(),
                line: 2
            }
        );
        assert_eq!(
            kh.check(&q("host.example"), &cert(6)),
            HostKeyVerdict::Mismatch {
                file: file.clone(),
                line: 2
            }
        );
        // A certificate line without a marker is not the plain key (`sshkey_equal` compares
        // the types).
        assert_eq!(
            kh.check(&q("ca.example"), &cert(5)),
            HostKeyVerdict::Mismatch { file, line: 3 }
        );
    }

    #[test]
    fn a_revoked_certificate_line_revokes_its_key() {
        let a = ed25519_blob(3);
        let text = format!(
            "@revoked * {}\n",
            line("ssh-ed25519-cert-v01@openssh.com", &cert(3))
        );
        let (dir, kh) = hosts(&text);
        assert_eq!(
            kh.check(&q("x"), &a),
            HostKeyVerdict::Revoked {
                file: dir.0.join("known_hosts"),
                line: 1
            }
        );
    }

    #[test]
    fn a_revoked_ca_key_revokes_its_certificates() {
        let key = ed25519_blob(4);
        let ca = ed25519_blob(0xca);
        let other = ed25519_blob(0xcb);
        let known = format!("host.example {}\n", line("ssh-ed25519", &key));
        // `cert(4)` is signed by the CA `0xca`.
        let (dir, kh) = hosts(&format!("{known}@revoked * {}\n", line("ssh-ed25519", &ca)));
        assert_eq!(
            kh.check(&q("host.example"), &cert(4)),
            HostKeyVerdict::Revoked {
                file: dir.0.join("known_hosts"),
                line: 2
            }
        );
        // The plain key is not revoked by the CA key.
        assert_eq!(
            kh.check(&q("host.example"), &key),
            HostKeyVerdict::Match {
                file: dir.0.join("known_hosts"),
                line: 1
            }
        );
        // Another CA key revoked: the verdict of the plain key.
        let (dir, kh) = hosts(&format!(
            "{known}@revoked * {}\n",
            line("ssh-ed25519", &other)
        ));
        assert_eq!(
            kh.check(&q("host.example"), &cert(4)),
            HostKeyVerdict::Match {
                file: dir.0.join("known_hosts"),
                line: 1
            }
        );
        // The first `@revoked` line with the key or the CA key is the place.
        let (dir, kh) = hosts(&format!(
            "{known}@revoked * {}\n@revoked * {}\n",
            line("ssh-ed25519", &ca),
            line("ssh-ed25519", &key)
        ));
        assert_eq!(
            kh.check(&q("host.example"), &cert(4)),
            HostKeyVerdict::Revoked {
                file: dir.0.join("known_hosts"),
                line: 2
            }
        );
    }

    #[test]
    fn the_retry_without_the_port_checks_the_address_without_the_port() {
        let a = ed25519_blob(21);
        let b = ed25519_blob(22);
        let (ka, kb) = (line("ssh-ed25519", &a), line("ssh-ed25519", &b));
        let text = format!(
            "web.example {ka}\n10.0.0.7 {kb}\n[10.0.0.7]:2222 {ka}\n10.0.0.8 {ka}\n\
             @revoked 10.0.0.9 {ka}\n"
        );
        let (dir, kh) = hosts(&text);
        let file = dir.0.join("known_hosts");
        let web = HostKeyQuery::for_host("web.example", 2222, None);
        let matched = HostKeyVerdict::Match {
            file: file.clone(),
            line: 1,
        };
        // The address without the port has another key: the retry fails, the host stays new.
        assert_eq!(
            kh.check_with_address(&web, Some("10.0.0.7"), &a),
            HostKeyVerdict::NotFound
        );
        // Without the address (no `CheckHostIP`) the retry takes the line.
        assert_eq!(kh.check(&web, &a), matched);
        assert_eq!(kh.check_with_address(&web, None, &a), matched);
        // The address without the port has the key, has no line, or revokes it: the retry
        // takes the line (`ssh` fails only on `HOST_CHANGED`).
        assert_eq!(kh.check_with_address(&web, Some("10.0.0.8"), &a), matched);
        assert_eq!(kh.check_with_address(&web, Some("10.0.0.10"), &a), matched);
        assert_eq!(kh.check_with_address(&web, Some("10.0.0.9"), &a), matched);
        // A certificate is not checked by address.
        let (dir, kh) = hosts(&format!(
            "web.example {ka}\n10.0.0.7 {kb}\n",
            ka = line("ssh-ed25519", &ed25519_blob(4))
        ));
        assert_eq!(
            kh.check_with_address(&web, Some("10.0.0.7"), &cert(4)),
            HostKeyVerdict::Match {
                file: dir.0.join("known_hosts"),
                line: 1
            }
        );
        // No retry for port 22: the address is the caller's check.
        let plain = HostKeyQuery::for_host("web.example", 22, None);
        assert_eq!(
            kh.check_with_address(&plain, Some("10.0.0.7"), &ed25519_blob(4)),
            HostKeyVerdict::Match {
                file: dir.0.join("known_hosts"),
                line: 1
            }
        );
    }

    #[test]
    fn lines_end_at_a_newline_or_a_nul() {
        let text = b"a\nb\0c\n\nlast";
        let got: Vec<(usize, Vec<u8>)> = lines(text).map(|(n, l)| (n, l.to_vec())).collect();
        assert_eq!(
            got,
            vec![
                (1, b"a".to_vec()),
                (2, b"b".to_vec()),
                (3, Vec::new()),
                (4, b"last".to_vec())
            ]
        );
    }

    #[test]
    fn entries_name_the_lines_of_a_host() {
        let a = ed25519_blob(4);
        let text = format!(
            "# comment\n  *.example,!db.example {k}\n[web.example]:2222 {k}\nweb.example\n\
             web.example #x\nWeb.Example,10.0.0.7 {k}\n@bad web.example {k}\n",
            k = line("ssh-ed25519", &a)
        );
        let (_dir, kh) = hosts(&text);
        let lines_of = |n: &str| -> Vec<(usize, bool)> {
            kh.entries(&q(n))
                .iter()
                .map(|e| (e.line, e.key.is_some()))
                .collect()
        };
        assert_eq!(
            lines_of("web.example"),
            vec![(2, true), (6, true)],
            "no key type (lines 4 and 5) and a bad marker (line 7) are not entries"
        );
        assert_eq!(lines_of("db.example"), Vec::<(usize, bool)>::new());
        assert_eq!(lines_of("[web.example]:2222"), vec![(3, true)]);
        assert_eq!(lines_of("10.0.0.7"), vec![(6, true)]);
    }

    #[test]
    fn queries_add_the_port_as_put_host_port() {
        assert_eq!(HostKeyQuery::for_host("h", 22, None).name, "h");
        assert_eq!(HostKeyQuery::for_host("h", 0, None).name, "h");
        assert_eq!(HostKeyQuery::for_host("h", 2222, None).name, "[h]:2222");
        assert_eq!(
            HostKeyQuery::for_host("h", 2222, Some("alias")).name,
            "alias"
        );
        assert_eq!(
            HostKeyQuery::for_address("2001:db8::1", 2222).name,
            "[2001:db8::1]:2222"
        );
    }

    #[test]
    fn missing_files_are_skipped() {
        let dir = tempdir::Dir::new();
        let kh = KnownHosts::load(&[dir.0.join("none")]);
        assert_eq!(kh.files().count(), 0);
        let path = dir.0.join("known_hosts");
        std::fs::write(&path, "").unwrap();
        let kh = KnownHosts::load(&[dir.0.join("none"), path]);
        assert_eq!(kh.files().count(), 1, "the file that exists is opened");
    }

    #[test]
    fn a_certificate_reads_as_cert_parse() {
        let key = |b: &[u8]| parse_blob(b).map(|k| k.cert);
        assert_eq!(key(&cert(1)), Some(true));
        let full = cert(1);
        let header = blob(&[b"ssh-ed25519-cert-v01@openssh.com", b"nonce", &[1; 32]]).len();
        for cut in [header, header + 4, full.len() - 1] {
            assert_eq!(key(&full[..cut]), None, "cut at {cut} of {}", full.len());
        }
        let mut trailing = full.clone();
        trailing.push(0);
        assert_eq!(key(&trailing), None, "bytes after the signature");

        // The body with one field changed: `(serial, type, principals, ca, signature)`.
        let body = |kind: u32, principals: Vec<u8>, ca: Vec<u8>, signature: Vec<u8>| {
            let mut b = Vec::new();
            b.extend_from_slice(&1u64.to_be_bytes());
            b.extend_from_slice(&kind.to_be_bytes());
            b.extend(string(b"id"));
            b.extend(string(&principals));
            b.extend_from_slice(&0u64.to_be_bytes());
            b.extend_from_slice(&u64::MAX.to_be_bytes());
            b.extend(string(b""));
            b.extend(string(b""));
            b.extend(string(b""));
            b.extend(string(&ca));
            b.extend(string(&signature));
            Some(b)
        };
        let ca = ed25519_blob(0xca);
        let sig = blob(&[b"ssh-ed25519", &[0; 64]]);
        let names = |n: usize| -> Vec<u8> { (0..n).flat_map(|_| string(b"h")).collect() };
        assert_eq!(
            key(&cert_with(
                1,
                body(CERT_TYPE_USER, names(1), ca.clone(), sig.clone())
            )),
            Some(true)
        );
        assert_eq!(
            key(&cert_with(1, body(3, names(1), ca.clone(), sig.clone()))),
            None,
            "type 3"
        );
        assert_eq!(
            key(&cert_with(
                1,
                body(
                    CERT_TYPE_HOST,
                    names(CERT_MAX_PRINCIPALS),
                    ca.clone(),
                    sig.clone()
                )
            )),
            Some(true)
        );
        assert_eq!(
            key(&cert_with(
                1,
                body(
                    CERT_TYPE_HOST,
                    names(CERT_MAX_PRINCIPALS + 1),
                    ca.clone(),
                    sig.clone()
                )
            )),
            None,
            "too many principals"
        );
        assert_eq!(
            key(&cert_with(
                1,
                body(CERT_TYPE_HOST, b"\0\0".to_vec(), ca.clone(), sig.clone())
            )),
            None,
            "a cut principal"
        );
        assert_eq!(
            key(&cert_with(
                1,
                body(CERT_TYPE_HOST, names(1), cert(2), sig.clone())
            )),
            None,
            "a certificate as the CA"
        );
        assert_eq!(
            key(&cert_with(
                1,
                body(CERT_TYPE_HOST, names(1), b"junk".to_vec(), sig.clone())
            )),
            None,
            "a CA that is not a key"
        );
        assert_eq!(
            key(&cert_with(
                1,
                body(CERT_TYPE_HOST, names(1), ca.clone(), Vec::new())
            )),
            None,
            "a signature without its type"
        );
        // A line with a cut certificate is not a key (`sshkey_read` fails).
        let text = format!(
            "cut.example {}\n",
            line("ssh-ed25519-cert-v01@openssh.com", &full[..header])
        );
        let (_dir, kh) = hosts(&text);
        let entries = kh.entries(&q("cut.example"));
        assert_eq!(entries.len(), 1);
        assert!(entries[0].key.is_none());
        assert_eq!(
            kh.check(&q("cut.example"), &full[..header]),
            HostKeyVerdict::NotFound
        );
    }

    #[test]
    fn decoded_blobs_fit_an_sshbuf() {
        assert!(fits_buffer(SSHBUF_SIZE_MAX));
        assert!(!fits_buffer(SSHBUF_SIZE_MAX + 1));
    }

    #[test]
    fn a_read_error_keeps_the_lines_before_it() {
        /// Gives the bytes, then fails every read.
        struct Failing(Vec<u8>);
        impl std::io::Read for Failing {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() {
                    return Err(std::io::Error::other("read error"));
                }
                let n = buf.len().min(self.0.len());
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0.drain(..n);
                Ok(n)
            }
        }
        let read = |text: &[u8]| read_lines_from(std::io::BufReader::new(Failing(text.to_vec())));
        assert_eq!(
            read(b"one\ntwo\nthr"),
            b"one\ntwo\nthr",
            "the piece before the error is a last line"
        );
        assert_eq!(read(b"one\n"), b"one\n");
        assert_eq!(read(b""), b"");
        let got = read_lines_from(&b"one\nlast"[..]);
        assert_eq!(got, b"one\nlast", "the last line without a newline is kept");
        // A last `@revoked` piece before the error revokes, as in `ssh`.
        let a = ed25519_blob(31);
        let k = line("ssh-ed25519", &a);
        let path = PathBuf::from("known_hosts");
        let kh = KnownHosts {
            files: vec![(
                path.clone(),
                read(format!("h.example {k}\n@revoked h.example {k}").as_bytes()),
            )],
        };
        assert_eq!(
            kh.check(&q("h.example"), &a),
            HostKeyVerdict::Revoked {
                file: path,
                line: 2
            }
        );
    }

    #[test]
    fn a_port_is_dropped_for_a_new_host() {
        let a = ed25519_blob(7);
        let b = ed25519_blob(8);
        let text = format!(
            "port.example {k}\n[changed.example]:2222 {o}\nchanged.example {k}\n\
             @revoked [rev.example]:2222 {k}\nrev.example {k}\nother.example {o}\n",
            k = line("ssh-ed25519", &a),
            o = line("ssh-ed25519", &b)
        );
        let (dir, kh) = hosts(&text);
        let file = dir.0.join("known_hosts");
        let at = |host: &str, port: u16, alias: Option<&str>, key: &[u8]| {
            kh.check(&HostKeyQuery::for_host(host, port, alias), key)
        };
        assert_eq!(
            at("port.example", 2222, None, &a),
            HostKeyVerdict::Match {
                file: file.clone(),
                line: 1
            }
        );
        assert_eq!(
            at("port.example", 2222, Some("port.example:alias"), &a),
            HostKeyVerdict::NotFound,
            "not with HostKeyAlias"
        );
        assert_eq!(
            at("port.example", 2222, None, &b),
            HostKeyVerdict::NotFound,
            "a Mismatch of the retry leaves HOST_NEW"
        );
        assert_eq!(
            at("changed.example", 2222, None, &a),
            HostKeyVerdict::Mismatch {
                file: file.clone(),
                line: 2
            },
            "no retry after HOST_CHANGED"
        );
        assert_eq!(
            at("rev.example", 2222, None, &a),
            HostKeyVerdict::Revoked {
                file: file.clone(),
                line: 4
            },
            "no retry after HOST_REVOKED"
        );
        assert_eq!(HostKeyQuery::for_host("h", 22, None).without_port, None);
        assert_eq!(HostKeyQuery::for_host("h", 0, None).without_port, None);
        assert_eq!(
            HostKeyQuery::for_host("h", 2222, None)
                .without_port
                .as_deref(),
            Some("h")
        );
        assert_eq!(
            HostKeyQuery::for_address("10.0.0.1", 2222).without_port,
            None
        );
    }

    /// Two files in order: `(entries, verdict)` for the host `h.example`.
    fn two_files(
        first: &str,
        second: &str,
        key: &[u8],
    ) -> (Vec<(PathBuf, usize)>, HostKeyVerdict, [PathBuf; 2]) {
        let dir = tempdir::Dir::new();
        let paths = [dir.0.join("first"), dir.0.join("second")];
        std::fs::write(&paths[0], first).unwrap();
        std::fs::write(&paths[1], second).unwrap();
        let kh = KnownHosts::load(&paths);
        let entries = kh
            .entries(&q("h.example"))
            .into_iter()
            .map(|e| (e.file, e.line))
            .collect();
        let verdict = kh.check(&q("h.example"), key);
        (entries, verdict, paths)
    }

    #[test]
    fn files_are_read_in_order() {
        let a = ed25519_blob(9);
        let b = ed25519_blob(10);
        let (ka, kb) = (line("ssh-ed25519", &a), line("ssh-ed25519", &b));
        // Another key in the first file, the key in the second: the key is found.
        let (entries, verdict, [first, second]) = two_files(
            &format!("# one\nh.example {kb}\n"),
            &format!("h.example {ka}\n"),
            &a,
        );
        assert_eq!(entries, vec![(first.clone(), 2), (second.clone(), 1)]);
        assert_eq!(
            verdict,
            HostKeyVerdict::Match {
                file: second,
                line: 1
            }
        );
        // The key in the first file, revoked in the second: revoked.
        let (entries, verdict, [first, second]) = two_files(
            &format!("h.example {ka}\n"),
            &format!("\n@revoked h.example {ka}\n"),
            &a,
        );
        assert_eq!(entries, vec![(first, 1), (second.clone(), 2)]);
        assert_eq!(
            verdict,
            HostKeyVerdict::Revoked {
                file: second,
                line: 2
            }
        );
    }

    #[test]
    fn host_fields_match_as_bytes() {
        let a = ed25519_blob(11);
        let mut text = b"bad\xffname.example ".to_vec();
        text.extend_from_slice(line("ssh-ed25519", &a).as_bytes());
        text.push(b'\n');
        text.extend_from_slice(format!("ok.example {}\n", line("ssh-ed25519", &a)).as_bytes());
        let dir = tempdir::Dir::new();
        let path = dir.0.join("known_hosts");
        std::fs::write(&path, &text).unwrap();
        let kh = KnownHosts::load(&[path]);
        // `from_utf8_lossy` would make the byte U+FFFD.
        assert_eq!(kh.entries(&q("bad\u{fffd}name.example")), Vec::new());
        assert_eq!(
            kh.check(&q("bad\u{fffd}name.example"), &a),
            HostKeyVerdict::NotFound
        );
        assert_eq!(
            kh.entries(&q("ok.example")).len(),
            1,
            "the line after it is read"
        );
    }
}
