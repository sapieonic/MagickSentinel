package ingest

import "github.com/oklog/ulid/v2"

// formatULID renders the binary call id from a media record header as the Crockford
// base32 string the control frames use, so the two halves of the protocol agree on
// what identifies a call.
func formatULID(b [16]byte) string {
	return ulid.ULID(b).String()
}

// parseULID is the inverse, for building records in tests and for the storage layer.
func parseULID(s string) ([16]byte, error) {
	u, err := ulid.ParseStrict(s)
	if err != nil {
		return [16]byte{}, err
	}
	return u, nil
}
