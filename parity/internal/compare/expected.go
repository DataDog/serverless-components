package compare

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"regexp"
	"slices"

	"gopkg.in/yaml.v3"

	"github.com/DataDog/serverless-components/parity/internal/diff"
)

// expectedDifference is one entry of expected-differences.yaml.
type expectedDifference struct {
	Scenario string `yaml:"scenario"`
	Area     string `yaml:"area"`
	// Path is a path pattern, as in the package documentation.
	Path string `yaml:"path"`
	// Allowed is only_in_actual (the reference has no such value), only_in_reference (the actual
	// capture has none) or any.
	Allowed string `yaml:"allowed"`
	// Justification names the implementation-specific behavior entry in the spec.
	Justification string `yaml:"justification"`
	Reason        string `yaml:"reason"`
}

var (
	// undecoded is left out: a payload that was not compared is never expected.
	expectedAreas   = []string{"series", "sketches", "logs", "traces", "stats", "check_runs", "process", "timing"}
	expectedAllowed = []string{"only_in_actual", "only_in_reference", "any"}
	specEntry       = regexp.MustCompile(`^[a-z][a-z0-9-]*(\.[a-z0-9-]+)+$`)
)

// ApplyExpected marks the differences that an entry of the file at expectedPath allows for the
// scenario, and sets their justification. It returns an error if the file is invalid.
func ApplyExpected(diffs []diff.Difference, scenario string, expectedPath string) ([]diff.Difference, error) {
	entries, err := loadExpected(expectedPath)
	if err != nil {
		return nil, err
	}
	out := slices.Clone(diffs)
	for i, d := range out {
		for _, e := range entries {
			if e.Scenario == scenario && e.Area == d.Area && matchPattern(e.Path, d.Path) && allows(e.Allowed, d) {
				out[i].Expected, out[i].Justification = true, e.Justification
				break
			}
		}
	}
	return out, nil
}

func allows(allowed string, d diff.Difference) bool {
	switch allowed {
	case "only_in_actual":
		return d.Reference == nil && d.Actual != nil
	case "only_in_reference":
		return d.Reference != nil && d.Actual == nil
	}
	return allowed == "any"
}

func loadExpected(path string) ([]expectedDifference, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var file struct {
		Differences []expectedDifference `yaml:"differences"`
	}
	decoder := yaml.NewDecoder(bytes.NewReader(raw))
	decoder.KnownFields(true)
	if err := decoder.Decode(&file); err != nil && !errors.Is(err, io.EOF) {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	for i, e := range file.Differences {
		var problem string
		switch {
		case e.Scenario == "":
			problem = "has no scenario"
		case !slices.Contains(expectedAreas, e.Area):
			problem = fmt.Sprintf("has the unknown area %q", e.Area)
		case e.Path == "":
			problem = "has no path"
		case !slices.Contains(expectedAllowed, e.Allowed):
			problem = fmt.Sprintf("has the unknown allowed difference %q, want one of %v", e.Allowed, expectedAllowed)
		case e.Justification == "":
			problem = "has no justification"
		case !specEntry.MatchString(e.Justification):
			problem = fmt.Sprintf("has the justification %q, which is not a spec entry such as implementation.local-priority-0-drop", e.Justification)
		default:
			continue
		}
		return nil, fmt.Errorf("%s: entry %d %s", path, i+1, problem)
	}
	return file.Differences, nil
}
