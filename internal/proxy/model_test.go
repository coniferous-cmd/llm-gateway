package proxy

import (
	"encoding/json"
	"testing"
)

func TestModelValue(t *testing.T) {
	tests := []struct {
		name, body, want string
		spliced          string // expected body after splicing "real"; empty when unresolvable
	}{
		{"compact", `{"model":"alias","n":1}`, "alias", `{"model":"real","n":1}`},
		{"pretty printed", "{\n  \"n\" : 1 ,\n  \"model\" : \"alias\"\n}", "alias", "{\n  \"n\" : 1 ,\n  \"model\" : \"real\"\n}"},
		{"escaped value", `{"model":"alias","n":1}`, "alias", `{"model":"real","n":1}`},
		{"duplicate key first wins", `{"model":"first","model":"second"}`, "first", `{"model":"real","model":"second"}`},
		{"model after nested object", `{"nested":{"model":"inner"},"model":"outer"}`, "outer", `{"nested":{"model":"inner"},"model":"real"}`},
		{"nested only", `{"nested":{"model":"inner"}}`, "", ""},
		{"numeric model", `{"model":123}`, "", ""},
		{"null model", `{"model":null}`, "", ""},
		{"object model", `{"model":{"id":1}}`, "", ""},
		{"array body", `[{"model":"alias"}]`, "", ""},
		{"scalar body", `"model"`, "", ""},
		{"malformed", `{"model":`, "", ""},
		{"empty", ``, "", ""},
		{"text body", "not JSON at all", "", ""},
		{"binary body", "\x00raw\n", "", ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			value, start, end, ok := modelValue([]byte(tt.body))
			if tt.want == "" {
				if ok {
					t.Fatalf("resolved %q from %q", value, tt.body)
				}
				return
			}
			if !ok || value != tt.want {
				t.Fatalf("modelValue(%q) = %q, [%d:%d), %v; want %q", tt.body, value, start, end, ok, tt.want)
			}
			if got := string(spliceModelValue([]byte(tt.body), start, end, "real")); got != tt.spliced {
				t.Fatalf("spliced = %q, want %q", got, tt.spliced)
			}
		})
	}
}

// FuzzModelValue pins the invariants of the hand-written JSON scanner: it
// never panics, a resolved span always covers exactly a JSON string literal
// that decodes to the reported value, and splicing leaves every other byte
// untouched.
func FuzzModelValue(f *testing.F) {
	f.Add(`{"model":"alias"}`)
	f.Add(`{"model":"a","model":"b"}`)
	f.Add(`{"nested":{"model":"x"}}`)
	f.Add(`{"model":1}`)
	f.Add(`{"model":" ali as "}`)
	f.Add(`{}`)
	f.Add(`[{"model":"x"}]`)
	f.Add(`{"model":`)
	f.Add("")
	f.Fuzz(func(t *testing.T, body string) {
		value, start, end, ok := modelValue([]byte(body))
		if !ok {
			return
		}
		if start < 0 || end > len(body) || start >= end {
			t.Fatalf("span [%d:%d) outside body %q", start, end, body)
		}
		var decoded string
		if err := json.Unmarshal([]byte(body[start:end]), &decoded); err != nil || decoded != value {
			t.Fatalf("span %q decodes as %q, %v; want %q", body[start:end], decoded, err, value)
		}
		quoted, err := json.Marshal("real")
		if err != nil {
			t.Fatal(err)
		}
		want := string(body[:start]) + string(quoted) + string(body[end:])
		if got := string(spliceModelValue([]byte(body), start, end, "real")); got != want {
			t.Fatalf("splice = %q, want %q", got, want)
		}
	})
}

func TestSplicePreservesSurroundingBytes(t *testing.T) {
	body := []byte("{\n\t\"keep\" : \"ünïcodé ✓\",\n  \"model\"   :   \"alias\" ,\n  \"n\": 2e10\n}")
	value, start, end, ok := modelValue(body)
	if !ok || value != "alias" {
		t.Fatal(value, ok)
	}
	want := "{\n\t\"keep\" : \"ünïcodé ✓\",\n  \"model\"   :   \"模型-真实\" ,\n  \"n\": 2e10\n}"
	if got := string(spliceModelValue(body, start, end, "模型-真实")); got != want {
		t.Fatalf("spliced = %q, want %q", got, want)
	}
}
