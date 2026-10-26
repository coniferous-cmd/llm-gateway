package conversationlog

import (
	"bytes"
	"encoding/base64"
	"io"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestCapture(t *testing.T) {
	for _, tc := range []struct {
		name                     string
		data                     []byte
		limit                    int
		length                   int64
		eof, complete, truncated bool
		encoding                 string
	}{
		{"empty", nil, 3, 0, false, true, false, "utf-8"},
		{"exact", []byte("abc"), 3, 3, false, true, false, "utf-8"},
		{"overflow", []byte("abcd"), 3, 4, false, true, true, "utf-8"},
		{"unknown", []byte("abc"), 3, -1, false, false, false, "utf-8"},
		{"unknown-eof", []byte("abc"), 3, -1, true, true, false, "utf-8"},
		{"partial", []byte("a"), 3, 3, false, false, false, "utf-8"},
		{"binary", []byte{0xff, 0xfe}, 3, 2, false, true, false, "base64"},
		{"utf8-prefix", []byte("你"), 2, 3, false, true, true, "base64"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			c := NewCapture(tc.limit, tc.length)
			var err error
			if tc.eof {
				err = io.EOF
			}
			c.Observe(tc.data, err)
			b := c.Snapshot()
			if b.Complete != tc.complete || b.Truncated != tc.truncated || b.Encoding != tc.encoding || b.ObservedBytes != int64(len(tc.data)) || b.CapturedBytes != min(tc.limit, len(tc.data)) {
				t.Fatalf("%+v", b)
			}
			got := []byte(b.Data)
			if b.Encoding == "base64" {
				got, err = base64.StdEncoding.DecodeString(b.Data)
				if err != nil {
					t.Fatal(err)
				}
			}
			if !bytes.Equal(got, tc.data[:b.CapturedBytes]) {
				t.Fatalf("bytes: %x", got)
			}
		})
	}
}

func TestReaderCaptureAndRace(t *testing.T) {
	c := NewCapture(5, -1)
	r := c.Wrap(io.NopCloser(strings.NewReader("abcdefgh")))
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		for i := 0; i < 100; i++ {
			_ = c.Snapshot()
		}
	}()
	data, err := io.ReadAll(r)
	if err != nil {
		t.Fatal(err)
	}
	_ = r.Close()
	wg.Wait()
	if string(data) != "abcdefgh" || !c.Snapshot().Complete || !c.Snapshot().Truncated {
		t.Fatal(c.Snapshot())
	}
	c.Observe(nil, io.ErrUnexpectedEOF)
	if c.Snapshot().Complete {
		t.Fatal("read failure marked complete")
	}
}

func TestCallFinish(t *testing.T) {
	now := time.Date(2026, 10, 26, 1, 2, 3, 0, time.UTC)
	c := &Call{Request: NewCapture(4, 0), Response: NewCapture(4, -1), record: Record{RequestID: "id", StartedAt: now}}
	c.Response.Observe([]byte("ok"), nil)
	c.Fail("secret raw error")
	r := c.Finish(now.Add(time.Second), 429, "origin", "id", "name")
	if !r.ResponseComplete || r.DurationMS != 1000 || r.Error != "" {
		t.Fatalf("%+v", r)
	}
	c.Fail("upstream_transport")
	c.Fail("canceled")
	r = c.Finish(now, 200, "", "", "")
	if r.ResponseComplete || r.Error != "canceled" {
		t.Fatalf("%+v", r)
	}
}
