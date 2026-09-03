package ca_test

import (
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"math/big"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/ca"
)

// A fixed clock, so the validity-window assertions are about the arithmetic rather
// than about how long the test took to run.
var t0 = time.Date(2026, 9, 1, 12, 0, 0, 0, time.UTC)

type authority struct {
	cert     *x509.Certificate
	key      crypto.Signer
	certPath string
	keyPath  string
}

// newAuthority writes a usable intermediate to a temporary directory. opts mutates
// the template before signing, which is how the negative cases produce a certificate
// that is wrong in exactly one way.
func newAuthority(t *testing.T, opts ...func(*x509.Certificate)) authority {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	tmpl := &x509.Certificate{
		SerialNumber:          big.NewInt(7),
		Subject:               pkix.Name{CommonName: "Sentinel Device Issuing CA"},
		NotBefore:             t0.Add(-24 * time.Hour),
		NotAfter:              t0.AddDate(5, 0, 0),
		IsCA:                  true,
		BasicConstraintsValid: true,
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageCRLSign,
		MaxPathLen:            0,
		MaxPathLenZero:        true,
	}
	for _, o := range opts {
		o(tmpl)
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	cert, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	dir := t.TempDir()
	a := authority{
		cert:     cert,
		key:      key,
		certPath: filepath.Join(dir, "ca.crt"),
		keyPath:  filepath.Join(dir, "ca.key"),
	}
	writePEM(t, a.certPath, "CERTIFICATE", der)
	keyDER, err := x509.MarshalPKCS8PrivateKey(key)
	if err != nil {
		t.Fatal(err)
	}
	writePEM(t, a.keyPath, "PRIVATE KEY", keyDER)
	return a
}

func writePEM(t *testing.T, path, typ string, der []byte) {
	t.Helper()
	if err := os.WriteFile(path, pem.EncodeToMemory(&pem.Block{Type: typ, Bytes: der}), 0o600); err != nil {
		t.Fatal(err)
	}
}

func (a authority) load(t *testing.T) *ca.FileCA {
	t.Helper()
	signer, err := ca.Load(ca.Config{CertPath: a.certPath, KeyPath: a.keyPath})
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	signer.Now = func() time.Time { return t0 }
	return signer
}

// csrFor builds a PKCS#10 request for a freshly generated key of the given shape.
func csrFor(t *testing.T, key crypto.Signer) *x509.CertificateRequest {
	t.Helper()
	der, err := x509.CreateCertificateRequest(rand.Reader,
		&x509.CertificateRequest{Subject: pkix.Name{CommonName: "ignored"}}, key)
	if err != nil {
		t.Fatal(err)
	}
	csr, err := x509.ParseCertificateRequest(der)
	if err != nil {
		t.Fatal(err)
	}
	return csr
}

func p256(t *testing.T) crypto.Signer {
	t.Helper()
	k, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	return k
}

const (
	tenant = "11111111-1111-1111-1111-111111111111"
	device = "22222222-2222-2222-2222-222222222222"
)

// ------------------------------------------------------------------- loading

func TestLoadRejectsEveryOperatorMistakeAtStartupRatherThanAtFirstEnrollment(t *testing.T) {
	good := newAuthority(t)
	other := newAuthority(t)

	dir := t.TempDir()
	chainWithKey := filepath.Join(dir, "chain-with-key.pem")
	keyDER, err := x509.MarshalPKCS8PrivateKey(other.key)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(chainWithKey, append(
		pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: other.cert.Raw}),
		pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: keyDER})...), 0o600); err != nil {
		t.Fatal(err)
	}

	twoCerts := filepath.Join(dir, "two.crt")
	if err := os.WriteFile(twoCerts, append(
		pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: good.cert.Raw}),
		pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: other.cert.Raw})...), 0o600); err != nil {
		t.Fatal(err)
	}

	encryptedKey := filepath.Join(dir, "encrypted.key")
	if err := os.WriteFile(encryptedKey, pem.EncodeToMemory(&pem.Block{
		Type:    "RSA PRIVATE KEY",
		Headers: map[string]string{"Proc-Type": "4,ENCRYPTED", "DEK-Info": "AES-256-CBC,0000"},
		Bytes:   []byte("not really encrypted"),
	}), 0o600); err != nil {
		t.Fatal(err)
	}

	emptyChain := filepath.Join(dir, "empty-chain.pem")
	if err := os.WriteFile(emptyChain, pem.EncodeToMemory(&pem.Block{
		Type: "CERTIFICATE REQUEST", Bytes: []byte("not a certificate"),
	}), 0o600); err != nil {
		t.Fatal(err)
	}

	notACA := newAuthority(t, func(c *x509.Certificate) {
		c.IsCA = false
		c.MaxPathLenZero = false
		c.KeyUsage = x509.KeyUsageDigitalSignature
	})
	pathLen := newAuthority(t, func(c *x509.Certificate) {
		c.MaxPathLen = 2
		c.MaxPathLenZero = false
	})

	cases := []struct {
		name string
		cfg  ca.Config
		// want is a fragment of the error, so a reader of a failing test can see
		// which refusal was expected without decoding the message.
		want string
	}{
		{"no paths at all", ca.Config{}, "certificate and a key path are required"},
		{"key path only", ca.Config{KeyPath: good.keyPath}, "certificate and a key path are required"},
		{"certificate file missing", ca.Config{CertPath: filepath.Join(dir, "nope.crt"), KeyPath: good.keyPath}, "read certificate"},
		{"key file missing", ca.Config{CertPath: good.certPath, KeyPath: filepath.Join(dir, "nope.key")}, "read key"},
		{"key belongs to another intermediate", ca.Config{CertPath: good.certPath, KeyPath: other.keyPath}, "does not match the certificate"},
		{"a leaf pointed at the CA variable", ca.Config{CertPath: notACA.certPath, KeyPath: notACA.keyPath}, "is not a CA certificate"},
		{"an intermediate that may mint further CAs", ca.Config{CertPath: pathLen.certPath, KeyPath: pathLen.keyPath}, "permits issuing further CAs"},
		{"a whole chain supplied as the certificate", ca.Config{CertPath: twoCerts, KeyPath: good.keyPath}, "more than one certificate"},
		{"a passphrase-protected key", ca.Config{CertPath: good.certPath, KeyPath: encryptedKey}, "passphrase-encrypted"},
		{"a chain file carrying a private key", ca.Config{CertPath: good.certPath, KeyPath: good.keyPath, ChainPath: chainWithKey}, "contains a private key"},
		{"a chain file with nothing usable in it", ca.Config{CertPath: good.certPath, KeyPath: good.keyPath, ChainPath: emptyChain}, "no CERTIFICATE block"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := ca.Load(tc.cfg)
			if err == nil {
				t.Fatalf("expected a refusal mentioning %q, got a working CA", tc.want)
			}
			if !strings.Contains(err.Error(), tc.want) {
				t.Fatalf("error %q does not mention %q", err, tc.want)
			}
		})
	}
}

func TestLoadingAValidIntermediateReportsWhatItLoaded(t *testing.T) {
	a := newAuthority(t)
	signer := a.load(t)
	if !signer.NotAfter().Equal(a.cert.NotAfter) {
		t.Fatalf("NotAfter %s, want %s", signer.NotAfter(), a.cert.NotAfter)
	}
	if !strings.Contains(signer.Subject(), "Sentinel Device Issuing CA") {
		t.Fatalf("subject %q", signer.Subject())
	}
}

func TestTheChainIsTheIntermediateFollowedByWhateverWasConfigured(t *testing.T) {
	a := newAuthority(t)
	root := newAuthority(t)
	chainPath := filepath.Join(t.TempDir(), "chain.pem")
	// Deliberately CRLF-terminated with a PKCS#12-style preamble, which is how
	// chain files arrive from a customer PKI team.
	raw := "Bag Attributes\r\n    friendlyName: root\r\n" +
		strings.ReplaceAll(string(pem.EncodeToMemory(
			&pem.Block{Type: "CERTIFICATE", Bytes: root.cert.Raw})), "\n", "\r\n")
	if err := os.WriteFile(chainPath, []byte(raw), 0o600); err != nil {
		t.Fatal(err)
	}
	signer, err := ca.Load(ca.Config{CertPath: a.certPath, KeyPath: a.keyPath, ChainPath: chainPath})
	if err != nil {
		t.Fatalf("load with chain: %v", err)
	}
	if n := strings.Count(signer.ChainPEM(), "BEGIN CERTIFICATE"); n != 2 {
		t.Fatalf("chain holds %d certificates, want the intermediate plus the root", n)
	}
	if strings.Contains(signer.ChainPEM(), "\r") || strings.Contains(signer.ChainPEM(), "Bag Attributes") {
		t.Fatal("the chain was concatenated rather than re-encoded")
	}
}

// -------------------------------------------------------------------- signing

func TestASignedDeviceCertificateIsUsableForClientAuthenticationAndNothingElse(t *testing.T) {
	a := newAuthority(t)
	signer := a.load(t)

	certPEM, chainPEM, err := signer.Sign(csrFor(t, p256(t)), tenant, device, t0.AddDate(1, 0, 0))
	if err != nil {
		t.Fatalf("sign: %v", err)
	}
	block, _ := pem.Decode([]byte(certPEM))
	if block == nil {
		t.Fatal("issued certificate is not PEM")
	}
	cert, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		t.Fatal(err)
	}

	if cert.KeyUsage != x509.KeyUsageDigitalSignature {
		t.Fatalf("key usage %b, want digitalSignature alone", cert.KeyUsage)
	}
	if len(cert.ExtKeyUsage) != 1 || cert.ExtKeyUsage[0] != x509.ExtKeyUsageClientAuth {
		t.Fatalf("extended key usage %v, want clientAuth alone", cert.ExtKeyUsage)
	}
	if cert.IsCA {
		t.Fatal("a device certificate must not be a CA")
	}
	if len(cert.DNSNames)+len(cert.IPAddresses)+len(cert.URIs)+len(cert.EmailAddresses) != 0 {
		t.Fatal("a device certificate carries no subject alternative names")
	}
	if cert.Subject.CommonName != device {
		t.Fatalf("common name %q, want the device id", cert.Subject.CommonName)
	}
	if len(cert.Subject.OrganizationalUnit) != 1 || cert.Subject.OrganizationalUnit[0] != tenant {
		t.Fatalf("organizational unit %v, want the tenant id", cert.Subject.OrganizationalUnit)
	}
	if cert.SerialNumber.Sign() <= 0 || cert.SerialNumber.BitLen() < 96 {
		t.Fatalf("serial %s is not a 128-bit positive integer", cert.SerialNumber)
	}
	if !cert.NotBefore.Before(t0) {
		t.Fatal("notBefore must be backdated to absorb endpoint clock skew")
	}
	// The fingerprint the enrollment handler stores has to be derivable from what
	// we returned, or mTLS cannot resolve the device on the next connection.
	if auth.CertFingerprint(cert) == "" {
		t.Fatal("no fingerprint")
	}

	// And the certificate must actually chain to the intermediate for client
	// authentication, which is the only verification that will ever be run on it.
	pool := x509.NewCertPool()
	pool.AddCert(a.cert)
	if _, err := cert.Verify(x509.VerifyOptions{
		Roots:       pool,
		CurrentTime: t0.AddDate(0, 6, 0),
		KeyUsages:   []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
	}); err != nil {
		t.Fatalf("issued certificate does not verify for client auth: %v", err)
	}
	if _, err := cert.Verify(x509.VerifyOptions{
		Roots:       pool,
		CurrentTime: t0.AddDate(0, 6, 0),
		KeyUsages:   []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
	}); err == nil {
		t.Fatal("issued certificate verifies for server authentication; the EKU is not doing its job")
	}
	if !strings.Contains(chainPEM, "BEGIN CERTIFICATE") {
		t.Fatal("no chain returned for the device to trust")
	}
}

func TestTwoSignaturesOverTheSameRequestProduceDistinctCertificates(t *testing.T) {
	// devices.cert_fingerprint carries a unique index. Two devices whose
	// certificates hashed to the same value would collide there, and the second
	// enrollment would take over the first machine's identity.
	signer := newAuthority(t).load(t)
	csr := csrFor(t, p256(t))
	first, _, err := signer.Sign(csr, tenant, device, t0.AddDate(1, 0, 0))
	if err != nil {
		t.Fatal(err)
	}
	second, _, err := signer.Sign(csr, tenant, device, t0.AddDate(1, 0, 0))
	if err != nil {
		t.Fatal(err)
	}
	if first == second {
		t.Fatal("re-signing the same request produced an identical certificate")
	}
}

func TestSigningRefusesWeakDeviceKeys(t *testing.T) {
	signer := newAuthority(t).load(t)

	rsa1024, err := rsa.GenerateKey(rand.Reader, 1024)
	if err != nil {
		t.Fatal(err)
	}
	rsa2048, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	p384, err := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	p224, err := ecdsa.GenerateKey(elliptic.P224(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}

	cases := []struct {
		name    string
		key     crypto.Signer
		wantErr string
	}{
		{"P-256, what the endpoint generates", p256(t), ""},
		{"P-384", p384, ""},
		{"RSA-2048", rsa2048, ""},
		{"P-224 is below the floor", p224, "not permitted"},
		{"RSA-1024 is below the floor", rsa1024, "below the 2048-bit floor"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			// P-224 cannot be used to self-sign a CSR with a modern hash in some
			// configurations, so build the request from whatever the key supports
			// and let Sign judge the key rather than the request.
			csr := csrFor(t, tc.key)
			_, _, err := signer.Sign(csr, tenant, device, t0.AddDate(1, 0, 0))
			switch {
			case tc.wantErr == "" && err != nil:
				t.Fatalf("expected a certificate, got %v", err)
			case tc.wantErr != "" && err == nil:
				t.Fatalf("expected a refusal mentioning %q", tc.wantErr)
			case tc.wantErr != "" && !strings.Contains(err.Error(), tc.wantErr):
				t.Fatalf("error %q does not mention %q", err, tc.wantErr)
			}
		})
	}
}

func TestSigningRefusesAnRsaKeyWithASmallPublicExponent(t *testing.T) {
	// e = 3 is legal and has a long history of padding attacks. Go's key generator
	// will not produce one, so the request is assembled by hand from a valid key
	// with the exponent swapped — which is also how such a key would reach us.
	signer := newAuthority(t).load(t)
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	csr := csrFor(t, key)
	pub := *key.Public().(*rsa.PublicKey)
	pub.E = 3
	csr.PublicKey = &pub

	if _, _, err := signer.Sign(csr, tenant, device, t0.AddDate(1, 0, 0)); err == nil {
		t.Fatal("signed a certificate for an RSA key with e = 3")
	} else if !strings.Contains(err.Error(), "public exponent") {
		t.Fatalf("error %q does not name the exponent", err)
	}
}

func TestTheValidityWindowHasToSurviveTheRenewalCadenceTheEndpointImplements(t *testing.T) {
	a := newAuthority(t)
	signer := a.load(t)

	cases := []struct {
		name     string
		notAfter time.Time
		wantErr  string
	}{
		{"a year, which is what enrollment asks for", t0.AddDate(1, 0, 0), ""},
		{"ninety days, comfortably past the renewal lead time", t0.Add(90 * 24 * time.Hour), ""},
		{"twenty days, inside the renewal lead time", t0.Add(20 * 24 * time.Hour), "re-enrols immediately and loops"},
		{"already expired", t0.Add(-time.Hour), "re-enrols immediately and loops"},
		{"ten years", t0.AddDate(10, 0, 0), "the cap is"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, _, err := signer.Sign(csrFor(t, p256(t)), tenant, device, tc.notAfter)
			switch {
			case tc.wantErr == "" && err != nil:
				t.Fatalf("expected a certificate, got %v", err)
			case tc.wantErr != "" && err == nil:
				t.Fatalf("expected a refusal mentioning %q", tc.wantErr)
			case tc.wantErr != "" && !strings.Contains(err.Error(), tc.wantErr):
				t.Fatalf("error %q does not mention %q", err, tc.wantErr)
			}
		})
	}
}

func TestALeafIsNeverIssuedPastTheIntermediatesOwnExpiry(t *testing.T) {
	// An intermediate with 200 days left. A one-year device certificate under it
	// would look valid on the machine and stop verifying on day 200, which presents
	// as the whole fleet dropping off at once for no reason a log explains.
	a := newAuthority(t, func(c *x509.Certificate) {
		c.NotAfter = t0.Add(200 * 24 * time.Hour)
	})
	signer := a.load(t)

	// Comfortably inside MaxValidity and comfortably past the intermediate.
	_, _, err := signer.Sign(csrFor(t, p256(t)), tenant, device, t0.Add(300*24*time.Hour))
	if err == nil {
		t.Fatal("issued a leaf that outlives its issuer")
	}
	if !strings.Contains(err.Error(), "after the issuing intermediate expires") {
		t.Fatalf("error %q does not explain the problem", err)
	}
	// Inside the intermediate's own window it still signs.
	if _, _, err := signer.Sign(csrFor(t, p256(t)), tenant, device, t0.Add(150*24*time.Hour)); err != nil {
		t.Fatalf("a window inside the intermediate's was refused: %v", err)
	}
}

func TestAnExpiredIntermediateRefusesToSignRatherThanIssuingDeadCertificates(t *testing.T) {
	a := newAuthority(t)
	signer := a.load(t)
	// Six years on: the intermediate in the fixture is good for five.
	signer.Now = func() time.Time { return t0.AddDate(6, 0, 0) }
	_, _, err := signer.Sign(csrFor(t, p256(t)), tenant, device, t0.AddDate(7, 0, 0))
	if err == nil {
		t.Fatal("an expired intermediate signed a certificate")
	}
}

func TestSigningRefusesAMissingTenantOrDevice(t *testing.T) {
	signer := newAuthority(t).load(t)
	for _, tc := range []struct{ name, tenant, device string }{
		{"no tenant", "", device},
		{"no device", tenant, ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if _, _, err := signer.Sign(csrFor(t, p256(t)), tc.tenant, tc.device, t0.AddDate(1, 0, 0)); err == nil {
				t.Fatal("signed an unattributable certificate")
			}
		})
	}
}
