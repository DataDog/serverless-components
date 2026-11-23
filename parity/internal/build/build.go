// Package build builds the binaries and the fakeintake image that the parity harness runs.
package build

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
)

// FakeintakeImage is the name of the locally built fakeintake image.
const FakeintakeImage = "parity-fakeintake:local"

// Runner runs an external command.
type Runner interface {
	Run(dir string, env []string, name string, args ...string) error
}

// ExecRunner runs commands on the host, adding env to the current environment.
type ExecRunner struct{}

// Run runs the command and streams its output to the terminal.
func (ExecRunner) Run(dir string, env []string, name string, args ...string) error {
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	cmd.Env = append(os.Environ(), env...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

// Config holds the locations and the architecture to build for.
type Config struct {
	AgentPath, CIDir, ComponentsDir, Arch string
}

// DefaultConfig reads PARITY_AGENT_PATH, PARITY_CI_DIR and PARITY_COMPONENTS_DIR, and falls back to
// the usual checkout locations and the host architecture. Call Resolve after applying flags.
func DefaultConfig() (Config, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return Config{}, fmt.Errorf("finding the home directory for the default checkout locations: %w", err)
	}
	return Config{
		AgentPath:     envOr("PARITY_AGENT_PATH", filepath.Join(home, "dd/datadog-agent")),
		CIDir:         envOr("PARITY_CI_DIR", filepath.Join(home, "dd/serverless-init-ci")),
		ComponentsDir: envOr("PARITY_COMPONENTS_DIR", filepath.Join(home, "dd/serverless-components")),
		Arch:          runtime.GOARCH,
	}, nil
}

// Resolve expands a leading "~/" to the home directory and makes every path absolute, because the
// commands run in other directories.
func (c Config) Resolve() (Config, error) {
	for _, path := range []*string{&c.AgentPath, &c.CIDir, &c.ComponentsDir} {
		if *path == "~" || strings.HasPrefix(*path, "~/") {
			home, err := os.UserHomeDir()
			if err != nil {
				return Config{}, fmt.Errorf("expanding %q: %w", *path, err)
			}
			*path = filepath.Join(home, strings.TrimPrefix(*path, "~"))
		}
		absolute, err := filepath.Abs(*path)
		if err != nil {
			return Config{}, fmt.Errorf("making %q absolute: %w", *path, err)
		}
		*path = absolute
	}
	return c, nil
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

// GoCommand returns the build script command for the Go serverless-init.
func GoCommand(ciDir, agentPath, arch string) (dir string, env []string, name string, args []string) {
	env = []string{"VERSION=parity", "VARIANT=standard", "ARCHITECTURE=" + arch, "AGENT_PATH=" + agentPath}
	return ciDir, env, "scripts/build.sh", nil
}

// rustTarget maps a Go architecture name to the Rust musl target triple.
func rustTarget(arch string) string {
	if arch == "amd64" {
		return "x86_64-unknown-linux-musl"
	}
	return "aarch64-unknown-linux-musl"
}

// RustCommand returns the command that builds datadog-init, with cross instead of cargo zigbuild
// when useCross is true.
func RustCommand(repo, arch string, useCross bool) (dir string, env []string, name string, args []string) {
	target := rustTarget(arch)
	if useCross {
		return repo, nil, "cross", []string{"build", "--release", "-p", "datadog-init", "--target", target}
	}
	return repo, nil, "cargo", []string{"zigbuild", "--release", "-p", "datadog-init", "--target", target}
}

// FakeintakeImageCommand returns the command that builds the fakeintake image. The Dockerfile copies
// test/fakeintake, so the build context is the agent checkout root.
func FakeintakeImageCommand(agentPath string) (dir string, env []string, name string, args []string) {
	return agentPath, nil, "docker", []string{"build", "-t", FakeintakeImage, "-f", "test/fakeintake/Dockerfile", "."}
}

// WorkloadCommand returns the command that builds the workload driver for Linux.
func WorkloadCommand(parityDir, arch, output string) (dir string, env []string, name string, args []string) {
	return linuxGoCommand(parityDir, arch, output, "./cmd/workload")
}

// MetadataServerCommand returns the command that builds the fake Google Cloud metadata server for
// Linux.
func MetadataServerCommand(parityDir, arch, output string) (dir string, env []string, name string, args []string) {
	return linuxGoCommand(parityDir, arch, output, "./cmd/fake-gcp-metadata")
}

func linuxGoCommand(parityDir, arch, output, pkg string) (dir string, env []string, name string, args []string) {
	env = []string{"CGO_ENABLED=0", "GOOS=linux", "GOARCH=" + arch}
	return parityDir, env, "go", []string{"build", "-o", output, pkg}
}

// Build builds the target (go, rust, workload or fakeintake). For fakeintake it returns the image
// name; for the others it returns the path of the binary under parity/out/bin/<target>.
func Build(target string, r Runner, cfg Config) (string, error) {
	parityDir := filepath.Join(cfg.ComponentsDir, "parity")
	outDir := filepath.Join(parityDir, "out/bin", target)
	switch target {
	case "go":
		// A failed build must not leave the previous binary and its description behind.
		for _, name := range []string{"datadog-init", InfoFile} {
			if err := os.Remove(filepath.Join(outDir, name)); err != nil && !errors.Is(err, fs.ErrNotExist) {
				return "", err
			}
		}
		info, err := describeCheckout(cfg.AgentPath)
		if err != nil {
			return "", err
		}
		dir, env, name, args := GoCommand(cfg.CIDir, cfg.AgentPath, cfg.Arch)
		if err := r.Run(dir, env, name, args...); err != nil {
			return "", fmt.Errorf("building the Go serverless-init: %w", err)
		}
		source := filepath.Join(cfg.CIDir, "bin", "datadog_extension-"+cfg.Arch, "datadog-agent")
		binary, err := copyBinary(source, filepath.Join(outDir, "datadog-init"))
		if err != nil {
			return "", err
		}
		return binary, WriteInfo(filepath.Join(outDir, InfoFile), info)
	case "rust":
		dir, env, name, args := RustCommand(cfg.ComponentsDir, cfg.Arch, false)
		if zigbuildErr := r.Run(dir, env, name, args...); zigbuildErr != nil {
			fmt.Fprintf(os.Stderr, "cargo zigbuild failed (%v); trying cross\n", zigbuildErr)
			dir, env, name, args = RustCommand(cfg.ComponentsDir, cfg.Arch, true)
			if err := r.Run(dir, env, name, args...); err != nil {
				return "", fmt.Errorf("building the Rust datadog-init failed with cargo zigbuild and with cross: %w", errors.Join(zigbuildErr, err))
			}
		}
		source := filepath.Join(cfg.ComponentsDir, "target", rustTarget(cfg.Arch), "release/datadog-init")
		return copyBinary(source, filepath.Join(outDir, "datadog-init"))
	case "workload":
		output := filepath.Join(outDir, "parity-workload")
		if err := os.MkdirAll(outDir, 0o755); err != nil {
			return "", err
		}
		dir, env, name, args := WorkloadCommand(parityDir, cfg.Arch, output)
		if err := r.Run(dir, env, name, args...); err != nil {
			return "", fmt.Errorf("building the workload driver: %w", err)
		}
		// The fake metadata server goes next to the workload driver, where Compose generation
		// looks for it.
		dir, env, name, args = MetadataServerCommand(parityDir, cfg.Arch, filepath.Join(outDir, "fake-gcp-metadata"))
		if err := r.Run(dir, env, name, args...); err != nil {
			return "", fmt.Errorf("building the fake Google Cloud metadata server: %w", err)
		}
		return output, nil
	case "fakeintake":
		dir, env, name, args := FakeintakeImageCommand(cfg.AgentPath)
		if err := r.Run(dir, env, name, args...); err != nil {
			return "", fmt.Errorf("building the fakeintake image: %w", err)
		}
		return FakeintakeImage, nil
	}
	return "", fmt.Errorf("unknown build target %q: use go, rust, workload or fakeintake", target)
}

// InfoFile is the name of the file, next to the Go binary, that describes the checkout it was
// built from.
const InfoFile = "BUILD_INFO"

// Info describes the datadog-agent checkout that the Go binary was built from. Dirty is true when
// the checkout had uncommitted changes.
type Info struct {
	AgentPath, Commit string
	Dirty             bool
}

// describeCheckout returns the Info of the datadog-agent checkout at path. Tests replace it.
var describeCheckout = func(path string) (Info, error) {
	git := func(args ...string) (string, error) {
		cmd := exec.Command("git", args...)
		cmd.Dir = path
		out, err := cmd.Output()
		if err != nil {
			return "", fmt.Errorf("git %s in %s: %w", strings.Join(args, " "), path, err)
		}
		return strings.TrimSpace(string(out)), nil
	}
	commit, err := git("rev-parse", "HEAD")
	if err != nil {
		return Info{}, err
	}
	status, err := git("status", "--porcelain")
	if err != nil {
		return Info{}, err
	}
	return Info{AgentPath: path, Commit: commit, Dirty: status != ""}, nil
}

// WriteInfo writes info to the file at path.
func WriteInfo(path string, info Info) error {
	content := fmt.Sprintf("agent_path: %s\ncommit: %s\ndirty: %t\n", info.AgentPath, info.Commit, info.Dirty)
	return os.WriteFile(path, []byte(content), 0o644)
}

// ReadInfo reads the file at path that WriteInfo wrote.
func ReadInfo(path string) (Info, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return Info{}, err
	}
	var info Info
	for _, line := range strings.Split(string(data), "\n") {
		key, value, _ := strings.Cut(line, ":")
		value = strings.TrimSpace(value)
		switch key {
		case "agent_path":
			info.AgentPath = value
		case "commit":
			info.Commit = value
		case "dirty":
			if info.Dirty, err = strconv.ParseBool(value); err != nil {
				return Info{}, fmt.Errorf("%s: the dirty line %q is not true or false", path, value)
			}
		}
	}
	if info.Commit == "" {
		return Info{}, fmt.Errorf("%s has no commit line", path)
	}
	return info, nil
}

func copyBinary(source, destination string) (string, error) {
	in, err := os.Open(source)
	if err != nil {
		return "", fmt.Errorf("the build did not produce %s: %w", source, err)
	}
	defer in.Close()
	if err := os.MkdirAll(filepath.Dir(destination), 0o755); err != nil {
		return "", err
	}
	out, err := os.OpenFile(destination, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o755)
	if err != nil {
		return "", err
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		return "", err
	}
	return destination, out.Close()
}
