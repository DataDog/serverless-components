package main

import (
	"os"
	"path/filepath"
	"slices"
	"testing"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/normalize"
	"github.com/DataDog/serverless-components/parity/internal/reference"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

func TestDecide(t *testing.T) {
	expected := filepath.Join(t.TempDir(), "expected-differences.yaml")
	allowExitCode := `differences:
  - scenario: process.exit-codes
    area: process
    path: process.exit_code
    allowed: any
    justification: implementation.exit-code-test
    reason: Test entry.
`
	if err := os.WriteFile(expected, []byte(allowExitCode), 0o644); err != nil {
		t.Fatal(err)
	}
	exitCode := func(code int) normalize.Normalized {
		return normalize.Normalized{Process: normalize.ProcessView{ExitCode: code}}
	}
	undecoded := normalize.Normalized{Undecoded: []normalize.Undecoded{{Route: "/api/v1/check_run", Error: "no decoder"}}}
	shutdown := func(d time.Duration) timing.Measures {
		return timing.Measures{Sigterm: true, Shutdown: timing.Measurement{Measurable: true, Value: timing.Duration(d)}}
	}
	// shutdownAfter gives n a timing input whose shutdown takes d.
	shutdownAfter := func(n normalize.Normalized, d time.Duration) normalize.Normalized {
		n.Timing = timing.Input{Sigterm: time.Unix(100, 0), Exited: time.Unix(100, 0).Add(d)}
		return n
	}
	tests := []struct {
		name       string
		reportOnly bool
		reference  *reference.Reference
		observed   observation
		wantStatus string
		wantPaths  []string
	}{
		{
			name:       "no reference",
			observed:   observation{normalized: exitCode(3)},
			wantStatus: "no-reference",
		},
		{
			name:       "a timing difference alone fails",
			reference:  &reference.Reference{Normalized: shutdownAfter(exitCode(3), time.Second)},
			observed:   observation{normalized: exitCode(3), measures: shutdown(5 * time.Second)},
			wantStatus: "fail",
			wantPaths:  []string{"timing.shutdown"},
		},
		{
			name: "the reference is measured again from its timing input, not from its saved measures",
			reference: &reference.Reference{
				Normalized: normalize.Normalized{Process: normalize.ProcessView{ExitCode: 3}, Timing: timing.Input{Sigterm: time.Unix(100, 0), Exited: time.Unix(101, 0)}},
			},
			observed:   observation{normalized: exitCode(3)},
			wantStatus: "fail",
			wantPaths:  []string{"timing.shutdown"},
		},
		{
			name:       "an undecoded payload fails even when both runs have it",
			reference:  &reference.Reference{Normalized: undecoded},
			observed:   observation{normalized: undecoded},
			wantStatus: "fail",
			wantPaths:  []string{"undecoded[route=/api/v1/check_run,error=no decoder]"},
		},
		{
			name:       "only expected differences",
			reference:  &reference.Reference{Normalized: exitCode(3)},
			observed:   observation{normalized: exitCode(1)},
			wantStatus: "expected-only",
			wantPaths:  []string{"process.exit_code"},
		},
		{
			name:       "a report-only scenario is reported, with its differences",
			reportOnly: true,
			reference:  &reference.Reference{Normalized: shutdownAfter(exitCode(3), time.Second)},
			observed:   observation{normalized: exitCode(3), measures: shutdown(5 * time.Second)},
			wantStatus: "reported",
			wantPaths:  []string{"timing.shutdown"},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			s := scenario.Scenario{Name: "process.exit-codes", Priority: "P0", ReportOnly: test.reportOnly}
			got := decide(test.reference, test.observed, s, nil, timing.DefaultTolerances(), expected)
			var paths []string
			for _, d := range got.Differences {
				paths = append(paths, d.Path)
			}
			if got.Status != test.wantStatus || !slices.Equal(paths, test.wantPaths) {
				t.Errorf("decide = %s with %q (error %q), want %s with %q", got.Status, paths, got.Error, test.wantStatus, test.wantPaths)
			}
		})
	}
}

// Scenarios whose timing is compared run alone, so that other scenarios do not make it noisy.
func TestRunsAlone(t *testing.T) {
	flushes := func(starts ...int) timing.Input {
		in := timing.Input{Sent: []timing.Sent{{At: time.Unix(0, 0), Items: []string{"metric:a"}}}, Exited: time.Unix(100, 0)}
		for _, start := range starts {
			in.Arrivals = append(in.Arrivals, timing.Arrival{Area: "series", At: time.Unix(int64(start), 0), Items: []string{"metric:a"}})
		}
		return in
	}
	withTiming := func(in timing.Input) *reference.Reference {
		return &reference.Reference{Normalized: normalize.Normalized{Timing: in}}
	}
	for _, tc := range []struct {
		name      string
		scenario  scenario.Scenario
		reference *reference.Reference
		want      bool
	}{
		{"no reference and no timing settings", scenario.Scenario{}, nil, false},
		{"align", scenario.Scenario{Align: scenario.Align{Every: 10 * time.Second}}, nil, true},
		{"a stop signal", scenario.Scenario{Stop: scenario.Stop{Signal: "SIGTERM", AfterSeconds: 3}}, nil, true},
		{"a reference with a measurable interval", scenario.Scenario{}, withTiming(flushes(3, 6, 9)), true},
		{"a reference with two flushes", scenario.Scenario{}, withTiming(flushes(3, 6)), false},
		{"a reference with a measurable shutdown", scenario.Scenario{}, withTiming(timing.Input{Sigterm: time.Unix(10, 0), Exited: time.Unix(11, 0)}), true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := runsAlone(tc.scenario, tc.reference, timing.DefaultTolerances()); got != tc.want {
				t.Fatalf("got %v, want %v", got, tc.want)
			}
		})
	}
}
