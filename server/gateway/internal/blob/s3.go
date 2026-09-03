package blob

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"strings"

	"github.com/aws/aws-sdk-go-v2/aws"
	awsconfig "github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"
	"github.com/aws/smithy-go"
)

// DefaultRegion is ap-south-1 (Mumbai).
//
// It is a default rather than a required setting because getting it wrong is a
// data-residency incident, not a misconfiguration: OPEN-4 in docs/open-decisions.md
// records the working assumption that all storage stays in an India region, and a
// deployment that forgot to set a region should land in Mumbai rather than in
// us-east-1, which is where the AWS SDK's own default resolution ends up when a
// container has no region configured. An operator who genuinely needs another region
// has to say so, which is exactly the conversation OPEN-4 asks for.
const DefaultRegion = "ap-south-1"

// S3Config configures the production object store.
type S3Config struct {
	Bucket string
	Region string
	// Endpoint overrides the AWS endpoint. Set for MinIO in development; empty in
	// production. Setting it also switches on path-style addressing, because MinIO
	// and most S3-compatible servers do not resolve bucket-name subdomains.
	Endpoint string
	// SSE selects server-side encryption: "AES256" (SSE-S3, the default),
	// "aws:kms", or "none".
	SSE string
	// KMSKeyID is required for SSE "aws:kms" and ignored otherwise.
	KMSKeyID string
	// AccessKeyID and SecretAccessKey force static credentials. Leave both empty in
	// production so the default chain picks up the instance or IRSA role: a
	// long-lived key pair in the gateway's environment is a credential that has to
	// be rotated by redeploying, and this one can read every call recording the
	// product has ever captured.
	AccessKeyID     string
	SecretAccessKey string
}

// S3 is the production Store.
//
// Keys are passed through exactly as SegmentKey produced them. That matters beyond
// tidiness: the retention sweep deletes a day of audio with one prefix delete against
// `audio/{tenant}/{day}/` (OPEN-6), so anything that rewrote, prefixed or flattened
// the key here would leave the sweep unable to find the objects it is supposed to
// purge — and audio nothing can delete is a retention-policy breach that shows up
// only in an audit.
type S3 struct {
	client   *s3.Client
	bucket   string
	region   string
	sse      types.ServerSideEncryption
	kmsKeyID string
}

var _ Store = (*S3)(nil)

// OpenS3 builds the client. It does not talk to S3; call Ping for that.
func OpenS3(ctx context.Context, cfg S3Config) (*S3, error) {
	if cfg.Bucket == "" {
		return nil, errors.New("blob: an S3 bucket name is required")
	}
	region := cfg.Region
	if region == "" {
		region = DefaultRegion
	}
	// Everything that can be decided from the configuration alone is decided
	// before the AWS SDK is asked for anything. LoadDefaultConfig consults the
	// environment, the shared config files and, on an instance, the metadata
	// service; a bad SSE setting should not be reported after a metadata timeout,
	// and it makes these refusals testable without AWS.
	sse, err := serverSideEncryption(cfg)
	if err != nil {
		return nil, err
	}

	opts := []func(*awsconfig.LoadOptions) error{awsconfig.WithRegion(region)}
	if cfg.AccessKeyID != "" || cfg.SecretAccessKey != "" {
		if cfg.AccessKeyID == "" || cfg.SecretAccessKey == "" {
			return nil, errors.New("blob: an S3 access key needs both an id and a secret")
		}
		opts = append(opts, awsconfig.WithCredentialsProvider(
			credentials.NewStaticCredentialsProvider(cfg.AccessKeyID, cfg.SecretAccessKey, "")))
	}
	awsCfg, err := awsconfig.LoadDefaultConfig(ctx, opts...)
	if err != nil {
		return nil, fmt.Errorf("blob: load AWS configuration: %w", err)
	}

	s3opts := []func(*s3.Options){}
	if cfg.Endpoint != "" {
		endpoint := cfg.Endpoint
		s3opts = append(s3opts, func(o *s3.Options) {
			o.BaseEndpoint = aws.String(endpoint)
			o.UsePathStyle = true
		})
	}

	return &S3{
		client:   s3.NewFromConfig(awsCfg, s3opts...),
		bucket:   cfg.Bucket,
		region:   region,
		sse:      sse,
		kmsKeyID: cfg.KMSKeyID,
	}, nil
}

// serverSideEncryption resolves the SSE setting.
//
// Encryption at rest is on unless an operator explicitly turns it off, and turning it
// off is spelled "none" rather than left blank. An empty setting meaning "no
// encryption" would make an unset environment variable — a deploy that forgot a
// line — silently produce a bucket of unencrypted borrower call audio, which is the
// one outcome a bank's security review will not forgive.
func serverSideEncryption(cfg S3Config) (types.ServerSideEncryption, error) {
	switch strings.ToLower(strings.TrimSpace(cfg.SSE)) {
	case "", "aes256", "sse-s3":
		if cfg.KMSKeyID != "" {
			return "", errors.New("blob: a KMS key id was given but SSE is AES256; set SSE to aws:kms")
		}
		return types.ServerSideEncryptionAes256, nil
	case "aws:kms", "kms":
		if cfg.KMSKeyID == "" {
			// SSE-KMS without a key id falls back to the account's default
			// aws/s3 key, which is shared with everything else in the account
			// and cannot carry a key policy that names this bucket. If someone
			// asked for KMS they wanted a key they control.
			return "", errors.New("blob: SSE aws:kms requires a KMS key id")
		}
		return types.ServerSideEncryptionAwsKms, nil
	case "none", "off":
		// MinIO in development, and nothing else. The caller logs this.
		return "", nil
	default:
		return "", fmt.Errorf("blob: unknown SSE mode %q; use AES256, aws:kms or none", cfg.SSE)
	}
}

// Region reports the resolved region, so the caller can say out loud where borrower
// audio is about to be written.
func (s *S3) Region() string { return s.region }

// Bucket reports the resolved bucket, for the same reason.
func (s *S3) Bucket() string { return s.bucket }

// Encrypted reports whether server-side encryption is in force.
func (s *S3) Encrypted() bool { return s.sse != "" }

func (s *S3) Put(ctx context.Context, key string, body []byte) error {
	in := &s3.PutObjectInput{
		Bucket: aws.String(s.bucket),
		Key:    aws.String(key),
		Body:   bytes.NewReader(body),
		// Set explicitly so a browser that is ever handed a presigned URL plays
		// the segment rather than downloading it as application/octet-stream.
		ContentType: aws.String("audio/opus"),
	}
	if s.sse != "" {
		in.ServerSideEncryption = s.sse
		if s.kmsKeyID != "" {
			in.SSEKMSKeyId = aws.String(s.kmsKeyID)
		}
	}
	if _, err := s.client.PutObject(ctx, in); err != nil {
		return fmt.Errorf("blob: put %s: %w", key, err)
	}
	return nil
}

func (s *S3) Get(ctx context.Context, key string) ([]byte, error) {
	out, err := s.client.GetObject(ctx, &s3.GetObjectInput{
		Bucket: aws.String(s.bucket),
		Key:    aws.String(key),
	})
	if err != nil {
		if isNotFound(err) {
			return nil, ErrNotFound
		}
		return nil, fmt.Errorf("blob: get %s: %w", key, err)
	}
	defer out.Body.Close()
	b, err := io.ReadAll(out.Body)
	if err != nil {
		return nil, fmt.Errorf("blob: read %s: %w", key, err)
	}
	return b, nil
}

// Delete removes an object, treating an absent one as success.
//
// S3's DeleteObject is already idempotent, but the retention sweep depends on that
// being true of this interface rather than of the backend underneath it: it deletes
// each audio object before its database row so that a failed delete leaves the row
// for the next run to retry (docs/security.md §6). A second run must therefore be
// able to delete an object the first run already removed without reporting failure
// and stalling the row forever.
func (s *S3) Delete(ctx context.Context, key string) error {
	if _, err := s.client.DeleteObject(ctx, &s3.DeleteObjectInput{
		Bucket: aws.String(s.bucket),
		Key:    aws.String(key),
	}); err != nil {
		if isNotFound(err) {
			return nil
		}
		return fmt.Errorf("blob: delete %s: %w", key, err)
	}
	return nil
}

// Ping checks that the bucket exists and the credentials can reach it.
//
// HeadBucket rather than a write probe. Readiness is polled continuously, and a probe
// object written on every poll would land outside the `audio/{tenant}/{day}/` layout
// the retention sweep deletes by prefix — so the bucket would slowly fill with
// objects no sweep can ever collect. HeadBucket exercises the same credential path
// and costs nothing.
func (s *S3) Ping(ctx context.Context) error {
	if _, err := s.client.HeadBucket(ctx, &s3.HeadBucketInput{Bucket: aws.String(s.bucket)}); err != nil {
		return fmt.Errorf("blob: bucket %s unreachable: %w", s.bucket, err)
	}
	return nil
}

// isNotFound recognises the several ways S3 says "no such object".
//
// The typed errors are checked first, but they are not enough on their own: an
// S3-compatible server, and S3 itself on a HEAD, answers with a bare 404 that the SDK
// surfaces as a generic smithy response error with no modelled shape.
func isNotFound(err error) bool {
	var nsk *types.NoSuchKey
	if errors.As(err, &nsk) {
		return true
	}
	var nsb *types.NoSuchBucket
	if errors.As(err, &nsb) {
		return false // a missing bucket is a configuration error, not a missing object
	}
	var nf *types.NotFound
	if errors.As(err, &nf) {
		return true
	}
	var api smithy.APIError
	if errors.As(err, &api) {
		switch api.ErrorCode() {
		case "NoSuchKey", "NotFound", "404":
			return true
		}
	}
	return false
}

// Ping on the development backends, so readiness has something to check whichever
// backend is configured and /readyz does not have to know which one it got.

// Ping verifies the directory exists and is writable.
//
// A write probe rather than a stat, because the failure this is here to catch is a
// volume that mounted read-only or filled up, and both of those stat perfectly well.
// The probe file sits outside the `audio/` prefix so it cannot be mistaken for a
// segment.
func (d Dir) Ping(ctx context.Context) error {
	const probe = ".sentinel-readyz"
	if err := d.Put(ctx, probe, []byte("ok")); err != nil {
		return fmt.Errorf("blob: %s is not writable: %w", d.Root, err)
	}
	return d.Delete(ctx, probe)
}

// Ping always succeeds: an in-memory store cannot be unreachable.
func (m *Memory) Ping(context.Context) error { return nil }

// Prober is implemented by every backend in this package. /readyz uses it so that a
// gateway with a dead object store reports itself unready instead of accepting ingest
// it cannot durably store — losing call audio is the one failure this product cannot
// absorb, and it is silent.
type Prober interface {
	Ping(ctx context.Context) error
}

var (
	_ Prober = (*S3)(nil)
	_ Prober = Dir{}
	_ Prober = (*Memory)(nil)
)
