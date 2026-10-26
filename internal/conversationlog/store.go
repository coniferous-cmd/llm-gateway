package conversationlog

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"time"
)

type tempFile interface {
	io.Writer
	Close() error
	Name() string
}

// Store is safe for concurrent calls. Hooks are fixed before serving requests.
type Store struct {
	directory string
	limit     int
	logger    *slog.Logger
	now       func() time.Time
	id        func() (string, error)
	mkdir     func(string, os.FileMode) error
	create    func(string) (tempFile, error)
	publish   func(string, string) error
	remove    func(string) error
}

func randomID() (string, error) {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(b[:]), nil
}

func New(directory string, limit int, logger *slog.Logger) *Store {
	if logger == nil {
		logger = slog.Default()
	}
	s := &Store{directory: directory, limit: limit, logger: logger, now: time.Now, id: randomID, mkdir: os.MkdirAll, publish: os.Link, remove: os.Remove,
		create: func(dir string) (tempFile, error) { return os.CreateTemp(dir, ".conversation-*.tmp") }}
	if err := s.mkdir(directory, 0700); err != nil {
		s.report("", "initialize", err)
	}
	return s
}

func (s *Store) Begin(method, path string, length int64) (*Call, error) {
	id, err := s.id()
	if err != nil {
		s.report("", "generate_id", err)
		return nil, err
	}
	return &Call{Request: NewCapture(s.limit, length), Response: NewCapture(s.limit, -1), record: Record{SchemaVersion: 1, RequestID: id, StartedAt: s.now().UTC(), Method: method, Path: path}}, nil
}

func (s *Store) Finish(c *Call, status int, upstream, modelID, modelName string) {
	record := c.Finish(s.now(), status, upstream, modelID, modelName)
	if err := s.save(record); err != nil {
		s.report(c.ID(), "persist", err)
	}
}

// Diagnostics deliberately avoid raw errors and paths; both can carry secrets.
func (s *Store) report(id, operation string, err error) {
	category := "io_failure"
	switch {
	case errors.Is(err, os.ErrPermission):
		category = "permission_denied"
	case errors.Is(err, os.ErrExist):
		category = "collision"
	case errors.Is(err, os.ErrNotExist):
		category = "not_found"
	}
	s.logger.Error("conversation logging failed", "request_id", id, "operation", operation, "category", category)
}

func (s *Store) save(r Record) error {
	dir := filepath.Join(s.directory, r.StartedAt.UTC().Format("2006-01-02"))
	if err := s.mkdir(dir, 0700); err != nil {
		return err
	}
	f, err := s.create(dir)
	if err != nil {
		return err
	}
	defer s.remove(f.Name())
	if err := json.NewEncoder(f).Encode(r); err != nil {
		_ = f.Close()
		return err
	}
	if err := f.Close(); err != nil {
		return err
	}
	suffix := r.RequestID
	for attempts := 0; attempts < 8; attempts++ {
		name := filepath.Join(dir, r.StartedAt.UTC().Format("150405")+"_"+suffix+".json")
		err = s.publish(f.Name(), name)
		if err == nil {
			return nil
		}
		if !errors.Is(err, os.ErrExist) {
			return err
		}
		suffix, err = s.id()
		if err != nil {
			return err
		}
	}
	return os.ErrExist
}
