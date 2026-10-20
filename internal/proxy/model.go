package proxy

import (
	"bytes"
	"encoding/json"
)

// modelValue locates the top-level "model" string of a JSON object body and
// returns its decoded value together with the byte span [start,end) covering
// exactly the value literal. Only the first top-level occurrence counts;
// nested "model" keys, non-string values and non-object bodies are reported as
// unresolvable so callers forward them untouched.
func modelValue(body []byte) (value string, start, end int, ok bool) {
	dec := json.NewDecoder(bytes.NewReader(body))
	if token, err := dec.Token(); err != nil || token != json.Delim('{') {
		return "", 0, 0, false
	}
	for dec.More() {
		token, err := dec.Token()
		if err != nil {
			return "", 0, 0, false
		}
		key, isString := token.(string)
		afterKey := int(dec.InputOffset())
		var raw json.RawMessage
		if err := dec.Decode(&raw); err != nil {
			return "", 0, 0, false
		}
		if !isString || key != "model" {
			continue
		}
		raw = bytes.TrimSpace(raw)
		if len(raw) < 2 || raw[0] != '"' {
			return "", 0, 0, false // non-string model value
		}
		// The gap between the key token and the decoder position holds only
		// the colon, whitespace and the value literal, so the raw literal's
		// first occurrence in it is the exact span to replace.
		gap := body[afterKey:int(dec.InputOffset())]
		offset := bytes.Index(gap, raw)
		if offset < 0 {
			return "", 0, 0, false
		}
		if err := json.Unmarshal(raw, &value); err != nil {
			return "", 0, 0, false
		}
		return value, afterKey + offset, afterKey + offset + len(raw), true
	}
	return "", 0, 0, false
}

// spliceModelValue replaces the value literal span with name, JSON-encoded.
// Every other byte of the body is left as-is.
func spliceModelValue(body []byte, start, end int, name string) []byte {
	quoted, err := json.Marshal(name)
	if err != nil {
		return body
	}
	spliced := make([]byte, 0, len(body)-(end-start)+len(quoted))
	spliced = append(spliced, body[:start]...)
	spliced = append(spliced, quoted...)
	spliced = append(spliced, body[end:]...)
	return spliced
}
