package api

import (
	"crypto"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"errors"
	"math/big"
	"net/http"
	"time"

	"github.com/magickvoice/sentinel/server/gateway/internal/auth"
	"github.com/magickvoice/sentinel/server/gateway/internal/httpx"
	"github.com/magickvoice/sentinel/server/gateway/internal/store"
)

// CertificateAuthority signs device certificates.
type CertificateAuthority interface {
	// Sign issues a device certificate for a verified CSR.
	Sign(csr *x509.CertificateRequest, tenantID, deviceID string, notAfter time.Time) (certPEM, chainPEM string, err error)
}

type enrollRequest struct {
	EnrollmentToken string `json:"enrollment_token"`
	CSRPEM          string `json:"csr_pem"`
	MachineGUID     string `json:"machine_guid"`
	HWFingerprint   string `json:"hw_fingerprint"`
	OSBuild         string `json:"os_build"`
	CaptureTier     string `json:"capture_tier"`
	AgentVersion    string `json:"agent_version"`
}

// enrollDevice exchanges a single-use enrollment token and a CSR for a device
// certificate.
//
// The private key never leaves the endpoint: it is generated in CNG, marked
// non-exportable, and only the CSR crosses the wire. Nothing here can be replayed —
// the token is consumed atomically before the certificate is signed, so a retry with
// the same token fails even if the response was lost.
func (s *Server) enrollDevice(w http.ResponseWriter, r *http.Request) {
	// The CA is checked before anything else is parsed. Main refuses to start
	// without one, so reaching this branch means either a development build or a
	// deployment that lost its secret mount — and in both cases the operator wants
	// to hear "no CA" rather than "malformed request", which is what a body check
	// first would have said for a request that was fine.
	if s.CA == nil {
		s.Metrics.Enrollment(r.Context(), "no_ca")
		httpx.WriteError(w, r, http.StatusServiceUnavailable, "no_ca",
			"no certificate authority is configured")
		return
	}
	var body enrollRequest
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, 64<<10)).Decode(&body); err != nil {
		s.Metrics.Enrollment(r.Context(), "bad_request")
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "malformed enrollment request")
		return
	}
	if body.EnrollmentToken == "" || body.CSRPEM == "" || body.MachineGUID == "" ||
		body.HWFingerprint == "" {
		s.Metrics.Enrollment(r.Context(), "bad_request")
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_request", "missing required fields")
		return
	}
	if body.CaptureTier != "A" && body.CaptureTier != "B" {
		// Tier C machines are meant to be blocked by the installer; if one reaches
		// here the installer was bypassed and it must not be enrolled.
		s.Metrics.Enrollment(r.Context(), "unsupported_tier")
		httpx.WriteError(w, r, http.StatusBadRequest, "unsupported_tier",
			"this OS build does not support audio capture")
		return
	}

	csr, err := parseAndVerifyCSR(body.CSRPEM)
	if err != nil {
		s.Metrics.Enrollment(r.Context(), "bad_csr")
		httpx.WriteError(w, r, http.StatusBadRequest, "bad_csr", err.Error())
		return
	}

	now := s.now()
	tenantID, err := s.Store.ConsumeEnrollmentToken(r.Context(), body.EnrollmentToken, now)
	if errors.Is(err, store.ErrTokenUnusable) {
		s.Metrics.Enrollment(r.Context(), "token_unusable")
		httpx.WriteError(w, r, http.StatusUnauthorized, "token_unusable",
			"enrollment token is invalid, expired, or already used")
		return
	}
	if err != nil {
		s.Metrics.Enrollment(r.Context(), "internal")
		s.fail(w, r, err)
		return
	}

	notAfter := now.AddDate(1, 0, 0)
	deviceID := newDeviceID()
	certPEM, chainPEM, err := s.CA.Sign(csr, tenantID, deviceID, notAfter)
	if err != nil {
		// The enrollment token has already been consumed at this point, and that
		// ordering is deliberate — it is what stops a retry minting a second
		// certificate — so a signing failure costs the operator a token. That is
		// the right trade, and it is also why the CA validates everything it can
		// at load time (internal/ca): a misconfigured intermediate should fail the
		// deploy, not burn a token per desktop during a floor rollout.
		s.Metrics.Enrollment(r.Context(), "sign_failed")
		s.Log.Error("enroll: sign certificate", "error", err)
		httpx.WriteError(w, r, http.StatusInternalServerError, "internal", "could not issue a certificate")
		return
	}

	fingerprint, err := fingerprintOfPEM(certPEM)
	if err != nil {
		s.Metrics.Enrollment(r.Context(), "internal")
		s.fail(w, r, err)
		return
	}
	realID, err := s.Store.RegisterDevice(r.Context(), tenantID, body.MachineGUID,
		body.HWFingerprint, fingerprint, body.OSBuild, body.CaptureTier, body.AgentVersion, notAfter)
	if err != nil {
		s.Metrics.Enrollment(r.Context(), "internal")
		s.fail(w, r, err)
		return
	}
	s.Metrics.Enrollment(r.Context(), "issued")

	_ = s.Store.Audit(r.Context(), &auth.Identity{
		TenantID: tenantID, UserUID: "system", Role: auth.RoleAdmin,
	}, "device.enroll", "device", realID, map[string]any{
		"capture_tier": body.CaptureTier, "os_build": body.OSBuild,
	})

	httpx.WriteJSON(w, http.StatusCreated, map[string]any{
		"device_id":       realID,
		"certificate_pem": certPEM,
		"ca_chain_pem":    chainPEM,
		"not_after":       notAfter.UTC(),
	})
}

func parseAndVerifyCSR(pemStr string) (*x509.CertificateRequest, error) {
	block, _ := pem.Decode([]byte(pemStr))
	if block == nil || block.Type != "CERTIFICATE REQUEST" {
		return nil, errors.New("expected a PKCS#10 CERTIFICATE REQUEST block")
	}
	csr, err := x509.ParseCertificateRequest(block.Bytes)
	if err != nil {
		return nil, errors.New("could not parse the certificate request")
	}
	// Checking the signature proves the requester holds the private key for the
	// public key it is asking us to certify.
	if err := csr.CheckSignature(); err != nil {
		return nil, errors.New("certificate request signature does not verify")
	}
	if csr.PublicKeyAlgorithm != x509.ECDSA {
		return nil, errors.New("device keys must be ECDSA P-256")
	}
	return csr, nil
}

func fingerprintOfPEM(certPEM string) (string, error) {
	block, _ := pem.Decode([]byte(certPEM))
	if block == nil {
		return "", errors.New("issued certificate is not valid PEM")
	}
	cert, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		return "", err
	}
	return auth.CertFingerprint(cert), nil
}

func newDeviceID() string {
	n, _ := rand.Int(rand.Reader, new(big.Int).Lsh(big.NewInt(1), 128))
	return n.Text(16)
}

// DevCA is a self-signed CA for development and tests.
//
// Production signs with a managed CA behind an HSM; this exists so the enrollment
// path can be exercised end to end in CI without one. It is not wired into any
// production build.
type DevCA struct {
	Cert    *x509.Certificate
	Key     crypto.Signer
	CertPEM string
}

func NewDevCA(cert *x509.Certificate, key crypto.Signer, certPEM string) *DevCA {
	return &DevCA{Cert: cert, Key: key, CertPEM: certPEM}
}

func (c *DevCA) Sign(csr *x509.CertificateRequest, tenantID, deviceID string, notAfter time.Time) (string, string, error) {
	serial, err := rand.Int(rand.Reader, new(big.Int).Lsh(big.NewInt(1), 128))
	if err != nil {
		return "", "", err
	}
	tmpl := &x509.Certificate{
		SerialNumber: serial,
		Subject: pkix.Name{
			CommonName:   deviceID,
			Organization: []string{"MagickVoice Sentinel"},
			// The tenant travels in the subject so a certificate is readable
			// without a database round trip during incident response. It is not
			// trusted for authorisation: the device row is.
			OrganizationalUnit: []string{tenantID},
		},
		NotBefore:   time.Now().Add(-5 * time.Minute),
		NotAfter:    notAfter,
		KeyUsage:    x509.KeyUsageDigitalSignature,
		ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, c.Cert, csr.PublicKey, c.Key)
	if err != nil {
		return "", "", err
	}
	certPEM := string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}))
	return certPEM, c.CertPEM, nil
}
