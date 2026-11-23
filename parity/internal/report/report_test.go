package report

import (
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/compare"
	"github.com/DataDog/serverless-components/parity/internal/diff"
	"github.com/DataDog/serverless-components/parity/internal/normalize"
	"github.com/DataDog/serverless-components/parity/internal/reference"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

var update = flag.Bool("update", false, "rewrite the golden files")

func seconds(s float64) timing.Measurement {
	return timing.Measurement{Measurable: true, Value: timing.Duration(s * float64(time.Second))}
}

func measures(seriesInterval float64) timing.Measures {
	return timing.Measures{
		IntervalByArea: map[string]timing.Measurement{"series": seconds(seriesInterval)},
		MaxDelayByArea: map[string]timing.Measurement{"series": seconds(1.2)},
		Sigterm:        true,
		Shutdown:       seconds(0.4),
	}
}

func goldenReport() Report {
	maskHost := normalize.Rule{Action: "mask", Path: "series[].host", Reason: "Host name of the run."}
	maskSpan := normalize.Rule{Action: "mask", Path: "traces[].tracer_payloads[].chunks[].spans[].start", Reason: "Absolute timestamp of the run."}
	return Report{
		Binary: "rust (/parity/out/bin/rust/datadog-init)",
		Source: "datadog-agent commit 0123456789abcdef0123456789abcdef01234567, serverless-init release 1.7.8",
		Results: []Result{
			{
				Scenario:       scenario.Scenario{Name: "metrics.flush-cadence", Priority: "P0", Features: []string{"metrics.flush-cadence"}},
				Status:         compare.StatusFail,
				Differences:    []diff.Difference{{Area: "timing", Path: "timing.interval.series", Reference: "3s", Actual: "10s"}},
				UnmatchedRules: []normalize.Rule{maskSpan},
				Actual:         &Run{Measures: measures(10), Resources: reference.Resources{Startup: seconds(0.3), PeakMemoryBytes: 3 << 20, PeakMemoryKnown: true}, Killed: true},
				Reference:      &Run{Measures: measures(3), Resources: reference.Resources{Startup: seconds(0.9), PeakMemoryBytes: 60 << 20, PeakMemoryKnown: true}},
			},
			{
				Scenario:       scenario.Scenario{Name: "process.exit-codes", Priority: "P0", Features: []string{"process.exit-codes"}},
				Status:         compare.StatusPass,
				UnmatchedRules: []normalize.Rule{maskHost, maskSpan},
				Actual:         &Run{Measures: timing.Measures{}, Resources: reference.Resources{}},
				Reference:      &Run{Measures: timing.Measures{}, Resources: reference.Resources{Startup: seconds(0.8), PeakMemoryBytes: 50 << 20, PeakMemoryKnown: true}},
			},
			{
				Scenario:  scenario.Scenario{Name: "traces.info-endpoint", Priority: "P0", Features: []string{"traces.info-endpoint"}, CoverageGap: "The /info body is not compared."},
				Status:    compare.StatusPass,
				Actual:    &Run{Measures: timing.Measures{}},
				Reference: &Run{Measures: timing.Measures{}},
			},
			{
				Scenario:    scenario.Scenario{Name: "performance.startup-and-memory", Priority: "P2", ReportOnly: true},
				Status:      compare.StatusReported,
				Differences: []diff.Difference{{Area: "sketches", Path: "sketches[metric=m,tags=].count", Reference: 1, Actual: 2}},
				Actual:      &Run{Measures: timing.Measures{}, Resources: reference.Resources{Startup: seconds(0.2)}},
				Reference:   &Run{Measures: timing.Measures{}, Resources: reference.Resources{Startup: seconds(0.1)}},
			},
			{
				Scenario: scenario.Scenario{Name: "process.signal-forwarding", Priority: "P0", Features: []string{"process.signal-forwarding"}},
				Status:   compare.StatusHarnessError,
				Error:    "starting parity-process-signal-forwarding-rust: docker compose up failed",
			},
			{
				Scenario: scenario.Scenario{Name: "traces.sampling", Priority: "P1", Features: []string{"traces.sampling", "traces.stats"}},
				Status:   compare.StatusFail,
				Differences: []diff.Difference{
					{Area: "traces", Path: "traces[priority=0,service=parity-app,name=web.request,resource=/]", Actual: map[string]any{"spans": 1}, Expected: true, Justification: "implementation.local-priority-0-drop"},
					{Area: "stats", Path: "stats[service=parity-app,name=web.request,resource=/].hits", Reference: 2, Actual: 1},
				},
				UnmatchedRules: []normalize.Rule{maskSpan},
				Actual:         &Run{Measures: timing.Measures{}},
				Reference:      &Run{Measures: timing.Measures{}},
			},
		},
	}
}

func TestReportMatchesGolden(t *testing.T) {
	dir := t.TempDir()
	if err := Write(dir, goldenReport()); err != nil {
		t.Fatal(err)
	}
	got, err := os.ReadFile(filepath.Join(dir, "report.md"))
	if err != nil {
		t.Fatal(err)
	}
	golden := filepath.Join("testdata", "report.golden.md")
	if *update {
		if err := os.WriteFile(golden, got, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	want, err := os.ReadFile(golden)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != string(want) {
		t.Errorf("report.md differs from %s; run go test ./internal/report -update and review the change.\ngot:\n%s", golden, got)
	}
	for _, file := range []string{"report.json", "traces.sampling/diff.txt", "metrics.flush-cadence/diff.txt"} {
		if _, err := os.Stat(filepath.Join(dir, file)); err != nil {
			t.Errorf("Write did not write %s: %v", file, err)
		}
	}
}

func TestReportTruncatesDiffAtFortyLines(t *testing.T) {
	var differences []diff.Difference
	for i := range 45 {
		differences = append(differences, diff.Difference{Area: "logs", Path: fmt.Sprintf("logs[ddsource=parity,index=%d].message", i), Reference: "a", Actual: "b"})
	}
	dir := t.TempDir()
	result := Result{Scenario: scenario.Scenario{Name: "logs.stdout", Priority: "P0"}, Status: compare.StatusFail, Differences: differences}
	if err := Write(dir, Report{Binary: "rust", Source: "not recorded", Results: []Result{result}}); err != nil {
		t.Fatal(err)
	}
	report, err := os.ReadFile(filepath.Join(dir, "report.md"))
	if err != nil {
		t.Fatal(err)
	}
	shown := strings.Count(string(report), "logs[ddsource=parity,index=")
	if shown != 40 {
		t.Errorf("report.md shows %d differences, want 40", shown)
	}
	if !strings.Contains(string(report), "The other 5 differences are in logs.stdout/diff.txt.") {
		t.Errorf("report.md does not say where the other 5 differences are:\n%s", report)
	}
	full, err := os.ReadFile(filepath.Join(dir, "logs.stdout", "diff.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if lines := strings.Count(string(full), "\n"); lines != 45 {
		t.Errorf("diff.txt has %d lines, want 45", lines)
	}
}

func TestParityBarCountsOnlyP0(t *testing.T) {
	result := func(priority, status string) Result {
		return Result{Scenario: scenario.Scenario{Priority: priority}, Status: status}
	}
	withGap := result("P0", compare.StatusPass)
	withGap.Scenario.CoverageGap = "no step posts a profile"
	failingWithGap := result("P0", compare.StatusFail)
	failingWithGap.Scenario.CoverageGap = "no step posts a profile"
	reportOnly := result("P0", compare.StatusReported)
	reportOnly.Scenario.ReportOnly = true
	results := []Result{
		withGap, failingWithGap, reportOnly,
		result("P0", compare.StatusPass),
		result("P0", compare.StatusExpectedOnly),
		result("P0", compare.StatusFail),
		result("P0", compare.StatusNoReference),
		result("P0", compare.StatusHarnessError),
		result("P1", compare.StatusPass),
		result("P1", compare.StatusFail),
		result("P2", compare.StatusPass),
	}
	got := ParityBar(results)
	if want := (Bar{Passing: 3, Total: 7, WithCoverageGaps: 2}); got != want {
		t.Errorf("ParityBar = %+v, want %+v", got, want)
	}
	if got, want := got.String(), "P0 scenarios passing: 3 of 7 (2 with known coverage gaps)"; got != want {
		t.Errorf("String() = %q, want %q", got, want)
	}
	if got, want := (Bar{Passing: 1, Total: 1}).String(), "P0 scenarios passing: 1 of 1"; got != want {
		t.Errorf("String() = %q, want %q", got, want)
	}
}

func TestStabilityListsVaryingPaths(t *testing.T) {
	sigterm := time.Date(2026, 11, 22, 10, 0, 7, 0, time.UTC)
	run := func(message string, exitCode int, shutdown time.Duration) normalize.Normalized {
		return normalize.Normalized{
			Logs:    []normalize.Log{{Message: message, Source: "parity"}, {Message: "same", Source: "parity"}},
			Process: normalize.ProcessView{ExitCode: exitCode},
			Timing:  timing.Input{Sigterm: sigterm, Exited: sigterm.Add(shutdown)},
		}
	}
	tests := []struct {
		name string
		runs []normalize.Normalized
		want []string
	}{
		{name: "stable", runs: []normalize.Normalized{run("a", 3, time.Second), run("a", 3, time.Second), run("a", 3, time.Second)}, want: nil},
		{name: "shutdown within the tolerance", runs: []normalize.Normalized{run("a", 3, time.Second), run("a", 3, 1500*time.Millisecond)}, want: nil},
		{
			name: "each run within the tolerance of the first, two runs apart by more",
			runs: []normalize.Normalized{run("a", 3, time.Second), run("a", 3, 1900*time.Millisecond), run("a", 3, 100*time.Millisecond)},
			want: []string{"timing.shutdown"},
		},
		{name: "shutdown shorter by more than the tolerance", runs: []normalize.Normalized{run("a", 3, 4*time.Second), run("a", 3, time.Second)}, want: []string{"timing.shutdown"}},
		{
			name: "two fields vary in different runs",
			runs: []normalize.Normalized{run("a", 3, time.Second), run("b", 3, time.Second), run("a", 1, time.Second)},
			want: []string{"logs[ddsource=parity,index=0].message", "process.exit_code"},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			got := Stability(test.runs, nil, timing.DefaultTolerances())
			if !slices.Equal(got, test.want) {
				t.Errorf("Stability = %q, want %q", got, test.want)
			}
		})
	}
}
