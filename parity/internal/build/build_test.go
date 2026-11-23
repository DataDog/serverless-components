package build

import (
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

type call struct {
	dir  string
	env  []string
	name string
	args []string
}

// fakeRunner records calls, fails the commands named in failNames, and runs onRun for the others.
type fakeRunner struct {
	calls     []call
	failNames map[string]bool
	onRun     func(c call)
}

func (f *fakeRunner) Run(dir string, env []string, name string, args ...string) error {
	c := call{dir, env, name, args}
	f.calls = append(f.calls, c)
	if f.failNames[name] {
		return errors.New("command failed")
	}
	if f.onRun != nil {
		f.onRun(c)
	}
	return nil
}

func TestGoCommandSetsBuildScriptEnvironment(t *testing.T) {
	dir, env, name, args := GoCommand("/ci", "/agent", "arm64")
	if dir != "/ci" || name != "scripts/build.sh" || len(args) != 0 {
		t.Fatalf("unexpected command: %q %q %q", dir, name, args)
	}
	want := []string{"VERSION=parity", "VARIANT=standard", "ARCHITECTURE=arm64", "AGENT_PATH=/agent"}
	if !reflect.DeepEqual(env, want) {
		t.Fatalf("environment = %q, want %q", env, want)
	}
}

func TestCommands(t *testing.T) {
	tests := []struct {
		name     string
		got      func() (string, []string, string, []string)
		wantDir  string
		wantName string
		wantArgs []string
	}{
		{
			name:     "rust with zigbuild on arm64",
			got:      func() (string, []string, string, []string) { return RustCommand("/repo", "arm64", false) },
			wantDir:  "/repo",
			wantName: "cargo",
			wantArgs: []string{"zigbuild", "--release", "-p", "datadog-init", "--target", "aarch64-unknown-linux-musl"},
		},
		{
			name:     "rust with cross on amd64",
			got:      func() (string, []string, string, []string) { return RustCommand("/repo", "amd64", true) },
			wantDir:  "/repo",
			wantName: "cross",
			wantArgs: []string{"build", "--release", "-p", "datadog-init", "--target", "x86_64-unknown-linux-musl"},
		},
		{
			name:     "fakeintake image",
			got:      func() (string, []string, string, []string) { return FakeintakeImageCommand("/agent") },
			wantDir:  "/agent",
			wantName: "docker",
			wantArgs: []string{"build", "-t", "parity-fakeintake:local", "-f", "test/fakeintake/Dockerfile", "."},
		},
		{
			name: "workload",
			got: func() (string, []string, string, []string) {
				return WorkloadCommand("/repo/parity", "arm64", "/out/parity-workload")
			},
			wantDir:  "/repo/parity",
			wantName: "go",
			wantArgs: []string{"build", "-o", "/out/parity-workload", "./cmd/workload"},
		},
		{
			name: "fake metadata server",
			got: func() (string, []string, string, []string) {
				return MetadataServerCommand("/repo/parity", "arm64", "/out/fake-gcp-metadata")
			},
			wantDir:  "/repo/parity",
			wantName: "go",
			wantArgs: []string{"build", "-o", "/out/fake-gcp-metadata", "./cmd/fake-gcp-metadata"},
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			dir, _, name, args := tt.got()
			if dir != tt.wantDir || name != tt.wantName || !reflect.DeepEqual(args, tt.wantArgs) {
				t.Fatalf("got %q %q %q, want %q %q %q", dir, name, args, tt.wantDir, tt.wantName, tt.wantArgs)
			}
		})
	}
}

func TestWorkloadCommandEnvironment(t *testing.T) {
	_, env, _, _ := WorkloadCommand("/repo/parity", "amd64", "/out/x")
	want := []string{"CGO_ENABLED=0", "GOOS=linux", "GOARCH=amd64"}
	if !reflect.DeepEqual(env, want) {
		t.Fatalf("environment = %q, want %q", env, want)
	}
}

func TestRustCommandFallsBackToCross(t *testing.T) {
	root := t.TempDir()
	r := &fakeRunner{failNames: map[string]bool{"cargo": true}}
	r.onRun = func(c call) {
		if c.name == "cross" {
			writeFile(t, filepath.Join(root, "target/aarch64-unknown-linux-musl/release/datadog-init"), "rust")
		}
	}
	cfg := Config{ComponentsDir: root, Arch: "arm64"}
	if _, err := Build("rust", r, cfg); err != nil {
		t.Fatal(err)
	}
	if len(r.calls) != 2 || r.calls[0].name != "cargo" || r.calls[1].name != "cross" {
		t.Fatalf("calls = %+v", r.calls)
	}
	if !reflect.DeepEqual(r.calls[1].args[1:], r.calls[0].args[1:]) {
		t.Fatalf("cross args %q differ from cargo args %q", r.calls[1].args, r.calls[0].args)
	}
}

func TestBuildCopiesBinaryToOut(t *testing.T) {
	tests := []struct {
		target     string
		sourceFile func(cfg Config) string
		wantResult func(cfg Config) string
	}{
		{
			target:     "go",
			sourceFile: func(c Config) string { return filepath.Join(c.CIDir, "bin/datadog_extension-arm64/datadog-agent") },
			wantResult: func(c Config) string { return filepath.Join(c.ComponentsDir, "parity/out/bin/go/datadog-init") },
		},
		{
			target: "rust",
			sourceFile: func(c Config) string {
				return filepath.Join(c.ComponentsDir, "target/aarch64-unknown-linux-musl/release/datadog-init")
			},
			wantResult: func(c Config) string { return filepath.Join(c.ComponentsDir, "parity/out/bin/rust/datadog-init") },
		},
		{
			target: "workload",
			sourceFile: func(c Config) string {
				return filepath.Join(c.ComponentsDir, "parity/out/bin/workload/parity-workload")
			},
			wantResult: func(c Config) string {
				return filepath.Join(c.ComponentsDir, "parity/out/bin/workload/parity-workload")
			},
		},
	}
	for _, tt := range tests {
		t.Run(tt.target, func(t *testing.T) {
			fakeCheckout(t, Info{Commit: "abc"})
			cfg := Config{AgentPath: "/agent", CIDir: t.TempDir(), ComponentsDir: t.TempDir(), Arch: "arm64"}
			r := &fakeRunner{onRun: func(call) { writeFile(t, tt.sourceFile(cfg), "binary-content") }}
			got, err := Build(tt.target, r, cfg)
			if err != nil {
				t.Fatal(err)
			}
			if want := tt.wantResult(cfg); got != want {
				t.Fatalf("path = %q, want %q", got, want)
			}
			content, err := os.ReadFile(got)
			if err != nil || string(content) != "binary-content" {
				t.Fatalf("content = %q, err = %v", content, err)
			}
			if info, _ := os.Stat(got); info.Mode()&0o111 == 0 {
				t.Fatalf("binary is not executable: %v", info.Mode())
			}
		})
	}
}

func TestBuildFakeintakeReturnsImageName(t *testing.T) {
	r := &fakeRunner{}
	got, err := Build("fakeintake", r, Config{AgentPath: "/agent"})
	if err != nil || got != "parity-fakeintake:local" {
		t.Fatalf("got %q, %v", got, err)
	}
}

func TestBuildRejectsUnknownTarget(t *testing.T) {
	if _, err := Build("python", &fakeRunner{}, Config{}); err == nil {
		t.Fatal("expected an error for an unknown target")
	}
}

func writeFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o755); err != nil {
		t.Fatal(err)
	}
}

func TestDefaultConfig(t *testing.T) {
	tests := []struct {
		name string
		env  map[string]string
		want Config
	}{
		{
			name: "defaults",
			want: Config{AgentPath: "/home/dev/dd/datadog-agent", CIDir: "/home/dev/dd/serverless-init-ci", ComponentsDir: "/home/dev/dd/serverless-components"},
		},
		{
			name: "environment overrides",
			env:  map[string]string{"PARITY_AGENT_PATH": "/a", "PARITY_CI_DIR": "/c", "PARITY_COMPONENTS_DIR": "/s"},
			want: Config{AgentPath: "/a", CIDir: "/c", ComponentsDir: "/s"},
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Setenv("HOME", "/home/dev")
			for _, key := range []string{"PARITY_AGENT_PATH", "PARITY_CI_DIR", "PARITY_COMPONENTS_DIR"} {
				t.Setenv(key, tt.env[key])
			}
			got, err := DefaultConfig()
			if err != nil {
				t.Fatal(err)
			}
			got.Arch = ""
			if got != tt.want {
				t.Fatalf("got %+v, want %+v", got, tt.want)
			}
		})
	}
}

func TestDefaultConfigFailsWithoutHomeDirectory(t *testing.T) {
	t.Setenv("HOME", "")
	if _, err := DefaultConfig(); err == nil {
		t.Fatal("expected an error when the home directory is unknown")
	}
}

func TestResolve(t *testing.T) {
	t.Setenv("HOME", "/home/dev")
	workingDirectory, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	got, err := Config{AgentPath: "~/dd/agent", CIDir: "relative/ci", ComponentsDir: "/already/absolute"}.Resolve()
	if err != nil {
		t.Fatal(err)
	}
	want := Config{AgentPath: "/home/dev/dd/agent", CIDir: filepath.Join(workingDirectory, "relative/ci"), ComponentsDir: "/already/absolute"}
	if got != want {
		t.Fatalf("got %+v, want %+v", got, want)
	}
}

func TestRustBuildErrorKeepsBothFailures(t *testing.T) {
	r := &fakeRunner{failNames: map[string]bool{"cargo": true, "cross": true}}
	_, err := Build("rust", r, Config{ComponentsDir: t.TempDir(), Arch: "arm64"})
	if err == nil || strings.Count(err.Error(), "command failed") != 2 {
		t.Fatalf("error should contain both failures, got %v", err)
	}
}

// fakeCheckout makes Build describe every checkout as info.
func fakeCheckout(t *testing.T, info Info) {
	t.Helper()
	original := describeCheckout
	describeCheckout = func(path string) (Info, error) { info.AgentPath = path; return info, nil }
	t.Cleanup(func() { describeCheckout = original })
}

func TestBuildGoWritesBuildInfo(t *testing.T) {
	fakeCheckout(t, Info{Commit: "144c8aebb3a455ec264ccdbd82a156c5fac34ed3", Dirty: true})
	cfg := Config{AgentPath: "/agent", CIDir: t.TempDir(), ComponentsDir: t.TempDir(), Arch: "arm64"}
	r := &fakeRunner{onRun: func(call) {
		writeFile(t, filepath.Join(cfg.CIDir, "bin/datadog_extension-arm64/datadog-agent"), "binary")
	}}
	if _, err := Build("go", r, cfg); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(cfg.ComponentsDir, "parity/out/bin/go/BUILD_INFO")
	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if want := "agent_path: /agent\ncommit: 144c8aebb3a455ec264ccdbd82a156c5fac34ed3\ndirty: true\n"; string(content) != want {
		t.Fatalf("BUILD_INFO = %q, want %q", content, want)
	}
	got, err := ReadInfo(path)
	if want := (Info{AgentPath: "/agent", Commit: "144c8aebb3a455ec264ccdbd82a156c5fac34ed3", Dirty: true}); err != nil || got != want {
		t.Fatalf("ReadInfo = %+v, %v, want %+v", got, err, want)
	}
}

// A failed build must not leave the previous binary, which record would then take for the new one.
func TestBuildGoRemovesTheStaleBinaryFirst(t *testing.T) {
	fakeCheckout(t, Info{Commit: "abc"})
	cfg := Config{AgentPath: "/agent", CIDir: t.TempDir(), ComponentsDir: t.TempDir(), Arch: "arm64"}
	outDir := filepath.Join(cfg.ComponentsDir, "parity/out/bin/go")
	writeFile(t, filepath.Join(outDir, "datadog-init"), "old binary")
	writeFile(t, filepath.Join(outDir, "BUILD_INFO"), "commit: old\n")
	r := &fakeRunner{failNames: map[string]bool{"scripts/build.sh": true}}
	if _, err := Build("go", r, cfg); err == nil {
		t.Fatal("Build succeeded although the build script failed")
	}
	for _, name := range []string{"datadog-init", "BUILD_INFO"} {
		if _, err := os.Stat(filepath.Join(outDir, name)); !os.IsNotExist(err) {
			t.Errorf("%s is still there after a failed build: %v", name, err)
		}
	}
}
