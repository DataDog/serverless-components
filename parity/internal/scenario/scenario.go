// Package scenario loads the scenario files that describe what the parity harness runs.
package scenario

import (
	"bytes"
	"errors"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"

	"gopkg.in/yaml.v3"
)

// Scenario is one parity scenario, read from one file.
type Scenario struct {
	Name        string            `yaml:"name"`
	Priority    string            `yaml:"priority"`
	Features    []string          `yaml:"features"`
	Mode        string            `yaml:"mode"`
	Platform    string            `yaml:"platform"`
	Environment map[string]string `yaml:"environment"`
	Workload    []Step            `yaml:"workload"`
	Align       Align             `yaml:"align,omitempty"`
	// Metadata sets how the fake Google Cloud metadata server answers. Only Google Cloud
	// platforms run that server.
	Metadata Metadata `yaml:"metadata,omitempty"`
	Stop     Stop     `yaml:"stop"`
	Compare  Compare  `yaml:"compare"`
	// CoverageGap says what the scenario can't check yet of the features it names. The report
	// marks such scenarios, and the parity bar counts them.
	CoverageGap string `yaml:"coverage_gap,omitempty"`
	// ReportOnly scenarios are measured and reported, but never pass or fail, and the parity bar
	// leaves them out.
	ReportOnly bool `yaml:"report_only,omitempty"`
}

// Step is one workload action. Args holds the step's value when it is a mapping;
// any other value is stored under the key "value". Then holds the nested steps of
// a wait_for_signal step.
type Step struct {
	At   time.Duration
	Kind string
	Args map[string]any
	Then []Step
}

// Align fixes a run's phase against the wall clock, so that Go's 10-second metric buckets and its
// 3-second flush ticks fall at the same phase in every run. The harness starts the agent
// container a fixed lead before an aligned moment, and the workload starts its step clock at the
// next aligned moment: Offset after a multiple of Every since the Unix epoch. The zero value
// aligns nothing.
type Align struct {
	Every  time.Duration `yaml:"every,omitempty"`
	Offset time.Duration `yaml:"offset,omitempty"`
}

// Next returns the first aligned moment at or after now, or now for the zero value.
func (a Align) Next(now time.Time) time.Time {
	if a.Every <= 0 {
		return now
	}
	wait := a.Offset - time.Duration(now.UnixNano()%int64(a.Every))
	if wait < 0 {
		wait += a.Every
	}
	return now.Add(wait)
}

// Metadata selects the mode of the fake Google Cloud metadata server: answer (the default), slow
// (answer after Delay) or fail (status 500 after Delay).
type Metadata struct {
	Mode  string        `yaml:"mode,omitempty"`
	Delay time.Duration `yaml:"delay,omitempty"`
}

// Stop describes how the harness stops the container.
type Stop struct {
	Signal       string
	AfterSeconds float64
}

// UnmarshalYAML reads the send_signal mapping, which must name a signal.
func (s *Stop) UnmarshalYAML(node *yaml.Node) error {
	if err := knownKeys(node, "stop", "send_signal"); err != nil {
		return err
	}
	for i := 0; i+1 < len(node.Content); i += 2 {
		if err := knownKeys(node.Content[i+1], "stop.send_signal", "signal", "after_seconds"); err != nil {
			return err
		}
	}
	var raw struct {
		SendSignal struct {
			Signal       string  `yaml:"signal"`
			AfterSeconds float64 `yaml:"after_seconds"`
		} `yaml:"send_signal"`
	}
	if err := node.Decode(&raw); err != nil {
		return err
	}
	if raw.SendSignal.Signal == "" {
		return fmt.Errorf("line %d: stop.send_signal.signal is required", node.Line)
	}
	*s = Stop{Signal: raw.SendSignal.Signal, AfterSeconds: raw.SendSignal.AfterSeconds}
	return nil
}

// knownKeys returns an error for a key of the mapping node that is not in allowed. A custom
// UnmarshalYAML decodes its node without the strict check of Load, so it checks its keys itself.
func knownKeys(node *yaml.Node, name string, allowed ...string) error {
	if node.Kind != yaml.MappingNode {
		return fmt.Errorf("line %d: %s must be a mapping", node.Line, name)
	}
	for i := 0; i < len(node.Content); i += 2 {
		if key := node.Content[i].Value; !slices.Contains(allowed, key) {
			return fmt.Errorf("line %d: %s has no field %q", node.Content[i].Line, name, key)
		}
	}
	return nil
}

// MarshalYAML writes the send_signal mapping that UnmarshalYAML reads.
func (s Stop) MarshalYAML() (any, error) {
	if s == (Stop{}) {
		return nil, nil
	}
	return map[string]map[string]any{"send_signal": {"signal": s.Signal, "after_seconds": s.AfterSeconds}}, nil
}

// Compare holds the per-scenario comparison settings.
type Compare struct {
	Overrides []Override `yaml:"overrides"`
}

// Override replaces the comparator for one path in the capture.
type Override struct {
	Path       string `yaml:"path"`
	Comparator string `yaml:"comparator"`
	Value      any    `yaml:"value"`
}

// Comparators are the comparators an override can name.
var Comparators = []string{"exact", "equals", "greater_than", "within_percent"}

// stepArguments maps each step kind to the arguments it reads. A step whose value is not a
// mapping stores it under "value". The then steps of wait_for_signal are not arguments.
var stepArguments = map[string][]string{
	"send_spans": {"fixture"}, "send_otlp": {"fixture"}, "send_dogstatsd": {"value"},
	"print": {"stream", "lines"}, "append_log": {"path", "lines"}, "http_get": {"value"},
	"sleep": {"value"}, "wait_for_signal": {"signal"}, "exit": {"value"},
}

// UnmarshalYAML reads a step: the optional "at" key and a single other key that is the kind.
func (s *Step) UnmarshalYAML(node *yaml.Node) error {
	if node.Kind != yaml.MappingNode {
		return fmt.Errorf("line %d: a step must be a mapping", node.Line)
	}
	*s = Step{}
	for i := 0; i < len(node.Content); i += 2 {
		key, value := node.Content[i].Value, node.Content[i+1]
		if key == "at" {
			at, err := time.ParseDuration(value.Value)
			if err != nil {
				return fmt.Errorf("line %d: invalid at: %w", value.Line, err)
			}
			s.At = at
			continue
		}
		if s.Kind != "" {
			return fmt.Errorf("line %d: a step has one kind, found %q and %q", node.Line, s.Kind, key)
		}
		s.Kind = key
		if err := s.decodeArgs(value); err != nil {
			return err
		}
	}
	return nil
}

// MarshalYAML writes the step in the form UnmarshalYAML reads, so the harness can copy a
// scenario into a container.
func (s Step) MarshalYAML() (any, error) {
	var value any = s.Args
	if v, ok := s.Args["value"]; ok && len(s.Args) == 1 {
		value = v
	}
	if len(s.Then) > 0 {
		args := maps.Clone(s.Args)
		if args == nil {
			args = map[string]any{}
		}
		args["then"] = s.Then
		value = args
	}
	step := map[string]any{s.Kind: value}
	if s.At != 0 {
		step["at"] = s.At.String()
	}
	return step, nil
}

func (s *Step) decodeArgs(value *yaml.Node) error {
	if value.Kind != yaml.MappingNode {
		var v any
		if err := value.Decode(&v); err != nil {
			return err
		}
		s.Args = map[string]any{"value": v}
		return nil
	}
	if err := value.Decode(&s.Args); err != nil {
		return err
	}
	if _, ok := s.Args["then"]; !ok {
		return nil
	}
	if s.Kind != "wait_for_signal" {
		return fmt.Errorf("line %d: then works only on wait_for_signal, not on %s", value.Line, s.Kind)
	}
	delete(s.Args, "then")
	for i := 0; i < len(value.Content); i += 2 {
		if value.Content[i].Value == "then" {
			return value.Content[i+1].Decode(&s.Then)
		}
	}
	return nil
}

// Load reads and checks one scenario file. A field the scenario format does not have is an error.
func Load(path string) (Scenario, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return Scenario{}, err
	}
	var s Scenario
	decoder := yaml.NewDecoder(bytes.NewReader(data))
	decoder.KnownFields(true)
	if err := decoder.Decode(&s); err != nil {
		return Scenario{}, fmt.Errorf("%s: %w", path, err)
	}
	if err := s.validate(strings.TrimSuffix(filepath.Base(path), ".yaml")); err != nil {
		return Scenario{}, fmt.Errorf("%s: %w", path, err)
	}
	return s, nil
}

func (s Scenario) validate(fileName string) error {
	if s.Name != fileName {
		return fmt.Errorf("name %q must equal the file name %q without .yaml", s.Name, fileName)
	}
	if !slices.Contains([]string{"P0", "P1", "P2"}, s.Priority) {
		return fmt.Errorf("priority %q must be P0, P1 or P2", s.Priority)
	}
	if !slices.Contains([]string{"in-container", "sidecar"}, s.Mode) {
		return fmt.Errorf("mode %q must be in-container or sidecar", s.Mode)
	}
	if s.Align != (Align{}) {
		if s.Align.Every <= 0 {
			return fmt.Errorf("align.every must be more than zero, found %s", s.Align.Every)
		}
		if s.Align.Offset < 0 || s.Align.Offset >= s.Align.Every {
			return fmt.Errorf("align.offset must be at least zero and less than align.every, found %s", s.Align.Offset)
		}
	}
	if err := s.Metadata.validate(); err != nil {
		return err
	}
	for _, o := range s.Compare.Overrides {
		if !slices.Contains(Comparators, o.Comparator) {
			return fmt.Errorf("override of %q has the unknown comparator %q, want one of %s", o.Path, o.Comparator, strings.Join(Comparators, ", "))
		}
	}
	return validateSteps(s.Workload)
}

func (m Metadata) validate() error {
	switch m.Mode {
	case "", "answer":
		if m.Delay != 0 {
			return fmt.Errorf("metadata.delay applies only to slow and fail, found %s with mode answer", m.Delay)
		}
	case "slow":
		if m.Delay <= 0 {
			return fmt.Errorf("metadata.delay must be more than zero in slow mode, found %s", m.Delay)
		}
	case "fail":
		if m.Delay < 0 {
			return fmt.Errorf("metadata.delay must not be negative, found %s", m.Delay)
		}
	default:
		return fmt.Errorf("metadata.mode must be answer, slow or fail, found %q", m.Mode)
	}
	return nil
}

func validateSteps(steps []Step) error {
	for _, step := range steps {
		if step.Kind == "" {
			return errors.New("a workload step has no kind")
		}
		allowed, known := stepArguments[step.Kind]
		if !known {
			return fmt.Errorf("unknown step kind %q", step.Kind)
		}
		for _, key := range slices.Sorted(maps.Keys(step.Args)) {
			if !slices.Contains(allowed, key) {
				return fmt.Errorf("%s has no argument %q, want %s", step.Kind, key, strings.Join(allowed, " or "))
			}
		}
		if err := validateValue(step); err != nil {
			return err
		}
		if err := validateSteps(step.Then); err != nil {
			return err
		}
	}
	return nil
}

// validateValue checks the value of a sleep or exit step.
func validateValue(step Step) error {
	value, text := step.Args["value"], fmt.Sprint(step.Args["value"])
	switch step.Kind {
	case "sleep":
		s, _ := value.(string)
		if d, err := time.ParseDuration(s); err != nil || d < 0 {
			return fmt.Errorf("sleep needs a duration that is not negative, found %q", text)
		}
	case "exit":
		if code, ok := value.(int); !ok || code < 0 || code > 255 {
			return fmt.Errorf("exit needs a whole number from 0 to 255, found %q", text)
		}
	}
	return nil
}

// LoadDir loads every .yaml file in dir, sorted by file name.
func LoadDir(dir string) ([]Scenario, error) {
	paths, err := filepath.Glob(filepath.Join(dir, "*.yaml"))
	if err != nil {
		return nil, err
	}
	slices.Sort(paths)
	scenarios := make([]Scenario, 0, len(paths))
	for _, path := range paths {
		s, err := Load(path)
		if err != nil {
			return nil, err
		}
		scenarios = append(scenarios, s)
	}
	return scenarios, nil
}

// ValidatePlatforms returns an error naming each scenario whose platform file
// <platformsDir>/<platform>.env does not exist.
func ValidatePlatforms(scenarios []Scenario, platformsDir string) error {
	var errs []error
	for _, s := range scenarios {
		file := filepath.Join(platformsDir, s.Platform+".env")
		if _, err := os.Stat(file); err != nil {
			errs = append(errs, fmt.Errorf("scenario %q: platform %q has no file %s", s.Name, s.Platform, file))
		}
	}
	return errors.Join(errs...)
}
