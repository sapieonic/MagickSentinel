package auth

import (
	"crypto/x509"
	"encoding/pem"
	"errors"
)

func parseCertPEM(s string) (*x509.Certificate, error) {
	block, _ := pem.Decode([]byte(s))
	if block == nil {
		return nil, errors.New("auth: not a PEM block")
	}
	return x509.ParseCertificate(block.Bytes)
}
