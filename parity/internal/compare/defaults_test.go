package compare

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/DataDog/serverless-components/parity/internal/normalize"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
)

func TestLoadDefaults(t *testing.T) {
	for _, tc := range []struct {
		name, contents, wantErr string
		want                    []scenario.Override
	}{
		{"valid", "overrides:\n  - path: series[metric=m,*].value\n    comparator: greater_than\n    value: 0\n    reason: Varies.\n", "",
			[]scenario.Override{{Path: "series[metric=m,*].value", Comparator: "greater_than", Value: 0}}},
		{"no reason", "overrides:\n  - path: p\n    comparator: exact\n", "has no reason", nil},
		{"unknown comparator", "overrides:\n  - path: p\n    comparator: close_to\n    reason: R.\n", `unknown comparator "close_to"`, nil},
		{"no path", "overrides:\n  - comparator: exact\n    reason: R.\n", "has no path", nil},
	} {
		t.Run(tc.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "compare-defaults.yaml")
			if err := os.WriteFile(path, []byte(tc.contents), 0o644); err != nil {
				t.Fatal(err)
			}
			got, err := LoadDefaults(path)
			if tc.wantErr != "" {
				if err == nil || !strings.Contains(err.Error(), tc.wantErr) {
					t.Fatalf("error %v, want it to contain %q", err, tc.wantErr)
				}
				return
			}
			if err != nil || !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("got %+v, %v, want %+v", got, err, tc.want)
			}
		})
	}
}

// A default applies to every scenario, and a scenario's own override for the same path wins.
func TestDefaultsApplyUnlessTheScenarioOverrides(t *testing.T) {
	cpu := func(sum float64) normalize.Normalized {
		return normalize.Normalized{Sketches: []normalize.Sketch{{Metric: "cpu", Points: []normalize.SketchPoint{{Count: 2, Sum: sum, Min: 1, Max: 9}}}}}
	}
	defaults := []scenario.Override{{Path: "sketches[metric=cpu,tags=].sum", Comparator: "greater_than", Value: 0}}
	exact := []scenario.Override{{Path: "sketches[metric=cpu,tags=].sum", Comparator: "exact"}}
	for _, tc := range []struct {
		name      string
		scenario  []scenario.Override
		actualSum float64
		want      []string
	}{
		{"default holds", nil, 7, nil},
		{"default fails", nil, 0, []string{"sketches sketches[metric=cpu,tags=].sum"}},
		{"scenario override wins", exact, 7, []string{"sketches sketches[metric=cpu,tags=].sum"}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got := paths(Content(cpu(5), cpu(tc.actualSum), WithDefaults(tc.scenario, defaults), tolerances))
			if !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("got %q\nwant %q", got, tc.want)
			}
		})
	}
}

func TestCompareDefaultsFileLoads(t *testing.T) {
	if got, err := LoadDefaults("../../compare-defaults.yaml"); err != nil || len(got) == 0 {
		t.Fatalf("got %d overrides and error %v", len(got), err)
	}
}
