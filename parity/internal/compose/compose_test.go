package compose

import (
	"flag"
	"os"
	"path/filepath"
	"reflect"
	"testing"
	"time"

	"gopkg.in/yaml.v3"

	"github.com/DataDog/serverless-components/parity/internal/scenario"
)

var update = flag.Bool("update", false, "rewrite the golden files")

func testScenario(mode string) scenario.Scenario {
	return scenario.Scenario{
		Name:        "process.signal-forwarding",
		Priority:    "P0",
		Mode:        mode,
		Platform:    "local",
		Environment: map[string]string{"DD_SERVICE": "parity-app"},
	}
}

func checkGolden(t *testing.T, name string, got []byte) {
	t.Helper()
	path := filepath.Join("testdata", name)
	if *update {
		if err := os.WriteFile(path, got, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	want, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != string(want) {
		t.Errorf("%s differs from the generated file:\n%s", path, got)
	}
}

func TestGenerateInContainer(t *testing.T) {
	got, err := Generate(testScenario("in-container"), "parity-process-signal-forwarding-go", "/repo/parity/out/bin/go/datadog-init",
		"/repo/parity/out/bin/workload/parity-workload", "/repo/parity/out/run/process.signal-forwarding/go",
		map[string]string{"K_SERVICE": "parity-service"})
	if err != nil {
		t.Fatal(err)
	}
	checkGolden(t, "in-container.golden.yaml", got)
}

func TestGenerateSidecar(t *testing.T) {
	got, err := Generate(testScenario("sidecar"), "parity-process-signal-forwarding-rust", "/repo/parity/out/bin/rust/datadog-init",
		"/repo/parity/out/bin/workload/parity-workload", "/repo/parity/out/run/process.signal-forwarding/rust",
		map[string]string{})
	if err != nil {
		t.Fatal(err)
	}
	checkGolden(t, "sidecar.golden.yaml", got)
}

func TestGenerateAddsMetadataServiceForGoogleCloud(t *testing.T) {
	s := testScenario("in-container")
	s.Platform = "cloud-run-service"
	s.Metadata = scenario.Metadata{Mode: "slow", Delay: 500 * time.Millisecond}
	got, err := Generate(s, "parity-process-signal-forwarding-go", "/repo/parity/out/bin/go/datadog-init",
		"/repo/parity/out/bin/workload/parity-workload", "/repo/parity/out/run/process.signal-forwarding/go",
		map[string]string{"K_SERVICE": "parity-service"})
	if err != nil {
		t.Fatal(err)
	}
	checkGolden(t, "cloud-run-service.golden.yaml", got)
}

func TestGenerateRejectsRelativePaths(t *testing.T) {
	if _, err := Generate(testScenario("in-container"), "parity-x", "out/bin/go/datadog-init",
		"/repo/parity-workload", "/repo/run", nil); err == nil {
		t.Fatal("expected an error for a relative binary path")
	}
}

func TestGenerateGivesSameEnvironmentToEveryBinary(t *testing.T) {
	environments := map[string]any{}
	for _, label := range []string{"go", "rust"} {
		data, err := Generate(testScenario("in-container"), "parity-x-"+label, "/repo/parity/out/bin/"+label+"/datadog-init",
			"/repo/parity/out/bin/workload/parity-workload", "/repo/parity/out/run/x/"+label,
			map[string]string{"K_SERVICE": "parity-service"})
		if err != nil {
			t.Fatal(err)
		}
		var file struct {
			Services map[string]struct {
				Environment map[string]string `yaml:"environment"`
			} `yaml:"services"`
		}
		if err := yaml.Unmarshal(data, &file); err != nil {
			t.Fatal(err)
		}
		environments[label] = file.Services["agent"].Environment
	}
	if !reflect.DeepEqual(environments["go"], environments["rust"]) {
		t.Errorf("environments differ:\ngo:   %v\nrust: %v", environments["go"], environments["rust"])
	}
	if len(environments["go"].(map[string]string)) == 0 {
		t.Error("the agent environment is empty")
	}
}

func TestIntakeEnvironmentCoversAllPayloadTypes(t *testing.T) {
	want := map[string]string{
		"DD_DD_URL":                     "http://fakeintake:80",
		"DD_APM_DD_URL":                 "http://fakeintake:80",
		"DD_LOGS_CONFIG_LOGS_DD_URL":    "fakeintake:80",
		"DD_LOGS_CONFIG_LOGS_NO_SSL":    "true",
		"DD_LOGS_CONFIG_FORCE_USE_HTTP": "true",
		"DD_API_KEY":                    "00000000000000000000000000parity",
	}
	got := IntakeEnvironment()
	for key, value := range want {
		if got[key] != value {
			t.Errorf("%s = %q, want %q", key, got[key], value)
		}
	}
}

func TestEnvironmentPrecedence(t *testing.T) {
	s := scenario.Scenario{Environment: map[string]string{"DD_SERVICE": "from-scenario", "DD_API_KEY": "scenario-key"}}
	got := Environment(s, map[string]string{"DD_SERVICE": "from-platform", "K_REVISION": "1"})
	for key, value := range map[string]string{
		"DD_SERVICE":    "from-scenario",
		"DD_API_KEY":    "scenario-key",
		"K_REVISION":    "1",
		"DD_APM_DD_URL": "http://fakeintake:80",
	} {
		if got[key] != value {
			t.Errorf("%s = %q, want %q", key, got[key], value)
		}
	}
}

func TestProjectName(t *testing.T) {
	tests := []struct{ runID, scenario, label, want string }{
		{"20261123-001714-2873955499", "process.signal-forwarding", "go", "parity-20261123-001714-2873955499-process-signal-forwarding-go"},
		{"", "Traces.Basic_v04", "rust", "parity-traces-basic_v04-rust"},
	}
	for _, tt := range tests {
		if got := ProjectName(tt.runID, tt.scenario, tt.label); got != tt.want {
			t.Errorf("ProjectName(%q, %q, %q) = %q, want %q", tt.runID, tt.scenario, tt.label, got, tt.want)
		}
	}
}

func TestParseEnvFile(t *testing.T) {
	tests := []struct {
		name    string
		input   string
		want    map[string]string
		wantErr bool
	}{
		{name: "comments only", input: "# nothing\n\n", want: map[string]string{}},
		{name: "values", input: "K_SERVICE=svc\n  # comment\nURL=http://x?a=b\nEMPTY=\n", want: map[string]string{"K_SERVICE": "svc", "URL": "http://x?a=b", "EMPTY": ""}},
		{name: "missing equals", input: "K_SERVICE\n", wantErr: true},
		{name: "empty key", input: "=value\n", wantErr: true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := ParseEnvFile([]byte(tt.input))
			if (err != nil) != tt.wantErr {
				t.Fatalf("error = %v, want error %v", err, tt.wantErr)
			}
			if !tt.wantErr && !reflect.DeepEqual(got, tt.want) {
				t.Errorf("got %v, want %v", got, tt.want)
			}
		})
	}
}

func TestLocalPlatformSetsNoVariables(t *testing.T) {
	got, err := LoadPlatform("../../platforms", "local")
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 0 {
		t.Errorf("local platform sets %v, want no variables", got)
	}
}
