// Package reference saves and loads the reference captures recorded from Go, and checks that they
// were recorded from the pinned datadog-agent commit.
package reference

import (
	"bufio"
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"

	"github.com/DataDog/serverless-components/parity/internal/build"
	"github.com/DataDog/serverless-components/parity/internal/capture"
	"github.com/DataDog/serverless-components/parity/internal/normalize"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

// Resources are the startup time and peak memory of one run.
type Resources struct {
	// Startup is the time from the container start until the workload driver's receiver_ready
	// event. It is not measurable when the driver logged no such event.
	Startup         timing.Measurement `json:"startup"`
	PeakMemoryBytes uint64             `json:"peak_memory_bytes,omitempty"`
	// PeakMemoryKnown is false when no memory sample succeeded.
	PeakMemoryKnown bool `json:"peak_memory_known"`
}

// MeasureResources returns the startup time and peak memory of the run that produced c. Both the
// event times and the container start time are on the Docker clock.
func MeasureResources(c capture.Capture) Resources {
	r := Resources{PeakMemoryBytes: c.Outcome.PeakMemoryBytes, PeakMemoryKnown: c.Outcome.PeakMemoryKnown}
	for _, e := range c.Events {
		if e.Kind == "receiver_ready" && !c.Outcome.StartedAt.IsZero() {
			r.Startup = timing.Measurement{Measurable: true, Value: timing.Duration(e.Time.Sub(c.Outcome.StartedAt))}
			break
		}
	}
	return r
}

// Reference is the normalized capture, timing measures and resources of one scenario run against
// Go, with what it was recorded from: the SHA-256 of the Go binary, and of each input file by its
// name relative to the parity directory. Measures are informational; a comparison measures
// Normalized.Timing again with the current tolerances.
type Reference struct {
	Scenario       string               `json:"scenario"`
	GoBinarySHA256 string               `json:"go_binary_sha256"`
	Inputs         map[string]string    `json:"inputs"`
	Normalized     normalize.Normalized `json:"normalized"`
	Measures       timing.Measures      `json:"measures"`
	Resources      Resources            `json:"resources"`
}

// HashInputs returns the SHA-256 of each file in files, which maps a name to a path.
func HashInputs(files map[string]string) (map[string]string, error) {
	hashes := map[string]string{}
	for name, path := range files {
		hash, err := FileSHA256(path)
		if err != nil {
			return nil, err
		}
		hashes[name] = hash
	}
	return hashes, nil
}

// Check returns an error when r was recorded with another Go binary than the one source names, or
// with input files whose hashes differ from current, so that it can't be compared.
func Check(r Reference, source Source, current map[string]string) error {
	if r.GoBinarySHA256 != source.GoBinarySHA256 {
		return fmt.Errorf("the reference of %s was recorded with the Go binary %s, but reference/SOURCE names %s; run parity record --scenario %s",
			r.Scenario, r.GoBinarySHA256, source.GoBinarySHA256, r.Scenario)
	}
	for _, name := range slices.Sorted(maps.Keys(current)) {
		if r.Inputs[name] != current[name] {
			return fmt.Errorf("the reference of %s is stale: %s changed since it was recorded; run parity record --scenario %s", r.Scenario, name, r.Scenario)
		}
	}
	return nil
}

// CheckBuild returns an error when the Go binary, described by info and with the SHA-256 binary,
// was not built from a clean checkout at commit, or, without repin, is not the binary that pinned
// names.
func CheckBuild(info build.Info, binary string, pinned Source, commit string, repin bool) error {
	if info.Dirty {
		return errors.New("the Go binary was built from a datadog-agent checkout with uncommitted changes; build it again from a clean checkout with parity build go")
	}
	if info.Commit != commit {
		return fmt.Errorf("the Go binary was built from datadog-agent %s, not from %s; build it again with parity build go", info.Commit, commit)
	}
	if !repin && binary != pinned.GoBinarySHA256 {
		return fmt.Errorf("the Go binary's SHA-256 is %s, but reference/SOURCE pins %s; build the pinned binary, or run parity record --repin", binary, pinned.GoBinarySHA256)
	}
	return nil
}

// Save writes r to <dir>/<scenario>.json.
func Save(dir string, r Reference) error {
	data, err := json.MarshalIndent(r, "", "  ")
	if err != nil {
		return fmt.Errorf("encoding the reference of %s: %w", r.Scenario, err)
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(dir, r.Scenario+".json"), append(data, '\n'), 0o644)
}

// Load reads <dir>/<scenario>.json. It returns nil without an error when the scenario has no
// reference.
func Load(dir, scenario string) (*Reference, error) {
	path := filepath.Join(dir, scenario+".json")
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var r Reference
	if err := json.Unmarshal(data, &r); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	return &r, nil
}

// Source is the content of the SOURCE file: the datadog-agent commit the references were
// recorded from, the serverless-init release that commit corresponds to, the SHA-256 of the
// Go binary that recorded them, and the commit of the fakeintake checkout whose decoders read them.
type Source struct {
	Commit, Release, GoBinarySHA256, FakeintakeCommit string
}

func (s Source) String() string {
	release, binary := s.Release, s.GoBinarySHA256
	if release == "" {
		release = "not recorded"
	}
	if binary == "" {
		binary = "not recorded"
	}
	fakeintake := s.FakeintakeCommit
	if fakeintake == "" {
		fakeintake = "not recorded"
	}
	return fmt.Sprintf("datadog-agent commit %s, serverless-init release %s, Go binary SHA-256 %s, fakeintake commit %s", s.Commit, release, binary, fakeintake)
}

const sourceHeader = `# The datadog-agent commit that the reference captures were recorded from, the serverless-init
# release that it corresponds to, the SHA-256 of the Go binary that recorded them, and the commit
# of the fakeintake checkout whose decoders read the payloads. parity record writes the commit, the
# binary and the fakeintake commit; a person keeps the release line up to date.
`

// ReadSource reads the "commit:", "release:" and "go_binary_sha256:" lines of the SOURCE file at
// path. Lines that start with # are comments.
func ReadSource(path string) (Source, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return Source{}, err
	}
	var s Source
	for _, line := range strings.Split(string(data), "\n") {
		key, value, _ := strings.Cut(line, ":")
		value = strings.TrimSpace(value)
		switch strings.TrimSpace(key) {
		case "commit":
			s.Commit = value
		case "release":
			s.Release = value
		case "go_binary_sha256":
			s.GoBinarySHA256 = value
		case "fakeintake_commit":
			s.FakeintakeCommit = value
		}
	}
	if s.Commit == "" {
		return Source{}, fmt.Errorf("%s has no commit line", path)
	}
	return s, nil
}

// WriteSource writes s to the SOURCE file at path.
func WriteSource(path string, s Source) error {
	content := sourceHeader + "commit: " + s.Commit + "\nrelease: " + s.Release + "\ngo_binary_sha256: " + s.GoBinarySHA256 +
		"\nfakeintake_commit: " + s.FakeintakeCommit + "\n"
	return os.WriteFile(path, []byte(content), 0o644)
}

// CheckSource checks that the datadog-agent checkout at agentPath has no uncommitted changes and
// is at the commit in the SOURCE file at sourceFile, and returns the checkout's commit. With
// repin, the commit may differ and SOURCE may be missing. A checkout with uncommitted changes is
// refused even with repin, because no commit describes it. CheckSource changes nothing.
func CheckSource(sourceFile, agentPath string, repin bool) (string, error) {
	status, err := git(agentPath, "status", "--porcelain")
	if err != nil {
		return "", err
	}
	if status != "" {
		return "", errors.New("datadog-agent has uncommitted changes")
	}
	head, err := git(agentPath, "rev-parse", "HEAD")
	if err != nil {
		return "", err
	}
	if repin {
		return head, nil
	}
	recorded, err := ReadSource(sourceFile)
	if errors.Is(err, fs.ErrNotExist) {
		return "", fmt.Errorf("%s does not exist; run parity record --repin to pin the references to the datadog-agent commit %s", sourceFile, head)
	}
	if err != nil {
		return "", err
	}
	if recorded.Commit != head {
		return "", fmt.Errorf("datadog-agent is at %s, references were recorded at %s", head, recorded.Commit)
	}
	return head, nil
}

// Publish moves the reference files staged in stagingDir into referenceDir, and then writes
// source to referenceDir/SOURCE. Every scenario in scenarios must have a staged file, or Publish
// returns an error and changes nothing. With prune, it also removes the reference files of
// scenarios that are not in scenarios. The SOURCE release line is kept from the existing file.
func Publish(stagingDir, referenceDir string, scenarios []string, prune bool, source Source) error {
	for _, name := range scenarios {
		if _, err := os.Stat(filepath.Join(stagingDir, name+".json")); err != nil {
			return fmt.Errorf("scenario %s has no staged reference, so no reference was changed: %w", name, err)
		}
	}
	sourceFile := filepath.Join(referenceDir, "SOURCE")
	if existing, err := ReadSource(sourceFile); err == nil {
		source.Release = existing.Release
	} else if !errors.Is(err, fs.ErrNotExist) {
		return err
	}
	if err := os.MkdirAll(referenceDir, 0o755); err != nil {
		return err
	}
	for _, name := range scenarios {
		if err := os.Rename(filepath.Join(stagingDir, name+".json"), filepath.Join(referenceDir, name+".json")); err != nil {
			return fmt.Errorf("moving the reference of %s: %w", name, err)
		}
	}
	if prune {
		existing, err := filepath.Glob(filepath.Join(referenceDir, "*.json"))
		if err != nil {
			return err
		}
		for _, path := range existing {
			if !slices.Contains(scenarios, strings.TrimSuffix(filepath.Base(path), ".json")) {
				if err := os.Remove(path); err != nil {
					return err
				}
			}
		}
	}
	return WriteSource(sourceFile, source)
}

// FileSHA256 returns the hexadecimal SHA-256 of the file at path.
func FileSHA256(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	hash := sha256.New()
	if _, err := io.Copy(hash, f); err != nil {
		return "", fmt.Errorf("hashing %s: %w", path, err)
	}
	return hex.EncodeToString(hash.Sum(nil)), nil
}

// FakeintakeCommit returns the commit of the fakeintake checkout that go.mod in parityDir replaces
// the fakeintake module with, followed by a note when that directory has uncommitted changes.
func FakeintakeCommit(parityDir string) (string, error) {
	data, err := os.ReadFile(filepath.Join(parityDir, "go.mod"))
	if err != nil {
		return "", err
	}
	var dir string
	scanner := bufio.NewScanner(bytes.NewReader(data))
	for scanner.Scan() {
		fields := strings.Fields(strings.TrimPrefix(strings.TrimSpace(scanner.Text()), "replace "))
		if len(fields) == 3 && fields[0] == "github.com/DataDog/datadog-agent/test/fakeintake" && fields[1] == "=>" {
			dir = fields[2]
		}
	}
	if dir == "" {
		return "", errors.New("go.mod does not replace the fakeintake module with a local checkout")
	}
	if !filepath.IsAbs(dir) {
		dir = filepath.Join(parityDir, dir)
	}
	commit, err := git(dir, "rev-parse", "HEAD")
	if err != nil {
		return "", err
	}
	status, err := git(dir, "status", "--porcelain", "--", ".")
	if err != nil {
		return "", err
	}
	if status != "" {
		commit += " (with uncommitted changes in " + dir + ")"
	}
	return commit, nil
}

func git(dir string, args ...string) (string, error) {
	cmd := exec.Command("git", args...)
	cmd.Dir = dir
	out, err := cmd.Output()
	if err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			return "", fmt.Errorf("git %s in %s: %w: %s", strings.Join(args, " "), dir, err, strings.TrimSpace(string(exitErr.Stderr)))
		}
		return "", fmt.Errorf("git %s in %s: %w", strings.Join(args, " "), dir, err)
	}
	return strings.TrimSpace(string(out)), nil
}
