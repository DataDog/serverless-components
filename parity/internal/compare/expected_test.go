package compare

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/DataDog/serverless-components/parity/internal/diff"
)

func writeExpected(t *testing.T, content string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "expected-differences.yaml")
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

const priorityZeroEntry = `differences:
  - scenario: traces.sampling
    area: traces
    path: traces[priority=0,*]
    allowed: only_in_actual
    justification: implementation.local-priority-0-drop
    reason: Rust forwards unsampled priority-0 traces.
`

func TestExpectedDifferenceIsMarked(t *testing.T) {
	path := writeExpected(t, priorityZeroEntry)
	chunk := "traces[priority=0,service=web,name=http.request,resource=GET /]"
	extra := diff.Difference{Area: "traces", Path: chunk, Actual: "chunk"}
	missing := diff.Difference{Area: "traces", Path: chunk, Reference: "chunk"}
	field := diff.Difference{Area: "traces", Path: chunk + ".chunk.spans[0].meta[error.message]", Actual: "boom"}
	sampled := diff.Difference{Area: "traces", Path: "traces[priority=1,service=web,name=http.request,resource=GET /]", Actual: "chunk"}
	marked := extra
	marked.Expected, marked.Justification = true, "implementation.local-priority-0-drop"
	for _, tc := range []struct {
		name     string
		scenario string
		diff     diff.Difference
		want     diff.Difference
	}{
		{"extra priority-0 chunk", "traces.sampling", extra, marked},
		{"other scenario", "traces.basic", extra, extra},
		{"missing priority-0 chunk", "traces.sampling", missing, missing},
		{"field inside a chunk", "traces.sampling", field, field},
		{"sampled chunk", "traces.sampling", sampled, sampled},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := ApplyExpected([]diff.Difference{tc.diff}, tc.scenario, path)
			if err != nil {
				t.Fatal(err)
			}
			checkDifferences(t, got, []diff.Difference{tc.want})
		})
	}
}

func TestExpectedDifferencesRejectInvalidEntries(t *testing.T) {
	valid := map[string]string{
		"scenario": "traces.sampling", "area": "traces", "path": "traces[priority=0,*]",
		"allowed": "only_in_actual", "justification": "implementation.local-priority-0-drop",
	}
	for _, tc := range []struct{ name, key, value, want string }{
		{"missing justification", "justification", "", "justification"},
		{"justification is not a spec entry", "justification", "Rust is different", "spec entry"},
		{"missing scenario", "scenario", "", "scenario"},
		{"unknown area", "area", "metrics", "area"},
		{"undecoded can't be expected", "area", "undecoded", "area"},
		{"missing path", "path", "", "path"},
		{"unknown allowed difference", "allowed", "some", "allowed"},
		{"unknown key", "comment", "x", "comment"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			entry := "differences:\n  -"
			for _, key := range []string{"scenario", "area", "path", "allowed", "justification", "comment"} {
				value, ok := valid[key]
				if key == tc.key {
					value, ok = tc.value, tc.value != ""
				}
				if ok {
					entry += " " + key + ": " + `"` + value + `"` + "\n   "
				}
			}
			_, err := ApplyExpected(nil, "traces.sampling", writeExpected(t, entry))
			if err == nil || !strings.Contains(err.Error(), tc.want) {
				t.Fatalf("got error %v, want an error about %s", err, tc.want)
			}
		})
	}
}

// TestExtraPriorityZeroChunkIsExpected runs Content, ApplyExpected with the real
// expected-differences.yaml, and Status, for the forwarded priority-0 chunk.
func TestExtraPriorityZeroChunkIsExpected(t *testing.T) {
	sampled := chunk(1, "trace-1", "span-1", "span-2", "span-1", "SELECT 1")
	unsampled := chunk(0, "trace-2", "span-3", "span-4", "span-3", "SELECT 2")
	reference := traces(sampled)
	for _, tc := range []struct {
		name     string
		scenario string
		want     string
	}{
		{"traces.sampling", "traces.sampling", "expected-only"},
		{"another scenario", "traces.basic", "fail"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			diffs, err := ApplyExpected(Content(reference, traces(sampled, unsampled), nil, tolerances), tc.scenario, "../../expected-differences.yaml")
			if err != nil {
				t.Fatal(err)
			}
			if got := Status(&reference, diffs); got != tc.want {
				t.Fatalf("got %q for %#v, want %q", got, diffs, tc.want)
			}
		})
	}
}
