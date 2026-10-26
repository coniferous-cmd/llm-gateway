package conversationlog

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"
)

func testStore(t *testing.T) (*Store, *bytes.Buffer) {
	t.Helper()
	var logs bytes.Buffer
	s := New(t.TempDir(), 8, slog.New(slog.NewJSONHandler(&logs, nil)))
	return s, &logs
}
func recordFiles(t *testing.T, s *Store) []string {
	t.Helper()
	files, err := filepath.Glob(filepath.Join(s.directory, "*", "*.json"))
	if err != nil {
		t.Fatal(err)
	}
	return files
}
func TestStoreConcurrent(t *testing.T) {
	s, _ := testStore(t)
	s.now = func() time.Time { return time.Date(2026, 10, 26, 1, 2, 3, 0, time.FixedZone("offset", 3600)) }
	var wg sync.WaitGroup
	for i := 0; i < 20; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			c, err := s.Begin("POST", "/v1/chat", 0)
			if err != nil {
				t.Error(err)
				return
			}
			s.Finish(c, 200, "http://origin", "", "")
		}()
	}
	wg.Wait()
	files := recordFiles(t, s)
	if len(files) != 20 {
		t.Fatalf("files %d", len(files))
	}
	for _, path := range files {
		data, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		var r Record
		if err = json.Unmarshal(data, &r); err != nil {
			t.Fatal(err)
		}
		if !strings.Contains(filepath.ToSlash(path), "2026-10-26/000203_") || r.RequestID == "" {
			t.Fatal(path)
		}
		if runtime.GOOS != "windows" {
			info, _ := os.Stat(path)
			if info.Mode().Perm() != 0600 {
				t.Fatal(info.Mode())
			}
			info, _ = os.Stat(filepath.Dir(path))
			if info.Mode().Perm() != 0700 {
				t.Fatal(info.Mode())
			}
		}
	}
}

func TestCollisionDoesNotOverwrite(t *testing.T) {
	s, _ := testStore(t)
	n := 0
	s.id = func() (string, error) {
		n++
		if n <= 2 {
			return strings.Repeat("a", 32), nil
		}
		return strings.Repeat("b", 32), nil
	}
	s.now = func() time.Time { return time.Date(2026, 10, 26, 0, 0, 0, 0, time.UTC) }
	for _, method := range []string{"FIRST", "SECOND"} {
		c, err := s.Begin(method, "/", 0)
		if err != nil {
			t.Fatal(err)
		}
		s.Finish(c, 200, "", "", "")
	}
	files := recordFiles(t, s)
	if len(files) != 2 {
		t.Fatalf("files %v", files)
	}
	data, _ := os.ReadFile(files[0])
	if !bytes.Contains(data, []byte("FIRST")) {
		t.Fatal("overwritten")
	}
}

type failingFile struct {
	tempFile
	stage string
}

func (f failingFile) Write(p []byte) (int, error) {
	if f.stage == "write" {
		return 0, errors.New("secret")
	}
	return f.tempFile.Write(p)
}
func (f failingFile) Close() error {
	err := f.tempFile.Close()
	if f.stage == "close" {
		return errors.New("secret")
	}
	return err
}
func TestPersistenceFailuresAndRecovery(t *testing.T) {
	for _, stage := range []string{"mkdir", "create", "write", "close", "publish", "id"} {
		t.Run(stage, func(t *testing.T) {
			s, logs := testStore(t)
			mkdir, create, publish, id := s.mkdir, s.create, s.publish, s.id
			fail := errors.New("secret-token ?api_key=secret-body")
			switch stage {
			case "mkdir":
				s.mkdir = func(string, os.FileMode) error { return fail }
			case "create":
				s.create = func(string) (tempFile, error) { return nil, fail }
			case "write", "close":
				s.create = func(dir string) (tempFile, error) { f, err := create(dir); return failingFile{f, stage}, err }
			case "publish":
				s.publish = func(string, string) error { return fail }
			case "id":
				s.id = func() (string, error) { return "", fail }
			}
			c, err := s.Begin("POST", "/", 0)
			if err == nil {
				s.Finish(c, 200, "", "", "")
			}
			if logs.Len() == 0 || strings.Contains(logs.String(), "secret") {
				t.Fatal(logs.String())
			}
			if len(recordFiles(t, s)) != 0 {
				t.Fatal("published failed record")
			}
			leftovers, _ := filepath.Glob(filepath.Join(s.directory, "*", "*.tmp"))
			if len(leftovers) != 0 {
				t.Fatal(leftovers)
			}
			s.mkdir, s.create, s.publish, s.id = mkdir, create, publish, id
			c, err = s.Begin("POST", "/", 0)
			if err != nil {
				t.Fatal(err)
			}
			s.Finish(c, 200, "", "", "")
			if len(recordFiles(t, s)) != 1 {
				t.Fatal("did not recover")
			}
		})
	}
}
func TestInitializationFailure(t *testing.T) {
	dir := t.TempDir()
	blocked := filepath.Join(dir, "blocked")
	if err := os.WriteFile(blocked, []byte("x"), 0600); err != nil {
		t.Fatal(err)
	}
	var logs bytes.Buffer
	s := New(blocked, 8, slog.New(slog.NewJSONHandler(&logs, nil)))
	if logs.Len() == 0 {
		t.Fatal("missing initialization diagnostic")
	}
	if err := os.Remove(blocked); err != nil {
		t.Fatal(err)
	}
	c, err := s.Begin("POST", "/", 0)
	if err != nil {
		t.Fatal(err)
	}
	s.Finish(c, 200, "", "", "")
	if len(recordFiles(t, s)) != 1 {
		t.Fatal("did not recover")
	}
}

var _ io.Writer = failingFile{}
