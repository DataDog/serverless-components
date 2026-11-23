// Package compose writes the Docker Compose file that runs one scenario against one binary.
package compose

import (
	"bufio"
	"bytes"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"gopkg.in/yaml.v3"

	"github.com/DataDog/serverless-components/parity/internal/build"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
)

const (
	// AgentImage is the image the binary under test runs in.
	AgentImage = "debian:12-slim"
	// SharedDir is the directory, inside the containers, that the agent and the workload share.
	SharedDir = "/parity-shared"
	// EventLogFile is the name of the workload's event log inside SharedDir.
	EventLogFile = "events.jsonl"
	// IntakeURL is the address of the fakeintake service inside the Compose network.
	IntakeURL = "http://fakeintake:80"
	// MetadataService is the Compose service of the fake Google Cloud metadata server.
	MetadataService = "fake-gcp-metadata"
	// MetadataHost is the host Go serverless-init calls for Google Cloud metadata
	// (datadog-agent cmd/serverless-init/cloudservice/cloudrun.go:42), the network alias of
	// MetadataService.
	MetadataHost = "metadata.google.internal"
)

// GoogleCloudPlatform reports whether a platform runs on Google Cloud, so its scenarios need the
// fake metadata server. Google Cloud platform files are named cloud-run-<kind>.env.
func GoogleCloudPlatform(platform string) bool {
	return strings.HasPrefix(platform, "cloud-run-")
}

// MetadataBinary returns the path of the fake metadata server, which parity build workload
// writes next to the workload driver.
func MetadataBinary(workloadBinary string) string {
	return filepath.Join(filepath.Dir(workloadBinary), MetadataService)
}

// IntakeEnvironment returns the variables that point every payload type at fakeintake.
// The list must be confirmed during the first run against Go serverless-init: any payload that
// does not reach fakeintake means a variable is missing here.
func IntakeEnvironment() map[string]string {
	return map[string]string{
		// A fixed fake key. Thirty-two characters, the length of a real key.
		"DD_API_KEY": "00000000000000000000000000parity",
		// Metrics: series, sketches and service checks.
		"DD_DD_URL": IntakeURL,
		// Traces and APM stats.
		"DD_APM_DD_URL": IntakeURL,
		// Proxied tracer endpoints.
		"DD_APM_PROFILING_DD_URL":       IntakeURL + "/api/v2/profile",
		"DD_APM_TELEMETRY_DD_URL":       IntakeURL,
		"DD_APM_DEBUGGER_DD_URL":        IntakeURL,
		"DD_APM_SYMDB_DD_URL":           IntakeURL,
		"DD_EVP_PROXY_CONFIG_DD_URL":    IntakeURL,
		"DD_LOGS_CONFIG_LOGS_DD_URL":    "fakeintake:80",
		"DD_LOGS_CONFIG_LOGS_NO_SSL":    "true",
		"DD_LOGS_CONFIG_FORCE_USE_HTTP": "true",
		"DD_SKIP_SSL_VALIDATION":        "true",
	}
}

// Environment returns the variables every container in the scenario receives. Later sources win:
// the platform file, then the intake variables, then the scenario's own variables.
func Environment(s scenario.Scenario, platformEnv map[string]string) map[string]string {
	env := maps.Clone(platformEnv)
	if env == nil {
		env = map[string]string{}
	}
	maps.Copy(env, IntakeEnvironment())
	maps.Copy(env, s.Environment)
	return env
}

var invalidProjectCharacters = regexp.MustCompile(`[^a-z0-9_-]+`)

// ProjectName returns the Compose project name parity-<run id>-<scenario>-<label>, or
// parity-<scenario>-<label> without a run id, with characters that Compose does not allow
// replaced by hyphens. The run id keeps the projects of two invocations apart.
func ProjectName(runID, scenarioName, label string) string {
	name := "parity-" + scenarioName + "-" + label
	if runID != "" {
		name = "parity-" + runID + "-" + scenarioName + "-" + label
	}
	return invalidProjectCharacters.ReplaceAllString(strings.ToLower(name), "-")
}

// BinaryLabel returns the label of a binary built at out/bin/<label>/datadog-init.
func BinaryLabel(binary string) string {
	return filepath.Base(filepath.Dir(binary))
}

// ScenarioFile returns the host path of the scenario copy in runDir. The workload requires the file
// name to equal the scenario name.
func ScenarioFile(s scenario.Scenario, runDir string) string {
	return filepath.Join(runDir, s.Name+".yaml")
}

// SharedHostDir returns the host directory mounted at SharedDir.
func SharedHostDir(runDir string) string {
	return filepath.Join(runDir, "shared")
}

type file struct {
	Name     string             `yaml:"name"`
	Services map[string]service `yaml:"services"`
}

type service struct {
	Image       string                       `yaml:"image"`
	Entrypoint  []string                     `yaml:"entrypoint,omitempty"`
	Environment map[string]string            `yaml:"environment,omitempty"`
	Volumes     []string                     `yaml:"volumes,omitempty"`
	Ports       []string                     `yaml:"ports,omitempty"`
	NetworkMode string                       `yaml:"network_mode,omitempty"`
	Networks    map[string]network           `yaml:"networks,omitempty"`
	DependsOn   map[string]map[string]string `yaml:"depends_on,omitempty"`
	Healthcheck *healthcheck                 `yaml:"healthcheck,omitempty"`
}

type network struct {
	Aliases []string `yaml:"aliases"`
}

type healthcheck struct {
	Test          []string `yaml:"test"`
	Interval      string   `yaml:"interval"`
	StartPeriod   string   `yaml:"start_period"`
	StartInterval string   `yaml:"start_interval"`
}

// Generate returns the Compose file of project for scenario s run against binary. All paths must
// be absolute.
func Generate(s scenario.Scenario, project, binary, workloadBinary, runDir string, platformEnv map[string]string) ([]byte, error) {
	for _, path := range []string{binary, workloadBinary, runDir} {
		if !filepath.IsAbs(path) {
			return nil, fmt.Errorf("path %q must be absolute", path)
		}
	}
	containerScenario := "/parity/" + s.Name + ".yaml"
	workloadCommand := []string{"/parity-workload", "--scenario", containerScenario, "--events", SharedDir + "/" + EventLogFile}
	scenarioMount := ScenarioFile(s, runDir) + ":" + containerScenario + ":ro"
	sharedMount := SharedHostDir(runDir) + ":" + SharedDir
	env := Environment(s, platformEnv)

	agent := service{
		Image:       AgentImage,
		Environment: env,
		Volumes:     []string{binary + ":/datadog-init:ro"},
		DependsOn:   map[string]map[string]string{"fakeintake": {"condition": "service_healthy"}},
	}
	services := map[string]service{
		"fakeintake": {
			Image: build.FakeintakeImage,
			// Published to a random host port on the loopback address, so the host can query it.
			Ports: []string{"127.0.0.1::80"},
			// The runner reads the Docker clock with /parity-workload --print-clock, because the
			// image's busybox date has no sub-second precision.
			Volumes: []string{workloadBinary + ":/parity-workload:ro"},
			Healthcheck: &healthcheck{
				Test:          []string{"CMD", "curl", "-fsS", "http://127.0.0.1/fakeintake/health"},
				Interval:      "5s",
				StartPeriod:   "30s",
				StartInterval: "250ms",
			},
		},
	}
	if GoogleCloudPlatform(s.Platform) {
		services[MetadataService] = metadataService(s.Metadata, MetadataBinary(workloadBinary))
		agent.DependsOn[MetadataService] = map[string]string{"condition": "service_healthy"}
	}
	switch s.Mode {
	case "in-container":
		agent.Entrypoint = append([]string{"/datadog-init"}, workloadCommand...)
		agent.Volumes = append(agent.Volumes, workloadBinary+":/parity-workload:ro", scenarioMount, sharedMount)
	case "sidecar":
		agent.Entrypoint = []string{"/datadog-init"}
		agent.Volumes = append(agent.Volumes, sharedMount)
		services["workload"] = service{
			Image:       AgentImage,
			Entrypoint:  workloadCommand,
			Environment: env,
			Volumes:     []string{workloadBinary + ":/parity-workload:ro", scenarioMount, sharedMount},
			NetworkMode: "service:agent",
			DependsOn:   map[string]map[string]string{"agent": {"condition": "service_started"}},
		}
	default:
		return nil, fmt.Errorf("scenario %q has unknown mode %q", s.Name, s.Mode)
	}
	services["agent"] = agent
	return yaml.Marshal(file{Name: project, Services: services})
}

// metadataService returns the fake metadata server, reachable as MetadataHost.
func metadataService(m scenario.Metadata, binary string) service {
	mode := m.Mode
	if mode == "" {
		mode = "answer"
	}
	return service{
		Image:      AgentImage,
		Entrypoint: []string{"/" + MetadataService, "--mode", mode, "--delay", m.Delay.String()},
		Volumes:    []string{binary + ":/" + MetadataService + ":ro"},
		Networks:   map[string]network{"default": {Aliases: []string{MetadataHost}}},
		Healthcheck: &healthcheck{
			Test:          []string{"CMD", "/" + MetadataService, "--check"},
			Interval:      "5s",
			StartPeriod:   "30s",
			StartInterval: "250ms",
		},
	}
}

// LoadPlatform reads <platformsDir>/<platform>.env.
func LoadPlatform(platformsDir, platform string) (map[string]string, error) {
	path := filepath.Join(platformsDir, platform+".env")
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("reading the platform file: %w", err)
	}
	env, err := ParseEnvFile(data)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	return env, nil
}

// ParseEnvFile reads KEY=VALUE lines. Blank lines and lines starting with # are ignored.
func ParseEnvFile(data []byte) (map[string]string, error) {
	env := map[string]string{}
	scanner := bufio.NewScanner(bytes.NewReader(data))
	for number := 1; scanner.Scan(); number++ {
		line := strings.TrimSpace(scanner.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		key, value, found := strings.Cut(line, "=")
		if !found || key == "" {
			return nil, fmt.Errorf("line %d: expected KEY=VALUE, found %q", number, line)
		}
		env[key] = value
	}
	return env, scanner.Err()
}
