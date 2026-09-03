//! The small DER writer the PKCS#10 builder needs.
//!
//! Why hand-rolled rather than a crate: the only ASN.1 this client ever produces is a
//! certificate signing request and the two integers of an ECDSA signature. That is
//! four structural rules — tag, definite length, INTEGER, and OID — and it is entirely
//! testable against byte vectors, which is the form a reviewer can actually check.
//! Pulling in a general-purpose ASN.1 stack for it would add a dependency that must
//! cross-compile to `x86_64-pc-windows-gnu` and be audited, in exchange for code that
//! is longer to read than what is below.
//!
//! **This module writes DER only; it never parses.** Nothing here is exposed to
//! untrusted input: every byte handled came from our own key or our own configuration.
//! Length fields are therefore written, not trusted, and there is no path where a
//! hostile length can cause an allocation. If a parser is ever needed, it does not
//! belong in this file.

/// Universal-class tags, plus the one context tag PKCS#10 needs.
pub mod tag {
    pub const INTEGER: u8 = 0x02;
    pub const BIT_STRING: u8 = 0x03;
    pub const OCTET_STRING: u8 = 0x04;
    pub const NULL: u8 = 0x05;
    pub const OID: u8 = 0x06;
    pub const UTF8_STRING: u8 = 0x0C;
    pub const SEQUENCE: u8 = 0x30;
    pub const SET: u8 = 0x31;
    /// `[0]` constructed — the `attributes` field of a CertificationRequestInfo.
    pub const CONTEXT_0_CONSTRUCTED: u8 = 0xA0;
}

/// Encode one tag-length-value.
///
/// DER requires the *definite, minimal* length encoding: short form below 128, and
/// otherwise the fewest possible base-256 bytes. BER's indefinite form and
/// non-minimal long forms are both legal ASN.1 and both rejected by a strict parser —
/// Go's `encoding/asn1`, which is what will read this, is strict.
pub fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(tag);
    write_len(&mut out, body.len());
    out.extend_from_slice(body);
    out
}

fn write_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
        return;
    }
    let bytes = len.to_be_bytes();
    // Skip leading zeroes: the length must be the shortest form that fits.
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len() - 1);
    let significant = &bytes[first..];
    out.push(0x80 | significant.len() as u8);
    out.extend_from_slice(significant);
}

/// `SEQUENCE { .. }` over already-encoded members.
pub fn sequence(members: &[Vec<u8>]) -> Vec<u8> {
    tlv(tag::SEQUENCE, &members.concat())
}

/// `SET { .. }` over already-encoded members.
///
/// DER requires a SET's members to be sorted by their encoding. Every SET this module
/// writes has exactly one member, so there is nothing to sort; a caller passing more
/// than one must sort them, and [`set`] says so rather than silently emitting
/// something a strict parser rejects.
pub fn set(members: &[Vec<u8>]) -> Vec<u8> {
    debug_assert!(
        members.len() <= 1,
        "a DER SET with several members must be sorted by encoding before it is written"
    );
    tlv(tag::SET, &members.concat())
}

/// `UTF8String`.
///
/// UTF8String rather than PrintableString for every name we write. The subject carries
/// the machine GUID, which Windows formats with braces — `{4c4c4544-...}` — and braces
/// are not in the PrintableString character set. Emitting them under a PrintableString
/// tag produces a CSR that some parsers accept and others reject, which is the worst
/// of the available outcomes.
pub fn utf8_string(s: &str) -> Vec<u8> {
    tlv(tag::UTF8_STRING, s.as_bytes())
}

/// `BIT STRING` with no unused trailing bits, which is every bit string here.
pub fn bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 1);
    body.push(0x00); // unused-bit count
    body.extend_from_slice(bytes);
    tlv(tag::BIT_STRING, &body)
}

/// `INTEGER` from an unsigned big-endian magnitude.
///
/// DER INTEGERs are signed two's complement, so a magnitude whose top bit is set needs
/// a leading zero byte or it decodes as a negative number. This is the classic ECDSA
/// signature bug: roughly half of all `r` and `s` values have the high bit set, so
/// omitting the pad produces a signature that verifies on about a quarter of attempts
/// and fails on the rest — which looks like a flaky network rather than an encoding
/// error.
pub fn unsigned_integer(magnitude: &[u8]) -> Vec<u8> {
    let start = magnitude.iter().position(|&b| b != 0).unwrap_or(magnitude.len());
    let trimmed = &magnitude[start..];
    if trimmed.is_empty() {
        return tlv(tag::INTEGER, &[0x00]);
    }
    let mut body = Vec::with_capacity(trimmed.len() + 1);
    if trimmed[0] & 0x80 != 0 {
        body.push(0x00);
    }
    body.extend_from_slice(trimmed);
    tlv(tag::INTEGER, &body)
}

/// `INTEGER` for a small non-negative value (the PKCS#10 version, which is 0).
pub fn small_integer(v: u8) -> Vec<u8> {
    tlv(tag::INTEGER, &[v])
}

/// `OBJECT IDENTIFIER` from its dotted arcs.
///
/// The first two arcs are packed into one byte as `40*a + b`, and every subsequent arc
/// is base-128 with the continuation bit set on all but the last byte.
pub fn oid(arcs: &[u32]) -> Vec<u8> {
    assert!(arcs.len() >= 2, "an OID has at least two arcs");
    assert!(arcs[0] <= 2 && arcs[1] < 40, "unsupported OID root {:?}", &arcs[..2]);
    let mut body = vec![(arcs[0] * 40 + arcs[1]) as u8];
    for &arc in &arcs[2..] {
        base128(&mut body, arc);
    }
    tlv(tag::OID, &body)
}

fn base128(out: &mut Vec<u8>, mut v: u32) {
    let mut digits = [0u8; 5];
    let mut n = 0;
    loop {
        digits[n] = (v & 0x7F) as u8;
        n += 1;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    for i in (1..n).rev() {
        out.push(digits[i] | 0x80);
    }
    out.push(digits[0]);
}

/// `NULL`, for an AlgorithmIdentifier that carries absent parameters explicitly.
pub fn null() -> Vec<u8> {
    tlv(tag::NULL, &[])
}

/// `OCTET STRING`.
pub fn octet_string(bytes: &[u8]) -> Vec<u8> {
    tlv(tag::OCTET_STRING, bytes)
}

/// Wrap DER in a PEM block.
///
/// 64-character lines and `\n` endings. Not `\r\n`: the body is base64 and the
/// gateway's `pem.Decode` accepts either, but a CSR that travels inside a JSON string
/// with CRLF in it is one more thing to get wrong in a log or a support ticket.
pub fn to_pem(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = String::with_capacity(b64.len() + 2 * label.len() + 64);
    out.push_str("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_and_long_lengths_use_the_minimal_definite_form() {
        assert_eq!(tlv(0x04, &[]), vec![0x04, 0x00]);
        assert_eq!(tlv(0x04, &[0xAA]), vec![0x04, 0x01, 0xAA]);
        // 127 is the last short-form length.
        assert_eq!(tlv(0x04, &[0u8; 127])[..2], [0x04, 0x7F]);
        // 128 needs the long form, one length byte.
        assert_eq!(tlv(0x04, &[0u8; 128])[..3], [0x04, 0x81, 0x80]);
        assert_eq!(tlv(0x04, &[0u8; 255])[..3], [0x04, 0x81, 0xFF]);
        // 256 needs two, and must not be padded to more.
        assert_eq!(tlv(0x04, &[0u8; 256])[..4], [0x04, 0x82, 0x01, 0x00]);
        assert_eq!(tlv(0x04, &[0u8; 1000])[..4], [0x04, 0x82, 0x03, 0xE8]);
    }

    #[test]
    fn the_known_oids_encode_to_their_published_bytes() {
        // 1.2.840.10045.2.1 — id-ecPublicKey (RFC 5480).
        assert_eq!(
            oid(&[1, 2, 840, 10045, 2, 1]),
            vec![0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]
        );
        // 1.2.840.10045.3.1.7 — prime256v1 / secp256r1 / P-256.
        assert_eq!(
            oid(&[1, 2, 840, 10045, 3, 1, 7]),
            vec![0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07]
        );
        // 1.2.840.10045.4.3.2 — ecdsa-with-SHA256.
        assert_eq!(
            oid(&[1, 2, 840, 10045, 4, 3, 2]),
            vec![0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02]
        );
        // 2.5.4.3 — id-at-commonName. Root arc 2 exercises the 40*a+b packing.
        assert_eq!(oid(&[2, 5, 4, 3]), vec![0x06, 0x03, 0x55, 0x04, 0x03]);
        // 1.2.840.113549.1.9.14 — a five-byte arc, exercising base-128 continuation.
        assert_eq!(
            oid(&[1, 2, 840, 113549, 1, 9, 14]),
            vec![0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x0E]
        );
    }

    #[test]
    fn a_magnitude_with_its_top_bit_set_gets_the_leading_zero() {
        // Half of all ECDSA `r` and `s` values look like this. Omitting the pad makes
        // a signature that verifies about a quarter of the time.
        assert_eq!(unsigned_integer(&[0x7F]), vec![0x02, 0x01, 0x7F]);
        assert_eq!(unsigned_integer(&[0x80]), vec![0x02, 0x02, 0x00, 0x80]);
        assert_eq!(unsigned_integer(&[0xFF, 0x01]), vec![0x02, 0x03, 0x00, 0xFF, 0x01]);
    }

    #[test]
    fn leading_zeroes_are_stripped_and_zero_stays_one_byte() {
        assert_eq!(unsigned_integer(&[0x00, 0x00, 0x2A]), vec![0x02, 0x01, 0x2A]);
        assert_eq!(unsigned_integer(&[0x00, 0x00, 0x00]), vec![0x02, 0x01, 0x00]);
        assert_eq!(unsigned_integer(&[]), vec![0x02, 0x01, 0x00]);
        // A stripped magnitude whose new top byte has the high bit set still pads.
        assert_eq!(unsigned_integer(&[0x00, 0x80]), vec![0x02, 0x02, 0x00, 0x80]);
    }

    #[test]
    fn bit_strings_declare_zero_unused_bits() {
        assert_eq!(bit_string(&[0xDE, 0xAD]), vec![0x03, 0x03, 0x00, 0xDE, 0xAD]);
    }

    #[test]
    fn nesting_composes_without_recomputing_lengths_by_hand() {
        let inner = sequence(&[small_integer(0), utf8_string("hi")]);
        assert_eq!(inner, vec![0x30, 0x07, 0x02, 0x01, 0x00, 0x0C, 0x02, b'h', b'i']);
        let outer = sequence(&[inner.clone()]);
        assert_eq!(outer[0], tag::SEQUENCE);
        assert_eq!(outer[1] as usize, inner.len());
    }

    #[test]
    fn utf8_strings_carry_the_characters_printablestring_cannot() {
        // The subject holds a Windows machine GUID, braces included.
        let s = utf8_string("{4c4c4544-0037-4a10-8043-b2c04f483233}");
        assert_eq!(s[0], tag::UTF8_STRING);
        assert_eq!(s[1] as usize, 38);
    }

    #[test]
    fn pem_wraps_at_sixtyfour_characters_with_the_requested_label() {
        let pem = to_pem("CERTIFICATE REQUEST", &[0xABu8; 100]);
        let lines: Vec<&str> = pem.lines().collect();
        assert_eq!(lines[0], "-----BEGIN CERTIFICATE REQUEST-----");
        assert_eq!(*lines.last().unwrap(), "-----END CERTIFICATE REQUEST-----");
        for line in &lines[1..lines.len() - 1] {
            assert!(line.len() <= 64, "body line is {} chars", line.len());
        }
        assert!(pem.ends_with('\n'), "a PEM block ends with a newline");
        assert!(!pem.contains('\r'), "LF endings only");
        // Round-trips through a decoder.
        use base64::Engine as _;
        let body: String = lines[1..lines.len() - 1].concat();
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(body).unwrap(),
            vec![0xABu8; 100]
        );
    }
}
