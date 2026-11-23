package scenario

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"

	"gopkg.in/yaml.v3"
)

func TestLoadExampleScenario(t *testing.T) {
	s, err := Load("testdata/process.signal-forwarding.yaml")
	if err != nil {
		t.Fatal(err)
	}
	if s.Name != "process.signal-forwarding" || s.Priority != "P0" || s.Mode != "in-container" || s.Platform != "local" {
		t.Errorf("unexpected header: %+v", s)
	}
	if len(s.Workload) != 5 {
		t.Fatalf("got %d steps, want 5", len(s.Workload))
	}
	if s.Workload[3].At != 5*time.Second {
		t.Errorf("fourth step at = %v, want 5s", s.Workload[3].At)
	}
	if s.Workload[0].Kind != "send_spans" || s.Workload[0].Args["fixture"] != "two-traces-v04" {
		t.Errorf("unexpected first step: %+v", s.Workload[0])
	}
	wait := s.Workload[4]
	if wait.Kind != "wait_for_signal" || wait.At != 0 || wait.Args["signal"] != "SIGTERM" {
		t.Errorf("unexpected wait step: %+v", wait)
	}
	if len(wait.Then) != 2 || wait.Then[0].Kind != "print" || wait.Then[1].Kind != "exit" {
		t.Errorf("unexpected nested steps: %+v", wait.Then)
	}
	if s.Stop.Signal != "SIGTERM" || s.Stop.AfterSeconds != 7 {
		t.Errorf("unexpected stop: %+v", s.Stop)
	}
}

func TestLoadRejects(t *testing.T) {
	valid := func(priority, mode, step string) string {
		return "name: case\npriority: " + priority + "\nmode: " + mode + "\nplatform: local\nworkload:\n  - " + step + "\n"
	}
	tests := []struct {
		name     string
		file     string
		contents string
		wantErr  string
	}{
		{"missing priority", "case.yaml", "name: case\nmode: sidecar\nplatform: local\n", "must be P0, P1 or P2"},
		{"unknown step kind", "case.yaml", valid("P0", "sidecar", "explode: {}"), `unknown step kind "explode"`},
		{"name not matching file", "other.yaml", valid("P0", "sidecar", "sleep: 1s"), "must equal the file name"},
		{"unknown mode", "case.yaml", valid("P1", "daemon", "sleep: 1s"), "must be in-container or sidecar"},
		{"step without kind", "case.yaml", valid("P1", "sidecar", "at: 1s"), "has no kind"},
		{"valid", "case.yaml", valid("P2", "sidecar", "sleep: 1s"), ""},
		{"align without every", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "align: {offset: 500ms}\n", "align.every must be more than zero"},
		{"align offset not below every", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "align: {every: 10s, offset: 10s}\n", "align.offset must be at least zero and less than align.every"},
		{"unknown metadata mode", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "metadata: {mode: hang}\n", "metadata.mode must be answer, slow or fail"},
		{"slow metadata without delay", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "metadata: {mode: slow}\n", "metadata.delay must be more than zero"},
		{"answer metadata with delay", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "metadata: {mode: answer, delay: 1s}\n", "metadata.delay applies only to slow and fail"},
		{"valid slow metadata", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "metadata: {mode: slow, delay: 2s}\n", ""},
		{"valid align", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "align: {every: 10s, offset: 500ms}\n", ""},
		{"unknown field", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "stopp: {send_signal: {signal: SIGTERM}}\n", "field stopp not found"},
		{"unknown align field", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "align: {every: 10s, ofset: 1s}\n", "field ofset not found"},
		{"unknown step argument", "case.yaml", valid("P0", "sidecar", "print: {stream: stdout, line: [x]}"), `print has no argument "line"`},
		{"mapping for a value step", "case.yaml", valid("P0", "sidecar", "send_dogstatsd: {lines: [\"a:1|c\"]}"), `send_dogstatsd has no argument "lines"`},
		{"then on another step", "case.yaml", valid("P0", "sidecar", "print: {lines: [x], then: [{exit: 0}]}"), "then works only on wait_for_signal"},
		{"unknown step inside then", "case.yaml", valid("P0", "sidecar", "wait_for_signal: {signal: SIGTERM, then: [{explode: 1}]}"), `unknown step kind "explode"`},
		{"invalid sleep", "case.yaml", valid("P0", "sidecar", "sleep: soon"), `sleep needs a duration that is not negative, found "soon"`},
		{"negative sleep", "case.yaml", valid("P0", "sidecar", "sleep: -1s"), `sleep needs a duration that is not negative, found "-1s"`},
		{"exit that is not a number", "case.yaml", valid("P0", "sidecar", "exit: three"), `exit needs a whole number from 0 to 255, found "three"`},
		{"exit out of range", "case.yaml", valid("P0", "sidecar", "exit: 256"), `exit needs a whole number from 0 to 255, found "256"`},
		{"stop without a signal", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "stop: {send_signal: {after_seconds: 3}}\n", "stop.send_signal.signal is required"},
		{"unknown stop field", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "stop: {send_signal: {signal: SIGTERM, after: 3}}\n", `stop.send_signal has no field "after"`},
		{"unknown comparator", "case.yaml", valid("P0", "sidecar", "sleep: 1s") + "compare: {overrides: [{path: process.exit_code, comparator: fuzzy}]}\n", `unknown comparator "fuzzy"`},
		{"valid wait_for_signal and stop", "case.yaml", valid("P0", "sidecar", "wait_for_signal: {signal: SIGTERM, then: [{exit: 7}]}") + "stop: {send_signal: {signal: SIGTERM, after_seconds: 3}}\n", ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), tt.file)
			if err := os.WriteFile(path, []byte(tt.contents), 0o600); err != nil {
				t.Fatal(err)
			}
			_, err := Load(path)
			if tt.wantErr == "" {
				if err != nil {
					t.Fatalf("unexpected error: %v", err)
				}
				return
			}
			if err == nil || !strings.Contains(err.Error(), tt.wantErr) {
				t.Fatalf("error = %v, want it to contain %q", err, tt.wantErr)
			}
		})
	}
}

func TestLoadDirSortsByFileName(t *testing.T) {
	dir := t.TempDir()
	for _, name := range []string{"b", "a"} {
		contents := "name: " + name + "\npriority: P1\nmode: sidecar\nplatform: local\n"
		if err := os.WriteFile(filepath.Join(dir, name+".yaml"), []byte(contents), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	got, err := LoadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 2 || got[0].Name != "a" || got[1].Name != "b" {
		t.Errorf("got %+v, want a then b", got)
	}
}

func TestValidatePlatforms(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "local.env"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	tests := []struct {
		name      string
		scenarios []Scenario
		wantErr   string
	}{
		{"all present", []Scenario{{Name: "a", Platform: "local"}}, ""},
		{"missing platform", []Scenario{{Name: "a", Platform: "local"}, {Name: "b", Platform: "cloud-run"}}, `scenario "b"`},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			err := ValidatePlatforms(tt.scenarios, dir)
			if tt.wantErr == "" {
				if err != nil {
					t.Fatalf("unexpected error: %v", err)
				}
				return
			}
			if err == nil || !strings.Contains(err.Error(), tt.wantErr) || !strings.Contains(err.Error(), "cloud-run") {
				t.Fatalf("error = %v, want it to contain %s and cloud-run", err, tt.wantErr)
			}
			if strings.Contains(err.Error(), `scenario "a"`) {
				t.Errorf("error names scenario a: %v", err)
			}
		})
	}
}

func TestMarshalRoundTrip(t *testing.T) {
	want, err := Load("testdata/process.signal-forwarding.yaml")
	if err != nil {
		t.Fatal(err)
	}
	data, err := yaml.Marshal(want)
	if err != nil {
		t.Fatal(err)
	}
	// The workload driver reads the copy with Load, which is strict.
	path := filepath.Join(t.TempDir(), want.Name+".yaml")
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}
	got, err := Load(path)
	if err != nil {
		t.Fatalf("%v\n%s", err, data)
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("round trip changed the scenario:\n%s", data)
	}
}

// defaultSpecPath is the serverless-init spec, whose test plan names every parity scenario.
// PARITY_SPEC_PATH replaces it.
const defaultSpecPath = "/Users/duncan.harvey/dd/notes/unified-agent-rust/spec-serverless-init.md"

// TestLoadDirParityScenarios loads parity/scenarios/ and checks that the spec's test plan names
// every scenario.
func TestLoadDirParityScenarios(t *testing.T) {
	specPath := defaultSpecPath
	if path := os.Getenv("PARITY_SPEC_PATH"); path != "" {
		specPath = path
	}
	spec, err := os.ReadFile(specPath)
	if err != nil {
		t.Logf("WARNING: THE SCENARIO NAMES WERE NOT CHECKED AGAINST THE SPEC. The serverless-init spec is not at %s (%v); set PARITY_SPEC_PATH to its path.", specPath, err)
		t.Skip("the serverless-init spec is missing")
	}
	planned := testPlanScenarios(string(spec))
	if len(planned) == 0 {
		t.Fatalf("found no scenarios in the test plan of %s", specPath)
	}
	scenarios, err := LoadDir("../../scenarios")
	if err != nil {
		t.Fatal(err)
	}
	if len(scenarios) == 0 {
		t.Fatal("parity/scenarios/ has no scenarios")
	}
	for _, s := range scenarios {
		if !planned[s.Name] {
			t.Errorf("scenario %s is not in the test plan of the spec", s.Name)
		}
	}
}

// testPlanScenarios returns the names in backticks in the first column of the parity table in
// section 5 of the spec.
func testPlanScenarios(spec string) map[string]bool {
	_, plan, _ := strings.Cut(spec, "\n## 5. Test plan")
	plan, _, _ = strings.Cut(plan, "\n## 6.")
	names := map[string]bool{}
	for _, line := range strings.Split(plan, "\n") {
		cells := strings.Split(line, "|")
		if len(cells) < 3 {
			continue
		}
		parts := strings.Split(cells[1], "`")
		for i := 1; i < len(parts); i += 2 {
			names[parts[i]] = true
		}
	}
	return names
}

// The harness copies the scenario into the container with MarshalYAML, so align must survive it.
func TestMarshalKeepsAlign(t *testing.T) {
	want := Scenario{Name: "a", Priority: "P0", Mode: "sidecar", Platform: "local", Align: Align{Every: 10 * time.Second, Offset: 500 * time.Millisecond}}
	data, err := yaml.Marshal(want)
	if err != nil {
		t.Fatal(err)
	}
	var got Scenario
	if err := yaml.Unmarshal(data, &got); err != nil || got.Align != want.Align {
		t.Fatalf("got %+v, %v from:\n%s", got.Align, err, data)
	}
}

func TestAlignNext(t *testing.T) {
	epoch := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	align := Align{Every: 10 * time.Second, Offset: 500 * time.Millisecond}
	for _, tc := range []struct {
		name      string
		now, want time.Duration
	}{
		{"before the offset", 200 * time.Millisecond, 500 * time.Millisecond},
		{"after the offset", 3200 * time.Millisecond, 10500 * time.Millisecond},
		{"at the offset", 500 * time.Millisecond, 500 * time.Millisecond},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := align.Next(epoch.Add(tc.now)); !got.Equal(epoch.Add(tc.want)) {
				t.Fatalf("got %v, want %v", got, epoch.Add(tc.want))
			}
		})
	}
	if now := epoch.Add(3 * time.Second); !(Align{}).Next(now).Equal(now) {
		t.Fatal("the zero Align must not wait")
	}
}
