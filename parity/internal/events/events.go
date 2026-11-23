// Package events reads and writes the JSON lines event log that the workload driver produces.
package events

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"time"
)

// Event is one line of the event log. Kinds are started, sent, signal,
// receiver_ready, printed and exiting.
type Event struct {
	Time   time.Time      `json:"time"`
	Kind   string         `json:"kind"`
	Detail map[string]any `json:"detail,omitempty"`
}

// Append writes e as one JSON line at the end of the file at path, creating the file if needed.
func Append(path string, e Event) error {
	line, err := json.Marshal(e)
	if err != nil {
		return err
	}
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return err
	}
	if _, err := f.Write(append(line, '\n')); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}

// Read returns every event in the file at path, in file order.
func Read(path string) ([]Event, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	var out []Event
	scanner := bufio.NewScanner(f)
	scanner.Buffer(nil, 1<<20)
	for n := 1; scanner.Scan(); n++ {
		if len(scanner.Bytes()) == 0 {
			continue
		}
		var e Event
		if err := json.Unmarshal(scanner.Bytes(), &e); err != nil {
			return nil, fmt.Errorf("%s line %d: %w", path, n, err)
		}
		out = append(out, e)
	}
	return out, scanner.Err()
}
