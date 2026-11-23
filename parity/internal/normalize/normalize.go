// Package normalize turns a raw capture into a form that two runs can be compared in, following
// the rules in normalize.yaml, and extracts the inputs of the flush timing check.
package normalize

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"regexp"
	"slices"
	"strings"
	"time"

	"gopkg.in/yaml.v3"

	"github.com/DataDog/serverless-components/parity/internal/capture"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

// placeholder replaces values that legitimately differ by binary.
const placeholder = "<placeholder>"

// masked replaces run-specific values that the mask action hides.
const masked = "<masked>"

// childMarker starts every line that the workload driver prints.
const childMarker = "parity-child: "

// Rule changes the values that Path reaches. Actions:
//   - drop removes the values, so their presence is not compared either. Use it only where
//     presence cannot matter to a customer.
//   - mask replaces present values with a constant and keeps empty ones, so presence is still
//     compared. Use it for run-specific values such as hostnames and timestamps. With
//     WhenMatches set, it masks only the values that match it.
//   - relabel replaces each value with <label>-<n>, numbered by order of first appearance.
//     Rules with the same label share one numbering, so a parent ID gets the label of the span
//     ID it points to. Empty and zero values are kept.
//   - relative replaces a non-zero timestamp with a label that checks it against the run, as
//     Relative describes, and keeps zero, so presence is still compared. Use it for absolute
//     timestamps, which differ between runs.
//   - placeholder replaces the values with "<placeholder>".
//   - sort sorts the lists.
//
// Path is dotted and starts with an area of Normalized: series, sketches, logs, traces, stats,
// check_runs, process or undecoded. A segment can end in brackets: name[] reaches every element of a list,
// and name[key] reaches the map entry key, or in a list of key:value tags, the value of every
// tag with that key. name[field=value] and name[field!=value] reach the objects of a list whose
// field equals, or does not equal, the value. For example
// traces[].tracer_payloads[].chunks[].spans[].span_id, series[].tags[datadog_init_version], or
// series[metric!=datadog.agent.running].points[].timestamp.
type Rule struct {
	Action string `yaml:"action"`
	Path   string `yaml:"path"`
	// Label names the numbering of a relabel rule. It defaults to the last field name without
	// an _id suffix, so span_id is labeled span.
	Label string `yaml:"label"`
	// WhenMatches, on a mask rule, is a regular expression that the whole value must match for the
	// rule to mask it; other values stay as they are and are compared. Empty masks every value.
	WhenMatches string `yaml:"when_matches"`
	// Relative configures a relative rule, and only a relative rule.
	Relative Relative `yaml:"relative,omitempty"`
	Reason   string   `yaml:"reason"`
}

// valueMatcher compiles WhenMatches anchored to the whole value, or returns nil when it is empty.
func (r Rule) valueMatcher() (*regexp.Regexp, error) {
	if r.WhenMatches == "" {
		return nil, nil
	}
	return regexp.Compile("^(?:" + r.WhenMatches + ")$")
}

func (r Rule) label(segments []segment) string {
	if r.Label != "" {
		return r.Label
	}
	return strings.TrimSuffix(segments[len(segments)-1].name, "_id")
}

var actions = []string{"drop", "mask", "relative", "relabel", "placeholder", "sort"}

var areas = []string{"series", "sketches", "logs", "traces", "stats", "check_runs", "process", "undecoded"}

type rulesFile struct {
	Tolerances struct {
		Interval   *time.Duration `yaml:"interval"`
		Delay      *time.Duration `yaml:"delay"`
		Shutdown   *time.Duration `yaml:"shutdown"`
		Percentile *float64       `yaml:"percentile"`
		FlushGap   *time.Duration `yaml:"flush_gap"`
		Reason     string         `yaml:"reason"`
	} `yaml:"tolerances"`
	Rules []Rule `yaml:"rules"`
}

// LoadRules reads the rules and timing tolerances in the file at path. Tolerances the file does
// not set keep their defaults. Every rule, and the tolerances when the file sets any, needs a
// reason.
func LoadRules(path string) ([]Rule, timing.Tolerances, error) {
	tolerances := timing.DefaultTolerances()
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, tolerances, err
	}
	var file rulesFile
	decoder := yaml.NewDecoder(bytes.NewReader(data))
	decoder.KnownFields(true)
	if err := decoder.Decode(&file); err != nil {
		return nil, tolerances, fmt.Errorf("%s: %w", path, err)
	}
	for i, rule := range file.Rules {
		if err := validate(rule); err != nil {
			return nil, tolerances, fmt.Errorf("%s: rule %d: %w", path, i+1, err)
		}
	}
	t := file.Tolerances
	if (t.Interval != nil || t.Delay != nil || t.Shutdown != nil || t.Percentile != nil || t.FlushGap != nil) && t.Reason == "" {
		return nil, tolerances, fmt.Errorf("%s: the tolerances have no reason", path)
	}
	for _, override := range []struct {
		value  *time.Duration
		target *time.Duration
	}{{t.Interval, &tolerances.Interval}, {t.Delay, &tolerances.Delay}, {t.Shutdown, &tolerances.Shutdown}, {t.FlushGap, &tolerances.FlushGap}} {
		if override.value != nil {
			*override.target = *override.value
		}
	}
	if t.Percentile != nil {
		tolerances.Percentile = *t.Percentile
	}

	if t.FlushGap != nil && *t.FlushGap <= 0 {
		return nil, tolerances, fmt.Errorf("%s: tolerances.flush_gap is %s; it must be more than zero, or every payload is its own flush", path, *t.FlushGap)
	}
	return file.Rules, tolerances, nil
}

func validate(rule Rule) error {
	if rule.Reason == "" {
		return errors.New("the rule has no reason")
	}
	if !slices.Contains(actions, rule.Action) {
		return fmt.Errorf("unknown action %q, want one of %s", rule.Action, strings.Join(actions, ", "))
	}
	if rule.WhenMatches != "" && rule.Action != "mask" {
		return fmt.Errorf("when_matches works only on mask rules, not on %s", rule.Action)
	}
	if _, err := rule.valueMatcher(); err != nil {
		return fmt.Errorf("when_matches: %w", err)
	}
	if rule.Action == "relative" {
		if err := rule.Relative.validate(); err != nil {
			return err
		}
	} else if rule.Relative != (Relative{}) {
		return fmt.Errorf("relative settings work only on relative rules, not on %s", rule.Action)
	}
	segments, err := parsePath(rule.Path)
	if err != nil {
		return err
	}
	if !slices.Contains(areas, segments[0].name) {
		return fmt.Errorf("path %q starts with %q, want one of %s", rule.Path, segments[0].name, strings.Join(areas, ", "))
	}
	return nil
}

// Apply builds the normalized form of c and applies rules to everything except Timing. Every
// rule except relabel runs first, in file order. Then canonicalTraces orders the traces, and the
// relabel rules run, in file order. Relative rules check timestamps against the window from the
// container start to its exit in c.Outcome. Rules whose path reached nothing in c are listed in
// UnmatchedRules.
func Apply(c capture.Capture, rules []Rule) (Normalized, error) {
	for _, rule := range rules {
		if err := validate(rule); err != nil {
			return Normalized{}, fmt.Errorf("rule for %q: %w", rule.Path, err)
		}
	}
	n, err := view(c)
	if err != nil {
		return Normalized{}, err
	}
	matched := make([]bool, len(rules))
	isRelabel := func(r Rule) bool { return r.Action == "relabel" }
	window := runWindow{start: c.Outcome.StartedAt, exit: c.Outcome.ExitedAt}
	if n, err = applyRules(n, rules, func(r Rule) bool { return !isRelabel(r) }, matched, window); err != nil {
		return Normalized{}, err
	}
	canonicalTraces(n.Traces)
	if n, err = applyRules(n, rules, isRelabel, matched, window); err != nil {
		return Normalized{}, err
	}
	for i, rule := range rules {
		if !matched[i] {
			n.UnmatchedRules = append(n.UnmatchedRules, rule)
		}
	}
	if n.Timing, err = timingInput(c); err != nil {
		return Normalized{}, err
	}
	return n, nil
}

// applyRules applies the selected rules to the JSON form of n, and marks in matched the rules
// whose path reached a value.
func applyRules(n Normalized, rules []Rule, selected func(Rule) bool, matched []bool, window runWindow) (Normalized, error) {
	data, err := json.Marshal(n)
	if err != nil {
		return Normalized{}, fmt.Errorf("encoding the normalized capture: %w", err)
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	var tree map[string]any
	if err := decoder.Decode(&tree); err != nil {
		return Normalized{}, fmt.Errorf("decoding the normalized capture: %w", err)
	}
	labels := labeler{}
	for i, rule := range rules {
		if !selected(rule) {
			continue
		}
		segments, _ := parsePath(rule.Path)
		reached, err := applyRule(tree, rule, segments, labels, window)
		if err != nil {
			return Normalized{}, fmt.Errorf("%s rule for %q: %w", rule.Action, rule.Path, err)
		}
		matched[i] = matched[i] || reached
	}
	if data, err = json.Marshal(tree); err != nil {
		return Normalized{}, fmt.Errorf("encoding the normalized capture: %w", err)
	}
	var out Normalized
	if err := json.Unmarshal(data, &out); err != nil {
		return Normalized{}, fmt.Errorf("the rules changed a value to the wrong type: %w", err)
	}
	return out, nil
}
