package compare

import (
	"bytes"
	"fmt"
	"os"
	"slices"

	"gopkg.in/yaml.v3"

	"github.com/DataDog/serverless-components/parity/internal/scenario"
)

// LoadDefaults reads compare-defaults.yaml: comparator overrides that apply to every scenario.
// Every entry needs a path, a known comparator and a reason.
func LoadDefaults(path string) ([]scenario.Override, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var file struct {
		Overrides []struct {
			Path       string `yaml:"path"`
			Comparator string `yaml:"comparator"`
			Value      any    `yaml:"value"`
			Reason     string `yaml:"reason"`
		} `yaml:"overrides"`
	}
	decoder := yaml.NewDecoder(bytes.NewReader(data))
	decoder.KnownFields(true)
	if err := decoder.Decode(&file); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	var out []scenario.Override
	for i, o := range file.Overrides {
		switch {
		case o.Path == "":
			return nil, fmt.Errorf("%s: override %d has no path", path, i+1)
		case !slices.Contains(scenario.Comparators, o.Comparator):
			return nil, fmt.Errorf("%s: override %d has the unknown comparator %q", path, i+1, o.Comparator)
		case o.Reason == "":
			return nil, fmt.Errorf("%s: override %d has no reason", path, i+1)
		}
		out = append(out, scenario.Override{Path: o.Path, Comparator: o.Comparator, Value: o.Value})
	}
	return out, nil
}

// WithDefaults returns the overrides of a scenario followed by the defaults. The first override
// whose pattern matches a path applies, so a scenario's own override wins.
func WithDefaults(scenarioOverrides, defaults []scenario.Override) []scenario.Override {
	return append(slices.Clone(scenarioOverrides), defaults...)
}
