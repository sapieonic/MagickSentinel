// Package ca issues the device certificates that carry the machine half of
// Sentinel's two identities.
//
// The gateway is an *intermediate* CA, not a root: the root's key belongs in an HSM
// or an offline safe, and the thing that has to be online is only the intermediate
// that signs a few hundred device certificates a year. Losing the intermediate means
// revoking one intermediate and re-enrolling a fleet; losing the root means the bank
// asks why the trust anchor for every device in production was sitting on an
// internet-facing host. So this package deliberately cannot create a CA — it only
// loads one somebody else generated — and it refuses to load a certificate that is
// not marked as a CA, so a leaf accidentally pointed at SENTINEL_CA_CERT fails at
// startup instead of producing certificates nothing will accept.
//
// What it signs is as narrow as X.509 allows. A device certificate is used for
// exactly one thing: proving to the gateway's TLS handshake which enrolled machine is
// on the other end. It is never a server, never signs code, never signs another
// certificate, and never authenticates a human. Every one of those is expressed in
// the template rather than left to the verifier's good judgement, because the
// verifier on the other side of a mutual-TLS handshake is not always ours — a
// customer's TLS-terminating proxy may be in the path, and a certificate that says
// "client authentication only" is refused by such a proxy for anything else even if
// the proxy is configured carelessly.
package ca

import (
	"crypto"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"errors"
	"fmt"
	"math/big"
	"os"
	"strings"
	"time"
)

// RenewalLeadTime is when the endpoint starts trying to renew: 30 days before
// expiry, per RENEW_WHEN_REMAINING in client/sentinel-service/src/device.rs.
//
// It is duplicated here rather than imported because the two are in different
// languages, and it is checked rather than merely documented: see MinValidity.
const RenewalLeadTime = 30 * 24 * time.Hour

// MinValidity is the shortest certificate lifetime this CA will issue.
//
// A certificate valid for less than the renewal lead time plus a working margin puts
// the fleet into a renewal loop: the service sees "fewer than 30 days remaining" the
// moment it stores the certificate, re-enrolls, gets another short-lived certificate,
// and repeats. On a 200-desktop floor that is a self-inflicted denial of service
// against our own enrollment endpoint, and it would first be noticed as an
// unexplained enrollment-token burn rate. Refusing to sign is the cheap end of that
// problem.
const MinValidity = RenewalLeadTime + 30*24*time.Hour

// MaxValidity caps a signing request at fifteen months.
//
// Certificates are meant to be issued for a year (spec 7.2). The cap exists because
// notAfter reaches Sign from the caller: a bug that computed it in the wrong unit
// would otherwise mint a certificate that outlives the product, and a device
// credential nobody can age out is the one an ex-employee's laptop still holds.
const MaxValidity = 456 * 24 * time.Hour

// Config names the PEM files the intermediate is loaded from.
type Config struct {
	// CertPath is the intermediate's own certificate.
	CertPath string
	// KeyPath is its private key: PKCS#8, SEC 1 or PKCS#1, unencrypted.
	KeyPath string
	// ChainPath is optional and holds the certificates between this intermediate
	// and the root, root included. It is what the endpoint stores as
	// ca_chain_pem and then presents when it dials the gateway.
	ChainPath string
}

// FileCA is a PEM-file-backed intermediate CA. It satisfies
// api.CertificateAuthority.
//
// Safe for concurrent use: every field is read-only after Load, and the signing
// operations underneath (ecdsa.Sign, rsa.SignPKCS1v15) are themselves safe to call
// from many goroutines.
type FileCA struct {
	cert *x509.Certificate
	key  crypto.Signer
	// chainPEM is the certificate the device should trust, followed by anything
	// between it and the root. Rendered once at load so signing does no I/O.
	chainPEM string
	// Now is injectable for tests.
	Now func() time.Time
}

var errNoCA = errors.New("ca: no certificate authority")

// Load reads and validates the intermediate.
//
// Everything checkable is checked here rather than at the first enrollment, because
// the failure modes are all operator mistakes made once at deploy time — the wrong
// file in the secret, the key of a rotated certificate, a certificate that expired
// while nobody was enrolling — and every one of them would otherwise surface as
// 500s on the enrollment endpoint during a floor rollout.
func Load(cfg Config) (*FileCA, error) {
	if cfg.CertPath == "" || cfg.KeyPath == "" {
		return nil, fmt.Errorf("%w: both a certificate and a key path are required", errNoCA)
	}
	certPEM, err := os.ReadFile(cfg.CertPath)
	if err != nil {
		return nil, fmt.Errorf("ca: read certificate %s: %w", cfg.CertPath, err)
	}
	keyPEM, err := os.ReadFile(cfg.KeyPath)
	if err != nil {
		return nil, fmt.Errorf("ca: read key %s: %w", cfg.KeyPath, err)
	}

	cert, err := parseSingleCertificate(certPEM)
	if err != nil {
		return nil, fmt.Errorf("ca: certificate %s: %w", cfg.CertPath, err)
	}
	key, err := parsePrivateKey(keyPEM)
	if err != nil {
		return nil, fmt.Errorf("ca: key %s: %w", cfg.KeyPath, err)
	}

	if !cert.IsCA || !cert.BasicConstraintsValid {
		// A leaf certificate can be used to sign if the verifier is lax, and some
		// are. Anything we issue that way is worthless the moment a correct
		// verifier is in the path, so this is refused at load rather than
		// discovered by a device that cannot connect.
		return nil, fmt.Errorf("ca: %s is not a CA certificate (basicConstraints cA is not true)", cfg.CertPath)
	}
	if cert.KeyUsage != 0 && cert.KeyUsage&x509.KeyUsageCertSign == 0 {
		return nil, fmt.Errorf("ca: %s does not carry the certificate-signing key usage", cfg.CertPath)
	}
	// MaxPathLen 0 with MaxPathLenZero set means "may not issue further CAs", which
	// is exactly right for an intermediate that only issues leaves. A negative
	// (absent) value is also fine. Anything positive means the operator handed us a
	// CA that is allowed to mint more CAs; that is not a certificate this service
	// should be holding, so say so.
	if cert.MaxPathLen > 0 {
		return nil, fmt.Errorf("ca: %s permits issuing further CAs (pathLenConstraint %d); "+
			"the online signing intermediate must be a leaf-only issuer", cfg.CertPath, cert.MaxPathLen)
	}
	if !publicKeysEqual(key.Public(), cert.PublicKey) {
		// The commonest deploy mistake by a wide margin: a rotated certificate
		// paired with the previous key, or two secrets mounted from different
		// versions. Every signature would verify against the wrong issuer.
		return nil, fmt.Errorf("ca: the key in %s does not match the certificate in %s",
			cfg.KeyPath, cfg.CertPath)
	}
	if err := checkKeyStrength(cert.PublicKey); err != nil {
		return nil, fmt.Errorf("ca: intermediate key in %s: %w", cfg.CertPath, err)
	}

	chain := string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: cert.Raw}))
	if cfg.ChainPath != "" {
		extra, err := os.ReadFile(cfg.ChainPath)
		if err != nil {
			return nil, fmt.Errorf("ca: read chain %s: %w", cfg.ChainPath, err)
		}
		normalized, err := normalizeChain(extra)
		if err != nil {
			return nil, fmt.Errorf("ca: chain %s: %w", cfg.ChainPath, err)
		}
		chain += normalized
	}

	return &FileCA{cert: cert, key: key, chainPEM: chain}, nil
}

func (c *FileCA) now() time.Time {
	if c.Now != nil {
		return c.Now()
	}
	return time.Now()
}

// NotAfter is the intermediate's own expiry, so the caller can log how long it has
// left and an operator can schedule the rotation before a floor rollout runs into it.
func (c *FileCA) NotAfter() time.Time { return c.cert.NotAfter }

// Subject is the intermediate's subject, for the same reason.
func (c *FileCA) Subject() string { return c.cert.Subject.String() }

// ChainPEM is what a device should be told to trust.
func (c *FileCA) ChainPEM() string { return c.chainPEM }

// Sign issues a device certificate for a CSR the enrollment handler has already
// checked the signature of.
//
// Two things about this certificate are load-bearing elsewhere and must not be
// "simplified" away:
//
//   - The tenant and the device id travel in the subject (OU and CN). Nothing
//     authorises off them — auth.attachDevice resolves the device by
//     auth.CertFingerprint, a SHA-256 over the DER, and reads the tenant from the
//     `devices` row — but incident response needs to be able to look at a
//     certificate pulled off a machine and say which tenant and which device it is
//     without a database.
//   - The fingerprint of what this returns is what enrollDevice stores. So the DER
//     must be stable: re-signing the same CSR must produce a *different*
//     certificate (different serial), because two devices sharing a fingerprint
//     would collide on the unique index over devices.cert_fingerprint and the
//     second enrollment would silently take over the first machine's identity.
func (c *FileCA) Sign(csr *x509.CertificateRequest, tenantID, deviceID string, notAfter time.Time) (string, string, error) {
	if c == nil || c.cert == nil || c.key == nil {
		return "", "", errNoCA
	}
	if tenantID == "" || deviceID == "" {
		return "", "", errors.New("ca: refusing to sign a certificate with no tenant or device")
	}
	if csr == nil || csr.PublicKey == nil {
		return "", "", errors.New("ca: certificate request carries no public key")
	}
	// The handler checks the CSR's self-signature; the CA independently checks the
	// key it is being asked to certify. These are different properties — a
	// requester can hold the private key for a 512-bit RSA key perfectly well — and
	// the second one is the CA's job because the certificate outlives the request
	// by a year. A weak device key means anyone who factors it can present as that
	// machine, and mTLS is one of the two things holding tenant isolation up.
	if err := checkKeyStrength(csr.PublicKey); err != nil {
		return "", "", fmt.Errorf("ca: device key: %w", err)
	}

	now := c.now()
	// Five minutes of backdating. Collections desktops are not reliably time-synced
	// (the same reason auth.Verifier carries a leeway), and a certificate that is
	// not yet valid on the machine holding it fails the handshake with an error the
	// agent reports as "device certificate rejected" — indistinguishable, from the
	// far end, from a revocation.
	notBefore := now.Add(-5 * time.Minute)

	if err := c.checkValidityWindow(now, notAfter); err != nil {
		return "", "", err
	}

	serial, err := randomSerial()
	if err != nil {
		return "", "", err
	}

	tmpl := &x509.Certificate{
		SerialNumber: serial,
		Subject: pkix.Name{
			CommonName:         deviceID,
			Organization:       []string{"MagickVoice Sentinel"},
			OrganizationalUnit: []string{tenantID},
		},
		NotBefore: notBefore,
		NotAfter:  notAfter,
		// DigitalSignature and nothing else. A TLS 1.3 client certificate proves
		// possession by signing the handshake transcript, so that single bit is the
		// entire requirement. KeyEncipherment would only matter for RSA key
		// transport, which TLS 1.3 removed; KeyAgreement, DataEncipherment and
		// CertSign are all things a device must never be able to do with this key.
		KeyUsage: x509.KeyUsageDigitalSignature,
		// Client authentication only. Without an extended key usage a certificate
		// is usable for anything the verifier will accept, including server
		// authentication — and a certificate that can serve TLS for a name is a
		// certificate that can be used to intercept one.
		ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
		// Explicitly not a CA, explicitly stated. An omitted basicConstraints
		// extension on a leaf is treated as "not a CA" by every correct verifier,
		// but stating it costs three bytes and closes the argument.
		BasicConstraintsValid: true,
		IsCA:                  false,
		// No subject alternative names, deliberately. A SAN exists so a verifier
		// can match a certificate against a name it expected — a hostname, an
		// email address, a URI. Nothing in this system matches a device
		// certificate against a name: the gateway looks it up by fingerprint. An
		// unmatched SAN is a field to keep consistent for no benefit, and a URI SAN
		// naming the tenant would put the tenant id into the TLS handshake in the
		// clear on the wire, where it is currently only inside the encrypted
		// portion of a TLS 1.3 handshake.
	}

	der, err := x509.CreateCertificate(rand.Reader, tmpl, c.cert, csr.PublicKey, c.key)
	if err != nil {
		return "", "", fmt.Errorf("ca: sign device certificate: %w", err)
	}
	certPEM := string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}))
	return certPEM, c.chainPEM, nil
}

// checkValidityWindow refuses windows that would break the renewal cadence the
// endpoint implements, and refuses to issue a leaf that outlives its issuer.
func (c *FileCA) checkValidityWindow(now, notAfter time.Time) error {
	validity := notAfter.Sub(now)
	if validity < MinValidity {
		return fmt.Errorf("ca: refusing to issue a certificate valid for %s: "+
			"the endpoint renews at %s remaining, so anything shorter than %s re-enrols immediately and loops",
			validity.Round(time.Hour), RenewalLeadTime, MinValidity)
	}
	if validity > MaxValidity {
		return fmt.Errorf("ca: refusing to issue a certificate valid for %s: the cap is %s",
			validity.Round(time.Hour), MaxValidity)
	}
	if notAfter.After(c.cert.NotAfter) {
		// A leaf outliving its issuer is valid X.509 and useless in practice: every
		// verifier rejects the chain once the intermediate expires, so the device
		// would stop connecting on the intermediate's expiry date with a
		// certificate that still looks fine to anyone reading it. Failing here
		// turns "the whole fleet dropped off on the 14th" into "enrollment refused
		// with a message naming the intermediate's expiry".
		return fmt.Errorf("ca: refusing to issue a certificate expiring %s, "+
			"after the issuing intermediate expires %s; rotate the intermediate",
			notAfter.UTC().Format(time.RFC3339), c.cert.NotAfter.UTC().Format(time.RFC3339))
	}
	if now.After(c.cert.NotAfter) {
		return fmt.Errorf("ca: the issuing intermediate expired %s",
			c.cert.NotAfter.UTC().Format(time.RFC3339))
	}
	return nil
}

// randomSerial draws a 128-bit positive serial from the CSPRNG.
//
// Random rather than sequential, and 128 bits rather than the 64 the RFCs require,
// because the serial is the only unpredictable input a CA contributes to the
// certificate it signs. A sequential or short serial is what made the SHA-1 chosen
// prefix attacks practical against real CAs: an attacker who can predict every field
// of the certificate you are about to sign can precompute a collision for it.
// SHA-256 is not currently vulnerable to that, and an entropic serial means we do not
// have to care whether that stays true.
func randomSerial() (*big.Int, error) {
	n, err := rand.Int(rand.Reader, new(big.Int).Lsh(big.NewInt(1), 128))
	if err != nil {
		return nil, fmt.Errorf("ca: draw serial: %w", err)
	}
	// RFC 5280 requires a positive integer. Zero is astronomically unlikely and
	// costs one comparison to exclude.
	return n.Add(n, big.NewInt(1)), nil
}

// checkKeyStrength refuses keys that are too weak to be worth certifying for a year.
//
// The floors are today's public baselines, not the minimum that still technically
// works: P-256 and RSA-2048 both sit at roughly 112–128 bits of security, and this
// certificate is the machine half of the pair that keeps one BPO's borrower audio away
// from another's.
func checkKeyStrength(pub crypto.PublicKey) error {
	switch k := pub.(type) {
	case *ecdsa.PublicKey:
		switch k.Curve {
		case elliptic.P256(), elliptic.P384(), elliptic.P521():
			return nil
		default:
			// Includes P-224, which Go supports and which is below the floor, and
			// any curve with unverified parameters.
			return fmt.Errorf("elliptic curve %s is not permitted; use P-256, P-384 or P-521",
				curveName(k.Curve))
		}
	case *rsa.PublicKey:
		if k.N.BitLen() < 2048 {
			return fmt.Errorf("RSA key of %d bits is below the 2048-bit floor", k.N.BitLen())
		}
		// A public exponent of 1, or an even one, is a broken key rather than a
		// weak one; e = 3 is legal but has a long history of padding attacks.
		if k.E < 65537 || k.E%2 == 0 {
			return fmt.Errorf("RSA public exponent %d is not permitted; use 65537", k.E)
		}
		return nil
	case ed25519.PublicKey:
		if len(k) != ed25519.PublicKeySize {
			return errors.New("malformed Ed25519 key")
		}
		return nil
	case nil:
		return errors.New("no public key")
	default:
		// DSA lands here, as does anything Go parsed but does not name. Refusing
		// the unknown is the only safe default for a CA: the alternative is
		// certifying a key whose strength we did not evaluate.
		return fmt.Errorf("unsupported public key type %T", pub)
	}
}

func curveName(c elliptic.Curve) string {
	if c == nil {
		return "unknown"
	}
	if p := c.Params(); p != nil && p.Name != "" {
		return p.Name
	}
	return "unnamed"
}

func publicKeysEqual(a, b crypto.PublicKey) bool {
	type equaler interface{ Equal(crypto.PublicKey) bool }
	if e, ok := a.(equaler); ok {
		return e.Equal(b)
	}
	return false
}

// parseSingleCertificate accepts exactly one CERTIFICATE block.
//
// Exactly one, not the first of several: a full chain accidentally supplied as
// SENTINEL_CA_CERT would load the leaf-most certificate and quietly issue
// certificates under whichever end of the file happened to be first. The chain
// belongs in SENTINEL_CA_CHAIN, and this says so.
func parseSingleCertificate(data []byte) (*x509.Certificate, error) {
	var found *x509.Certificate
	rest := data
	for {
		var block *pem.Block
		block, rest = pem.Decode(rest)
		if block == nil {
			break
		}
		if block.Type != "CERTIFICATE" {
			continue
		}
		cert, err := x509.ParseCertificate(block.Bytes)
		if err != nil {
			return nil, fmt.Errorf("unparseable certificate: %w", err)
		}
		if found != nil {
			return nil, errors.New("more than one certificate in the file; " +
				"the intermediate goes here and the rest of the chain in the chain file")
		}
		found = cert
	}
	if found == nil {
		return nil, errors.New("no CERTIFICATE block found")
	}
	return found, nil
}

// normalizeChain re-encodes a chain file, dropping anything that is not a
// certificate.
//
// Re-encoding rather than concatenating the bytes as read: chain files arrive from
// customer PKI teams with CRLF line endings, "Bag Attributes" preambles from
// OpenSSL's PKCS#12 export, and occasionally a private key someone did not notice
// was in there. The first two break naive PEM parsers on the client; the third is a
// key we would otherwise hand to every enrolling device in ca_chain_pem.
func normalizeChain(data []byte) (string, error) {
	var out strings.Builder
	rest := data
	n := 0
	for {
		var block *pem.Block
		block, rest = pem.Decode(rest)
		if block == nil {
			break
		}
		if block.Type != "CERTIFICATE" {
			if strings.Contains(block.Type, "PRIVATE KEY") {
				return "", errors.New("the chain file contains a private key; " +
					"ca_chain_pem is sent to every enrolling device")
			}
			continue
		}
		if _, err := x509.ParseCertificate(block.Bytes); err != nil {
			return "", fmt.Errorf("unparseable certificate in chain: %w", err)
		}
		out.Write(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: block.Bytes}))
		n++
	}
	if n == 0 {
		return "", errors.New("no CERTIFICATE block found")
	}
	return out.String(), nil
}

// parsePrivateKey accepts PKCS#8, SEC 1 (EC) and PKCS#1 (RSA), unencrypted.
//
// Encrypted PEM is refused with a message rather than a parse failure, because the
// answer — decrypt it into the secret store, or use a KMS-backed signer instead of a
// file — is not something an operator should have to infer from
// "x509: failed to parse private key".
func parsePrivateKey(data []byte) (crypto.Signer, error) {
	block, _ := pem.Decode(data)
	if block == nil {
		return nil, errors.New("no PEM block found")
	}
	if strings.Contains(string(block.Headers["Proc-Type"]), "ENCRYPTED") ||
		block.Type == "ENCRYPTED PRIVATE KEY" {
		return nil, errors.New("the key is passphrase-encrypted; " +
			"the gateway has no way to prompt for one, so store it decrypted in the secret manager")
	}

	var key crypto.PrivateKey
	var err error
	switch block.Type {
	case "PRIVATE KEY":
		key, err = x509.ParsePKCS8PrivateKey(block.Bytes)
	case "EC PRIVATE KEY":
		key, err = x509.ParseECPrivateKey(block.Bytes)
	case "RSA PRIVATE KEY":
		key, err = x509.ParsePKCS1PrivateKey(block.Bytes)
	default:
		return nil, fmt.Errorf("unexpected PEM block %q", block.Type)
	}
	if err != nil {
		return nil, err
	}
	signer, ok := key.(crypto.Signer)
	if !ok {
		return nil, fmt.Errorf("key of type %T cannot sign", key)
	}
	return signer, nil
}
