// Package blob stores media segments.
//
// Segments are written to object storage and only referenced from Postgres, so the
// database stays small enough to be operated normally while audio volume scales with
// the floor. Keys are deterministic — derived from tenant, call, channel and
// sequence — which is what makes a duplicate upload after a reconnect overwrite
// itself rather than create a second object.
package blob

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"
)

// Store is the storage interface. S3 in production, MinIO in dev, memory in tests.
type Store interface {
	Put(ctx context.Context, key string, body []byte) error
	Get(ctx context.Context, key string) ([]byte, error)
	Delete(ctx context.Context, key string) error
}

var ErrNotFound = errors.New("blob: not found")

// SegmentKey is the canonical layout. Date-partitioned so a retention sweep can
// delete a day's audio by prefix rather than by row.
func SegmentKey(tenantID, day, callID string, channel uint8, seq uint32) string {
	return fmt.Sprintf("audio/%s/%s/%s/%d/%08d.opus", tenantID, day, callID, channel, seq)
}

// Memory is an in-memory store for tests.
type Memory struct {
	mu   sync.RWMutex
	data map[string][]byte
}

func NewMemory() *Memory { return &Memory{data: map[string][]byte{}} }

func (m *Memory) Put(_ context.Context, key string, body []byte) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.data[key] = append([]byte(nil), body...)
	return nil
}

func (m *Memory) Get(_ context.Context, key string) ([]byte, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	b, ok := m.data[key]
	if !ok {
		return nil, ErrNotFound
	}
	return append([]byte(nil), b...), nil
}

func (m *Memory) Delete(_ context.Context, key string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	delete(m.data, key)
	return nil
}

func (m *Memory) Len() int {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return len(m.data)
}

// Dir is a filesystem-backed store for local development without MinIO.
type Dir struct{ Root string }

func (d Dir) path(key string) string { return filepath.Join(d.Root, filepath.FromSlash(key)) }

func (d Dir) Put(_ context.Context, key string, body []byte) error {
	p := d.path(key)
	if err := os.MkdirAll(filepath.Dir(p), 0o750); err != nil {
		return err
	}
	return os.WriteFile(p, body, 0o640)
}

func (d Dir) Get(_ context.Context, key string) ([]byte, error) {
	b, err := os.ReadFile(d.path(key))
	if errors.Is(err, os.ErrNotExist) {
		return nil, ErrNotFound
	}
	return b, err
}

func (d Dir) Delete(_ context.Context, key string) error {
	err := os.Remove(d.path(key))
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	return err
}
