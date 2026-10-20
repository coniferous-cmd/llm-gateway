package keymap

import (
	"fmt"
	"sync"
	"sync/atomic"
	"testing"

	"llm-gateway/internal/config"
)

func TestAuthentication(t *testing.T) {
	m := New(map[string][]config.Target{"dev": {{BaseURL: "one"}}, "empty": nil})
	for _, header := range []string{"", "Basic dev", "Bearer", "Bearer unknown", "Bearer dev extra", "Bearer empty"} {
		if _, ok := m.Select(header); ok {
			t.Fatalf("accepted %q", header)
		}
	}
	for i := 0; i < 6; i++ {
		target, ok := m.Select("bearer dev")
		if !ok || target.BaseURL != "one" {
			t.Fatal(target, ok)
		}
	}
}

func TestRoundRobin(t *testing.T) {
	targets := []config.Target{{BaseURL: "1"}, {BaseURL: "2"}, {BaseURL: "3"}}
	m := New(map[string][]config.Target{"dev": targets, "other": targets})
	targets[0].BaseURL = "mutated"
	for i := 0; i < 6; i++ {
		target, ok := m.Select("Bearer dev")
		if !ok || target.BaseURL != fmt.Sprint(i%3+1) {
			t.Fatal(target, ok)
		}
	}
	target, _ := m.Select("Bearer other")
	if target.BaseURL != "1" {
		t.Fatal("counters are not independent")
	}
}

func TestConcurrentSelection(t *testing.T) {
	m := New(map[string][]config.Target{"dev": {{BaseURL: "1"}, {BaseURL: "2"}, {BaseURL: "3"}}})
	var counts [3]atomic.Int64
	var wg sync.WaitGroup
	for i := 0; i < 300; i++ {
		wg.Go(func() {
			target, ok := m.Select("Bearer dev")
			if !ok {
				t.Error("authentication failed")
				return
			}
			counts[int(target.BaseURL[0]-'1')].Add(1)
		})
	}
	wg.Wait()
	for i := range counts {
		if counts[i].Load() != 100 {
			t.Fatal("unbalanced selection")
		}
	}
}
