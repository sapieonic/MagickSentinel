package blob_test

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/magickvoice/sentinel/server/gateway/internal/blob"
)

func TestSegmentKeyStaysDayPartitioned(t *testing.T) {
	// blob.SegmentKey's layout is load-bearing outside this package: the retention
	// sweep deletes a day of audio with one prefix delete against
	// `audio/{tenant}/{day}/` rather than row by row (OPEN-6 in
	// docs/open-decisions.md, and docs/security.md §6). Another work stream is
	// implementing that sweep against this shape, so this test exists to fail if
	// the layout is ever rearranged — including by an object-store backend that
	// decided to prefix or flatten keys of its own.
	key := blob.SegmentKey("11111111-1111-1111-1111-111111111111", "2026-09-01",
		"01J8ZQ8H2Q7X9K3M4N5P6R7S8T", 1, 42)
	const want = "audio/11111111-1111-1111-1111-111111111111/2026-09-01/" +
		"01J8ZQ8H2Q7X9K3M4N5P6R7S8T/1/00000042.opus"
	if key != want {
		t.Fatalf("segment key\n got %s\nwant %s", key, want)
	}
	// The day has to be the third segment, because that is what a prefix delete
	// truncates at.
	parts := strings.Split(key, "/")
	if len(parts) < 3 || parts[0] != "audio" || parts[2] != "2026-09-01" {
		t.Fatalf("a day-prefix delete could not address this key: %s", key)
	}
	// And the channel has to stay in the key: the two audio channels are never
	// mixed anywhere in this system (docs/architecture.md), object storage
	// included.
	if parts[4] != "1" {
		t.Fatalf("the channel is not in the key: %s", key)
	}
}

func TestOpenS3RefusesConfigurationsItCannotHonour(t *testing.T) {
	// These are all decidable from the configuration alone and are checked before
	// the AWS SDK is asked for anything, so this test needs no credentials, no
	// network and no instance metadata service.
	cases := []struct {
		name string
		cfg  blob.S3Config
		want string
	}{
		{"no bucket", blob.S3Config{}, "bucket name is required"},
		{
			"an access key id with no secret",
			blob.S3Config{Bucket: "b", AccessKeyID: "AKIA"},
			"needs both an id and a secret",
		},
		{
			"a secret with no access key id",
			blob.S3Config{Bucket: "b", SecretAccessKey: "s"},
			"needs both an id and a secret",
		},
		{
			"KMS with no key, which would silently use the shared account key",
			blob.S3Config{Bucket: "b", SSE: "aws:kms"},
			"requires a KMS key id",
		},
		{
			"a KMS key with AES256, which would silently ignore the key",
			blob.S3Config{Bucket: "b", SSE: "AES256", KMSKeyID: "arn:aws:kms:..."},
			"set SSE to aws:kms",
		},
		{
			"an SSE mode nobody implements",
			blob.S3Config{Bucket: "b", SSE: "rot13"},
			"unknown SSE mode",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := blob.OpenS3(context.Background(), tc.cfg)
			if err == nil {
				t.Fatalf("expected a refusal mentioning %q", tc.want)
			}
			if !strings.Contains(err.Error(), tc.want) {
				t.Fatalf("error %q does not mention %q", err, tc.want)
			}
		})
	}
}

func TestS3DefaultsToIndiaAndToEncryptionBeingOn(t *testing.T) {
	// Both defaults are the ones a forgotten environment variable should land on.
	// An unset region must not resolve through the SDK to us-east-1, and an unset
	// SSE setting must not mean "write borrower call audio unencrypted".
	store, err := blob.OpenS3(context.Background(), blob.S3Config{Bucket: "sentinel-audio"})
	if err != nil {
		t.Skipf("the AWS SDK could not build a configuration in this environment: %v", err)
	}
	if store.Region() != "ap-south-1" {
		t.Errorf("region %q, want ap-south-1 (OPEN-4 assumes India-only residency)", store.Region())
	}
	if !store.Encrypted() {
		t.Error("server-side encryption must be on unless it was explicitly turned off")
	}

	off, err := blob.OpenS3(context.Background(), blob.S3Config{Bucket: "b", SSE: "none"})
	if err != nil {
		t.Fatal(err)
	}
	if off.Encrypted() {
		t.Error("SSE \"none\" should turn it off; that is what MinIO in development needs")
	}
}

func TestEveryBackendCanBeProbedForReadiness(t *testing.T) {
	// /readyz asserts blob.Prober rather than switching on the concrete type, so
	// every backend has to satisfy it — otherwise a gateway reports itself ready
	// on an object store it never examined and then loses call audio silently.
	var probers = map[string]blob.Prober{
		"memory": blob.NewMemory(),
		"dir":    blob.Dir{Root: t.TempDir()},
	}
	for name, p := range probers {
		t.Run(name, func(t *testing.T) {
			if err := p.Ping(context.Background()); err != nil {
				t.Fatalf("ping: %v", err)
			}
		})
	}
}

func TestTheDirProbeIsAWriteNotAStat(t *testing.T) {
	// The failure this is here to catch is a volume that mounted read-only or
	// filled up, and both of those stat perfectly well.
	root := filepath.Join(t.TempDir(), "audio")
	if err := os.MkdirAll(root, 0o500); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Chmod(root, 0o700) })

	d := blob.Dir{Root: root}
	if os.Geteuid() == 0 {
		// Root ignores the mode bits, so there is nothing to assert here; the
		// positive case above still covers the probe running at all.
		t.Skip("running as root: a read-only directory is still writable")
	}
	if err := d.Ping(context.Background()); err == nil {
		t.Fatal("a read-only directory reported itself ready")
	}
}

func TestTheDirProbeLeavesNothingBehindForTheRetentionSweepToMiss(t *testing.T) {
	// Readiness is polled continuously. A probe object left in place — especially
	// one outside the `audio/{tenant}/{day}/` layout — would be an object no day
	// prefix delete can ever collect.
	root := t.TempDir()
	d := blob.Dir{Root: root}
	if err := d.Ping(context.Background()); err != nil {
		t.Fatal(err)
	}
	entries, err := os.ReadDir(root)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 0 {
		t.Fatalf("the probe left %d entries behind: %v", len(entries), entries)
	}
}

func TestAMissingObjectIsErrNotFoundOnEveryBackend(t *testing.T) {
	// The retention sweep and the evidence packer both distinguish "gone" from
	// "broken": deleting an object that is already absent must succeed, so a
	// second sweep can finish a first one that failed halfway.
	ctx := context.Background()
	for name, store := range map[string]blob.Store{
		"memory": blob.NewMemory(),
		"dir":    blob.Dir{Root: t.TempDir()},
	} {
		t.Run(name, func(t *testing.T) {
			if _, err := store.Get(ctx, "audio/t/2026-09-01/c/0/00000000.opus"); !errors.Is(err, blob.ErrNotFound) {
				t.Fatalf("Get on a missing key returned %v, want blob.ErrNotFound", err)
			}
			if err := store.Delete(ctx, "audio/t/2026-09-01/c/0/00000000.opus"); err != nil {
				t.Fatalf("Delete on a missing key returned %v, want nil", err)
			}
		})
	}
}
