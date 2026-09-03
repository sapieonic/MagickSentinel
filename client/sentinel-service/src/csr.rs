//! The PKCS#10 certificate signing request the enrollment exchange sends.
//!
//! One shape, built once, against a [`DeviceKey`](crate::devicekey::DeviceKey) that
//! signs it without ever handing over the private key. RFC 2986:
//!
//! ```text
//! CertificationRequest ::= SEQUENCE {
//!   certificationRequestInfo CertificationRequestInfo,
//!   signatureAlgorithm       AlgorithmIdentifier,
//!   signature                BIT STRING }
//!
//! CertificationRequestInfo ::= SEQUENCE {
//!   version       INTEGER { v1(0) },
//!   subject       Name,
//!   subjectPKInfo SubjectPublicKeyInfo,
//!   attributes    [0] IMPLICIT SET OF Attribute }
//! ```
//!
//! The reader on the other side is `parseAndVerifyCSR` in
//! `server/gateway/internal/api/enroll.go`, which is strict in three ways worth
//! naming, because each one is a way to produce a CSR that looks fine locally and is
//! rejected on enrollment:
//!
//! 1. The PEM block must be labelled `CERTIFICATE REQUEST`.
//! 2. `csr.CheckSignature()` must pass — that is what proves we hold the private key
//!    for the public key we are asking it to certify, and it is why the signature is
//!    made over the exact DER bytes of `certificationRequestInfo` rather than over a
//!    re-encoding of them.
//! 3. `csr.PublicKeyAlgorithm` must be `x509.ECDSA`, which means the SPKI has to carry
//!    `id-ecPublicKey` with the named-curve parameter, not explicit curve parameters.
//!
//! What the request deliberately does **not** carry: no requested extensions, no SAN,
//! no key usage. The CA template in `enroll.go` sets `KeyUsageDigitalSignature` and
//! `ExtKeyUsageClientAuth` itself and puts the tenant in the subject's OU and the
//! device id in the CN. A client that asked for its own extensions would either be
//! ignored — the honest outcome — or, with a less careful CA, be able to influence what
//! it is issued. Asking for nothing is the correct posture for an enrollment CSR.
//!
//! The subject here is therefore cosmetic and is never trusted: the gateway overwrites
//! it. It carries the machine GUID so that a CSR captured in a support bundle can be
//! matched to a machine.

use crate::der;
use crate::devicekey::{spki_p256, DeviceKey, DeviceKeyError};

/// `1.2.840.10045.4.3.2` — ecdsa-with-SHA256 (RFC 5758).
const ECDSA_WITH_SHA256: [u32; 7] = [1, 2, 840, 10045, 4, 3, 2];
/// `2.5.4.3` — id-at-commonName.
const COMMON_NAME: [u32; 4] = [2, 5, 4, 3];
/// `2.5.4.10` — id-at-organizationName.
const ORGANIZATION: [u32; 4] = [2, 5, 4, 10];

/// Organization every Sentinel device CSR carries. A constant rather than config: it
/// is our name, not the tenant's, and the tenant travels in the issued certificate's
/// OU where the CA puts it.
pub const ORGANIZATION_NAME: &str = "MagickVoice Sentinel";

/// Build a PEM-encoded PKCS#10 request for `key`, with `common_name` as the subject CN.
///
/// `common_name` is the machine GUID at enrollment. It is not validated beyond being
/// encodable as UTF8String, because nothing downstream trusts it.
pub fn build_csr_pem(key: &dyn DeviceKey, common_name: &str) -> Result<String, DeviceKeyError> {
    Ok(der::to_pem("CERTIFICATE REQUEST", &build_csr_der(key, common_name)?))
}

/// The DER form, for tests and for anything that needs the bytes.
pub fn build_csr_der(key: &dyn DeviceKey, common_name: &str) -> Result<Vec<u8>, DeviceKeyError> {
    let point = key.public_point()?;
    let info = certification_request_info(&spki_p256(&point), common_name);

    // Signed over the encoded `certificationRequestInfo` exactly as it will appear in
    // the request. Re-encoding it after signing — even into something equally valid —
    // breaks `CheckSignature`, and the resulting `bad_csr` says nothing about why.
    let signature = key.sign(&info)?;

    Ok(der::sequence(&[
        info,
        // No parameters field at all. RFC 5758 section 3.2: for
        // ecdsa-with-SHA256 the parameters MUST be absent. Emitting an explicit NULL
        // there is the classic mistake — copied from the RSA AlgorithmIdentifier,
        // where NULL is required — and Go rejects it.
        der::sequence(&[der::oid(&ECDSA_WITH_SHA256)]),
        der::bit_string(&signature),
    ]))
}

/// `CertificationRequestInfo`, DER-encoded.
fn certification_request_info(spki: &[u8], common_name: &str) -> Vec<u8> {
    der::sequence(&[
        // v1 is 0. There is no other version.
        der::small_integer(0),
        name_rdn_sequence(common_name),
        spki.to_vec(),
        // `attributes [0] IMPLICIT SET OF Attribute`, empty.
        //
        // Present-but-empty rather than omitted: the field is not OPTIONAL in RFC
        // 2986, and Go's `encoding/asn1` unmarshals the request into a struct with a
        // `tag:0` field. An omitted `[0]` decodes as an empty slice too, but an empty
        // constructed `A0 00` is what the RFC actually specifies, and every other
        // PKCS#10 producer emits it.
        der::tlv(der::tag::CONTEXT_0_CONSTRUCTED, &[]),
    ])
}

/// `Name ::= RDNSequence`, with one RDN per attribute.
///
/// One attribute per RelativeDistinguishedName, which is the conventional layout —
/// putting CN and O in a single RDN is legal DER and renders as `CN=x+O=y`, which
/// looks like a bug to whoever reads it in a certificate viewer.
fn name_rdn_sequence(common_name: &str) -> Vec<u8> {
    der::sequence(&[
        rdn(&COMMON_NAME, common_name),
        rdn(&ORGANIZATION, ORGANIZATION_NAME),
    ])
}

fn rdn(oid_arcs: &[u32], value: &str) -> Vec<u8> {
    der::set(&[der::sequence(&[der::oid(oid_arcs), der::utf8_string(value)])])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devicekey::software::SoftwareDeviceKey;
    use crate::devicekey::DeviceKey;
    use p256::ecdsa::signature::Verifier;

    fn key() -> (tempfile::TempDir, SoftwareDeviceKey) {
        let dir = tempfile::tempdir().unwrap();
        let key = SoftwareDeviceKey::generate(dir.path()).unwrap();
        (dir, key)
    }

    /// Minimal structural reader, for tests only.
    ///
    /// Deliberately not in `der.rs`: that module writes and never parses, because
    /// nothing it handles comes from an untrusted source. This helper reads our own
    /// output inside a test, where a malformed length is a failing assertion rather
    /// than an attack.
    fn read_tlv(bytes: &[u8]) -> (u8, &[u8], &[u8]) {
        let tag = bytes[0];
        let first = bytes[1];
        let (len, body_at) = if first < 0x80 {
            (first as usize, 2)
        } else {
            let n = (first & 0x7F) as usize;
            let mut len = 0usize;
            for i in 0..n {
                len = (len << 8) | bytes[2 + i] as usize;
            }
            (len, 2 + n)
        };
        (tag, &bytes[body_at..body_at + len], &bytes[body_at + len..])
    }

    #[test]
    fn the_request_has_the_three_rfc_2986_members_in_order() {
        let (_d, k) = key();
        let der_bytes = build_csr_der(&k, "{4c4c4544-0037-4a10-8043-b2c04f483233}").unwrap();

        let (tag, body, rest) = read_tlv(&der_bytes);
        assert_eq!(tag, der::tag::SEQUENCE);
        assert!(rest.is_empty(), "nothing follows the CertificationRequest");

        let (info_tag, info_body, after_info) = read_tlv(body);
        assert_eq!(info_tag, der::tag::SEQUENCE, "certificationRequestInfo");

        let (alg_tag, alg_body, after_alg) = read_tlv(after_info);
        assert_eq!(alg_tag, der::tag::SEQUENCE, "signatureAlgorithm");
        // ecdsa-with-SHA256 and nothing else: no NULL parameters.
        assert_eq!(alg_body, der::oid(&ECDSA_WITH_SHA256).as_slice());

        let (sig_tag, sig_body, after_sig) = read_tlv(after_alg);
        assert_eq!(sig_tag, der::tag::BIT_STRING, "signature");
        assert_eq!(sig_body[0], 0x00, "no unused bits");
        assert!(after_sig.is_empty());

        // version = 0 is the first member of the info.
        assert_eq!(&info_body[..3], &[0x02, 0x01, 0x00]);
    }

    #[test]
    fn the_signature_is_over_the_exact_info_bytes_and_verifies() {
        // This is what `csr.CheckSignature()` checks server-side, and the reason the
        // info is encoded once and both signed and embedded from the same buffer.
        let (_d, k) = key();
        let der_bytes = build_csr_der(&k, "machine-a").unwrap();
        let (_, body, _) = read_tlv(&der_bytes);

        // Re-slice the info *with* its tag and length, which is what is signed.
        let (_, info_body, after_info) = read_tlv(body);
        let info_len = body.len() - after_info.len();
        let signed_bytes = &body[..info_len];
        assert!(signed_bytes.ends_with(info_body));

        let (_, _, after) = read_tlv(after_info); // skip signatureAlgorithm
        let (_, sig_body, _) = read_tlv(after);
        let sig = p256::ecdsa::DerSignature::try_from(&sig_body[1..]).unwrap();

        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&k.public_point().unwrap()).unwrap();
        vk.verify(signed_bytes, &sig).expect("the CSR signature verifies");
    }

    #[test]
    fn the_public_key_in_the_request_is_the_devices_own() {
        let (_d, k) = key();
        let der_bytes = build_csr_der(&k, "machine-a").unwrap();
        let point = k.public_point().unwrap();
        assert!(
            der_bytes.windows(65).any(|w| w == point),
            "the request must carry this device's public point, not a re-derived one"
        );
    }

    #[test]
    fn the_subject_carries_the_machine_guid_and_our_organization() {
        let (_d, k) = key();
        let guid = "{4c4c4544-0037-4a10-8043-b2c04f483233}";
        let der_bytes = build_csr_der(&k, guid).unwrap();
        assert!(
            der_bytes.windows(guid.len()).any(|w| w == guid.as_bytes()),
            "braces and all, which is why the name is a UTF8String"
        );
        assert!(der_bytes
            .windows(ORGANIZATION_NAME.len())
            .any(|w| w == ORGANIZATION_NAME.as_bytes()));
    }

    #[test]
    fn the_attributes_field_is_present_and_empty() {
        // No requested extensions, no SAN, no key usage: the CA sets those itself,
        // and a CSR that asks for its own is at best ignored.
        let (_d, k) = key();
        let der_bytes = build_csr_der(&k, "machine-a").unwrap();
        let (_, body, _) = read_tlv(&der_bytes);
        let (_, info_body, _) = read_tlv(body);
        assert_eq!(
            &info_body[info_body.len() - 2..],
            &[der::tag::CONTEXT_0_CONSTRUCTED, 0x00]
        );
    }

    #[test]
    fn the_pem_carries_the_label_the_gateway_requires() {
        // enroll.go: `block.Type != "CERTIFICATE REQUEST"` is a `bad_csr`.
        let (_d, k) = key();
        let pem = build_csr_pem(&k, "machine-a").unwrap();
        assert!(pem.starts_with("-----BEGIN CERTIFICATE REQUEST-----\n"));
        assert!(pem.trim_end().ends_with("-----END CERTIFICATE REQUEST-----"));
    }

    #[test]
    fn two_calls_on_one_key_produce_the_same_public_key() {
        // Renewal re-certifies the same key. A builder that re-keyed per call would
        // strand the previous certificate and leave a stale device row online.
        let (_d, k) = key();
        let a = build_csr_der(&k, "machine-a").unwrap();
        let b = build_csr_der(&k, "machine-a").unwrap();
        let point = k.public_point().unwrap();
        assert!(a.windows(65).any(|w| w == point));
        assert!(b.windows(65).any(|w| w == point));
    }

    #[test]
    fn a_long_subject_crosses_the_long_form_length_boundary_intact() {
        // A CSR is around 300 bytes and every enclosing length is therefore long-form.
        // This pins the case where an inner member itself crosses 127 bytes.
        let (_d, k) = key();
        let long = "m".repeat(200);
        let der_bytes = build_csr_der(&k, &long).unwrap();
        let (_, body, rest) = read_tlv(&der_bytes);
        assert!(rest.is_empty(), "the outer length covered the whole request");
        assert!(body.windows(long.len()).any(|w| w == long.as_bytes()));
    }
}
