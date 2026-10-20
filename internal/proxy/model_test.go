package proxy

import "testing"

func TestModelValue(t *testing.T) {
	tests := []struct {
		name, body, want string
		spliced          string // expected body after splicing "real"; empty when unresolvable
	}{
		{"compact", `{"model":"alias","n":1}`, "alias", `{"model":"real","n":1}`},
		{"pretty printed", "{\n  \"n\" : 1 ,\n  \"model\" : \"alias\"\n}", "alias", "{\n  \"n\" : 1 ,\n  \"model\" : \"real\"\n}"},
		{"escaped value", `{"model":"ali\u0061s","n":1}`, "alias", `{"model":"real","n":1}`},
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
