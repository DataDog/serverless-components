package events

import (
	"path/filepath"
	"reflect"
	"testing"
	"time"
)

func TestAppendAndReadRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	first := Event{Time: time.Date(2026, 1, 2, 3, 4, 5, 6, time.UTC), Kind: "started", Detail: map[string]any{"arguments": []any{"a", "b"}}}
	second := Event{Time: time.Date(2026, 1, 2, 3, 4, 6, 0, time.UTC), Kind: "exiting", Detail: map[string]any{"code": float64(7)}}
	for _, e := range []Event{first, second} {
		if err := Append(path, e); err != nil {
			t.Fatal(err)
		}
	}
	got, err := Read(path)
	if err != nil {
		t.Fatal(err)
	}
	want := []Event{first, second}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %+v, want %+v", got, want)
	}
}
