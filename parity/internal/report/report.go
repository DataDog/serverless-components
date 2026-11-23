// Package report writes the parity report of a run, report.md for people and report.json for
// scripts, and finds what varies between stability runs.
package report

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/DataDog/serverless-components/parity/internal/compare"
	"github.com/DataDog/serverless-components/parity/internal/diff"
	"github.com/DataDog/serverless-components/parity/internal/normalize"
	"github.com/DataDog/serverless-components/parity/internal/reference"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

// maxDiffLines is the most differences report.md shows for one scenario. diff.txt has them all.
const maxDiffLines = 40

// maxValueLength is the longest value a difference line shows, so that one missing trace chunk
// does not fill the report.
const maxValueLength = 200

// Run is what one binary's run of a scenario measured.
type Run struct {
	Measures  timing.Measures     `json:"measures"`
	Resources reference.Resources `json:"resources"`
	// Killed is true when the agent did not exit within the grace period and was killed.
	Killed bool `json:"killed,omitempty"`
}

// Result is the outcome of one scenario.
type Result struct {
	Scenario scenario.Scenario
	// Status is one of the compare statuses.
	Status string
	// Error says what broke when Status is harness-error.
	Error       string
	Differences []diff.Difference
	// UnmatchedRules are the normalizer rules that reached nothing in this scenario's capture.
	UnmatchedRules []normalize.Rule
	// Actual is nil when the run produced no capture. Reference is nil without a reference.
	Actual, Reference *Run
}

// Report is everything one run of the harness writes.
type Report struct {
	// Binary names the binary that was compared with Go.
	Binary string
	// Source names the Go commit that the references came from.
	Source  string
	Results []Result
}

// Bar is the parity bar: how many P0 scenarios pass, counting expected differences as passing,
// how many P0 scenarios count, and how many of those have a known coverage gap. Report-only
// scenarios don't count.
type Bar struct {
	Passing          int `json:"p0_passing"`
	Total            int `json:"p0_total"`
	WithCoverageGaps int `json:"p0_with_coverage_gaps"`
}

// ParityBar returns the parity bar of results.
func ParityBar(results []Result) Bar {
	var bar Bar
	for _, r := range results {
		if r.Scenario.Priority != "P0" || r.Scenario.ReportOnly {
			continue
		}
		bar.Total++
		if r.Status == compare.StatusPass || r.Status == compare.StatusExpectedOnly {
			bar.Passing++
		}
		if r.Scenario.CoverageGap != "" {
			bar.WithCoverageGaps++
		}
	}
	return bar
}

// String returns the parity bar line, for example "P0 scenarios passing: 20 of 20 (2 with known
// coverage gaps)".
func (b Bar) String() string {
	line := fmt.Sprintf("P0 scenarios passing: %d of %d", b.Passing, b.Total)
	if b.WithCoverageGaps > 0 {
		line += fmt.Sprintf(" (%d with known coverage gaps)", b.WithCoverageGaps)
	}
	return line
}

// unmatchedEverywhere returns the rules that reached nothing in every scenario that produced a
// capture. A rule unmatched in one scenario is normal, since most scenarios send only some areas.
func unmatchedEverywhere(results []Result) []normalize.Rule {
	var common []normalize.Rule
	first := true
	for _, r := range results {
		if r.Actual == nil {
			continue
		}
		if first {
			common, first = slices.Clone(r.UnmatchedRules), false
			continue
		}
		common = slices.DeleteFunc(common, func(rule normalize.Rule) bool { return !slices.Contains(r.UnmatchedRules, rule) })
	}
	return common
}

// Write writes report.md and report.json to dir, and the full differences of each scenario that
// has any to <dir>/<scenario>/diff.txt.
func Write(dir string, r Report) error {
	for _, result := range r.Results {
		if len(result.Differences) == 0 {
			continue
		}
		var lines strings.Builder
		for _, d := range result.Differences {
			lines.WriteString(differenceLine(d, false) + "\n")
		}
		scenarioDir := filepath.Join(dir, result.Scenario.Name)
		if err := os.MkdirAll(scenarioDir, 0o755); err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(scenarioDir, "diff.txt"), []byte(lines.String()), 0o644); err != nil {
			return err
		}
	}
	if err := os.WriteFile(filepath.Join(dir, "report.md"), []byte(markdown(r)), 0o644); err != nil {
		return err
	}
	data, err := json.MarshalIndent(jsonForm(r), "", "  ")
	if err != nil {
		return fmt.Errorf("encoding report.json: %w", err)
	}
	return os.WriteFile(filepath.Join(dir, "report.json"), append(data, '\n'), 0o644)
}

func markdown(r Report) string {
	var b strings.Builder
	line := func(format string, args ...any) { fmt.Fprintf(&b, format+"\n", args...) }
	line("# Parity report")
	line("")
	line("- Binary: %s", r.Binary)
	line("- Go references: %s", r.Source)
	line("")
	line("## Parity bar")
	line("")
	line("%s", ParityBar(r.Results))
	line("")
	p1 := p1Differences(r.Results)
	if len(p1) == 0 {
		line("P1 differences: none")
	} else {
		line("P1 differences:")
		line("")
		for _, d := range p1 {
			line("- %s", d)
		}
	}
	line("")
	line("## Summary")
	line("")
	line("| Scenario | Priority | Status | Known coverage gap |")
	line("|---|---|---|---|")
	for _, result := range r.Results {
		gap := ""
		if result.Scenario.CoverageGap != "" {
			gap = "yes"
		}
		line("| %s | %s | %s | %s |", result.Scenario.Name, result.Scenario.Priority, result.Status, gap)
	}
	if unmatched := unmatchedEverywhere(r.Results); len(unmatched) > 0 {
		line("")
		line("## Warnings")
		line("")
		line("These normalizer rules matched nothing in any scenario of this run:")
		line("")
		for _, rule := range unmatched {
			line("- %s %s", rule.Action, rule.Path)
		}
	}
	line("")
	line("## Scenarios")
	for _, result := range r.Results {
		line("")
		writeScenario(&b, result)
	}
	return b.String()
}

// p1Differences lists the differences of P1 scenarios that are not expected, and the P1
// scenarios that could not be compared.
func p1Differences(results []Result) []string {
	var out []string
	for _, r := range results {
		if r.Scenario.Priority != "P1" {
			continue
		}
		switch r.Status {
		case compare.StatusHarnessError, compare.StatusNoReference:
			out = append(out, fmt.Sprintf("%s: %s", r.Scenario.Name, r.Status))
			continue
		}
		for _, d := range r.Differences {
			if !d.Expected {
				out = append(out, fmt.Sprintf("%s: %s", r.Scenario.Name, differenceLine(d, true)))
			}
		}
	}
	return out
}

func writeScenario(b *strings.Builder, r Result) {
	line := func(format string, args ...any) { fmt.Fprintf(b, format+"\n", args...) }
	features := strings.Join(r.Scenario.Features, ", ")
	if features == "" {
		features = "none"
	}
	line("### %s", r.Scenario.Name)
	line("")
	line("- Status: %s", r.Status)
	line("- Priority: %s", r.Scenario.Priority)
	line("- Features: %s", features)
	if r.Scenario.CoverageGap != "" {
		line("- Known coverage gap: %s", r.Scenario.CoverageGap)
	}
	if r.Scenario.ReportOnly {
		line("- Report only: measured and reported, never pass or fail")
	}
	if r.Error != "" {
		line("- Error: %s", r.Error)
	}
	if r.Status == compare.StatusNoReference {
		line("")
		line("There is no Go reference for this scenario. Run parity record to record one.")
	}
	if len(r.Differences) > 0 {
		line("")
		line("Differences (%d):", len(r.Differences))
		line("")
		line("```")
		for _, d := range r.Differences[:min(len(r.Differences), maxDiffLines)] {
			line("%s", differenceLine(d, true))
		}
		line("```")
		if extra := len(r.Differences) - maxDiffLines; extra > 0 {
			line("")
			line("The other %d differences are in %s/diff.txt.", extra, r.Scenario.Name)
		}
	}
	if r.Actual == nil {
		return
	}
	goSide := func(value func(Run) string) string {
		if r.Reference == nil {
			return "no reference"
		}
		return value(*r.Reference)
	}
	line("")
	line("Timing:")
	line("")
	rows := timingRows(r.Actual, r.Reference)
	if len(rows) == 0 {
		line("No timing measure was measurable.")
	} else {
		line("| Measure | Go | This binary |")
		line("|---|---|---|")
		for _, row := range rows {
			line("| %s | %s | %s |", row.name, goSide(row.value), row.value(*r.Actual))
		}
	}
	line("")
	line("Startup and memory:")
	line("")
	line("| Measure | Go | This binary |")
	line("|---|---|---|")
	startup := func(run Run) string { return run.Resources.Startup.String() }
	memory := func(run Run) string { return memoryString(run.Resources) }
	line("| Startup time | %s | %s |", goSide(startup), startup(*r.Actual))
	line("| Peak memory | %s | %s |", goSide(memory), memory(*r.Actual))
	if r.Actual.Killed || (r.Reference != nil && r.Reference.Killed) {
		killed := func(run Run) string {
			if run.Killed {
				return "yes"
			}
			return "no"
		}
		line("| Killed with SIGKILL | %s | %s |", goSide(killed), killed(*r.Actual))
	}
}

type timingRow struct {
	name  string
	value func(Run) string
}

// timingRows returns the timing measures that either run measured.
func timingRows(actual, ref *Run) []timingRow {
	runs := []*Run{actual}
	if ref != nil {
		runs = append(runs, ref)
	}
	anyRun := func(check func(timing.Measures) bool) bool {
		return slices.ContainsFunc(runs, func(run *Run) bool { return check(run.Measures) })
	}
	var rows []timingRow
	measurement := func(name string, get func(timing.Measures) timing.Measurement) {
		if anyRun(func(m timing.Measures) bool { return get(m).Measurable }) {
			rows = append(rows, timingRow{name, func(run Run) string { return get(run.Measures).String() }})
		}
	}
	items := func(name string, get func(timing.Measures) []string) {
		if anyRun(func(m timing.Measures) bool { return len(get(m)) > 0 }) {
			rows = append(rows, timingRow{name, func(run Run) string {
				if list := get(run.Measures); len(list) > 0 {
					return strings.Join(list, ", ")
				}
				return "none"
			}})
		}
	}
	for _, area := range timing.Areas {
		measurement("Flush interval of "+area+" sent by the workload", func(m timing.Measures) timing.Measurement { return m.IntervalByArea[area] })
		measurement("Flush interval of all "+area, func(m timing.Measures) timing.Measurement { return m.FlushIntervalByArea[area] })
		measurement("Longest delivery delay of "+area, func(m timing.Measures) timing.Measurement { return m.MaxDelayByArea[area] })
	}
	measurement("Shutdown after SIGTERM", func(m timing.Measures) timing.Measurement { return m.Shutdown })
	items("Items sent before SIGTERM that never arrived", func(m timing.Measures) []string { return m.MissingAfterSigterm })
	items("Items sent after SIGTERM that never arrived", func(m timing.Measures) []string { return m.MissingSentAfterSigterm })
	items("Items whose delivery is ambiguous", func(m timing.Measures) []string { return m.AmbiguousItems })
	return rows
}

func memoryString(r reference.Resources) string {
	if !r.PeakMemoryKnown {
		return "unknown"
	}
	return fmt.Sprintf("%.1f MiB", float64(r.PeakMemoryBytes)/(1<<20))
}

// differenceLine formats a difference as its path with Go's value and this binary's value. When
// shorten is true, long values are cut to maxValueLength characters.
func differenceLine(d diff.Difference, shorten bool) string {
	text := fmt.Sprintf("%s: Go %s, this binary %s", d.Path, valueString(d.Reference, shorten), valueString(d.Actual, shorten))
	if d.Expected {
		text += " (expected: " + d.Justification + ")"
	}
	return text
}

func valueString(v any, shorten bool) string {
	if v == nil {
		return "absent"
	}
	data, err := json.Marshal(v)
	if err != nil {
		data = []byte(fmt.Sprint(v))
	}
	text := string(data)
	if shorten && len(text) > maxValueLength {
		text = text[:maxValueLength] + " (cut; the full value is in diff.txt)"
	}
	return text
}

type jsonRule struct {
	Action string `json:"action"`
	Path   string `json:"path"`
}

type jsonScenario struct {
	Name        string            `json:"name"`
	Priority    string            `json:"priority"`
	Features    []string          `json:"features"`
	Status      string            `json:"status"`
	CoverageGap string            `json:"coverage_gap,omitempty"`
	ReportOnly  bool              `json:"report_only,omitempty"`
	Error       string            `json:"error,omitempty"`
	Differences []diff.Difference `json:"differences"`
	Actual      *Run              `json:"actual,omitempty"`
	Reference   *Run              `json:"reference,omitempty"`
}

type jsonReport struct {
	Binary         string         `json:"binary"`
	Source         string         `json:"source"`
	ParityBar      Bar            `json:"parity_bar"`
	UnmatchedRules []jsonRule     `json:"unmatched_rules"`
	Scenarios      []jsonScenario `json:"scenarios"`
}

func jsonForm(r Report) jsonReport {
	out := jsonReport{Binary: r.Binary, Source: r.Source, UnmatchedRules: []jsonRule{}, Scenarios: []jsonScenario{}}
	out.ParityBar = ParityBar(r.Results)
	for _, rule := range unmatchedEverywhere(r.Results) {
		out.UnmatchedRules = append(out.UnmatchedRules, jsonRule{rule.Action, rule.Path})
	}
	for _, result := range r.Results {
		differences := result.Differences
		if differences == nil {
			differences = []diff.Difference{}
		}
		out.Scenarios = append(out.Scenarios, jsonScenario{
			Name: result.Scenario.Name, Priority: result.Scenario.Priority, Features: result.Scenario.Features,
			Status: result.Status, CoverageGap: result.Scenario.CoverageGap, ReportOnly: result.Scenario.ReportOnly,
			Error: result.Error, Differences: differences,
			Actual: result.Actual, Reference: result.Reference,
		})
	}
	return out
}

// Stability returns, sorted, the content paths and timing measures that vary between runs of one
// scenario against the same binary. Every pair of runs is compared, for timing in both directions,
// with the comparator overrides and the tolerances in tol. The shutdown limit is not a variation,
// so it is left out.
func Stability(runs []normalize.Normalized, overrides []scenario.Override, tol timing.Tolerances) []string {
	measures := make([]timing.Measures, len(runs))
	for i, run := range runs {
		measures[i] = timing.Measure(run.Timing, tol)
	}
	var paths []string
	for i := range runs {
		for j := i + 1; j < len(runs); j++ {
			for _, d := range compare.Content(runs[i], runs[j], overrides, tol) {
				paths = append(paths, d.Path)
			}
			for _, d := range append(timing.Compare(measures[i], measures[j], tol, 0), timing.Compare(measures[j], measures[i], tol, 0)...) {
				if d.Path != "timing.shutdown_limit" {
					paths = append(paths, d.Path)
				}
			}
		}
	}
	slices.Sort(paths)
	return slices.Compact(paths)
}
