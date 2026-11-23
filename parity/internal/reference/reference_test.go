package reference

import (
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/build"
	"github.com/DataDog/serverless-components/parity/internal/capture"
	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/DataDog/serverless-components/parity/internal/runner"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

// gitRepository creates a git repository with one commit and returns its path and the commit.
func gitRepository(t *testing.T) (string, string) {
	t.Helper()
	dir := t.TempDir()
	git := func(args ...string) string {
		t.Helper()
		cmd := exec.Command("git", append([]string{"-c", "user.name=parity", "-c", "user.email=parity@example.com", "-c", "commit.gpgsign=false"}, args...)...)
		cmd.Dir = dir
		out, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("git %s: %v: %s", strings.Join(args, " "), err, out)
		}
		return strings.TrimSpace(string(out))
	}
	git("init", "--quiet")
	if err := os.WriteFile(filepath.Join(dir, "main.go"), []byte("package main\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	git("add", "main.go")
	git("commit", "--quiet", "-m", "first")
	return dir, git("rev-parse", "HEAD")
}

func writeSource(t *testing.T, commit string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "SOURCE")
	content := "commit: " + commit + "\nrelease: serverless-init 1.7.8\n"
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestCheckSourceRefusesDirtyCheckout(t *testing.T) {
	repository, commit := gitRepository(t)
	if err := os.WriteFile(filepath.Join(repository, "main.go"), []byte("package changed\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	for _, repin := range []bool{false, true} {
		_, err := CheckSource(writeSource(t, commit), repository, repin)
		if err == nil || err.Error() != "datadog-agent has uncommitted changes" {
			t.Errorf("repin %v: CheckSource error = %v, want datadog-agent has uncommitted changes", repin, err)
		}
	}
}

func TestCheckSourceRefusesDifferentCommit(t *testing.T) {
	repository, commit := gitRepository(t)
	_, err := CheckSource(writeSource(t, "0123456789abcdef0123456789abcdef01234567"), repository, false)
	want := "datadog-agent is at " + commit + ", references were recorded at 0123456789abcdef0123456789abcdef01234567"
	if err == nil || err.Error() != want {
		t.Errorf("CheckSource error = %v, want %s", err, want)
	}
	if head, err := CheckSource(writeSource(t, commit), repository, false); err != nil || head != commit {
		t.Errorf("CheckSource at the recorded commit = %q, %v, want %q", head, err, commit)
	}
}

func TestCheckSourceRepinAllowsAnotherCommitAndChangesNothing(t *testing.T) {
	repository, commit := gitRepository(t)
	source := writeSource(t, "0123456789abcdef0123456789abcdef01234567")
	head, err := CheckSource(source, repository, true)
	if err != nil || head != commit {
		t.Fatalf("CheckSource with repin = %q, %v, want %q", head, err, commit)
	}
	got, err := ReadSource(source)
	if err != nil {
		t.Fatal(err)
	}
	want := Source{Commit: "0123456789abcdef0123456789abcdef01234567", Release: "serverless-init 1.7.8"}
	if got != want {
		t.Errorf("SOURCE after CheckSource = %+v, want it unchanged, %+v", got, want)
	}
	if head, err := CheckSource(filepath.Join(t.TempDir(), "SOURCE"), repository, true); err != nil || head != commit {
		t.Errorf("CheckSource with repin and no SOURCE = %q, %v, want %q", head, err, commit)
	}
}

// referenceFiles returns the content of every file in dir, by file name.
func referenceFiles(t *testing.T, dir string) map[string]string {
	t.Helper()
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	files := map[string]string{}
	for _, e := range entries {
		data, err := os.ReadFile(filepath.Join(dir, e.Name()))
		if err != nil {
			t.Fatal(err)
		}
		files[e.Name()] = string(data)
	}
	return files
}

func writeFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestPublishChangesNothingWhenARecordFailed(t *testing.T) {
	staging, references := t.TempDir(), t.TempDir()
	writeFile(t, filepath.Join(references, "SOURCE"), "commit: old\nrelease: serverless-init 1.7.8\n")
	writeFile(t, filepath.Join(references, "a.json"), "old a")
	writeFile(t, filepath.Join(references, "b.json"), "old b")
	writeFile(t, filepath.Join(staging, "a.json"), "new a")
	err := Publish(staging, references, []string{"a", "b"}, true, Source{Commit: "new", GoBinarySHA256: "abc"})
	if err == nil {
		t.Fatal("Publish with a scenario that has no staged reference returned no error")
	}
	want := map[string]string{"SOURCE": "commit: old\nrelease: serverless-init 1.7.8\n", "a.json": "old a", "b.json": "old b"}
	if got := referenceFiles(t, references); !maps.Equal(got, want) {
		t.Errorf("reference files after a failed Publish = %q, want %q", got, want)
	}
}

func TestPublishMovesPrunesAndWritesSourceLast(t *testing.T) {
	tests := []struct {
		name  string
		prune bool
		want  []string
	}{
		{name: "repin removes scenarios that no longer exist", prune: true, want: []string{"SOURCE", "a.json"}},
		{name: "without repin other references stay", prune: false, want: []string{"SOURCE", "a.json", "gone.json"}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			staging, references := t.TempDir(), t.TempDir()
			writeFile(t, filepath.Join(references, "SOURCE"), "commit: old\nrelease: serverless-init 1.7.8\n")
			writeFile(t, filepath.Join(references, "gone.json"), "old")
			writeFile(t, filepath.Join(staging, "a.json"), "new a")
			if err := Publish(staging, references, []string{"a"}, test.prune, Source{Commit: "new", GoBinarySHA256: "abc"}); err != nil {
				t.Fatal(err)
			}
			files := referenceFiles(t, references)
			if got := slices.Sorted(maps.Keys(files)); !slices.Equal(got, test.want) {
				t.Errorf("reference files = %q, want %q", got, test.want)
			}
			if files["a.json"] != "new a" {
				t.Errorf("a.json = %q, want the staged content", files["a.json"])
			}
			source, err := ReadSource(filepath.Join(references, "SOURCE"))
			want := Source{Commit: "new", Release: "serverless-init 1.7.8", GoBinarySHA256: "abc"}
			if err != nil || source != want {
				t.Errorf("SOURCE = %+v, %v, want %+v", source, err, want)
			}
		})
	}
}

func TestMeasureResources(t *testing.T) {
	started := time.Date(2026, 11, 22, 10, 0, 0, 0, time.UTC)
	ready := events.Event{Time: started.Add(1500 * time.Millisecond), Kind: "receiver_ready"}
	other := events.Event{Time: started.Add(100 * time.Millisecond), Kind: "started"}
	tests := []struct {
		name    string
		capture capture.Capture
		want    Resources
	}{
		{
			name: "receiver ready and memory sampled",
			capture: capture.Capture{
				Outcome: runner.Outcome{StartedAt: started, PeakMemoryBytes: 2048, PeakMemoryKnown: true},
				Events:  []events.Event{other, ready},
			},
			want: Resources{Startup: timing.Measurement{Measurable: true, Value: timing.Duration(1500 * time.Millisecond)}, PeakMemoryBytes: 2048, PeakMemoryKnown: true},
		},
		{
			name:    "no receiver ready event and no memory sample",
			capture: capture.Capture{Outcome: runner.Outcome{StartedAt: started}, Events: []events.Event{other}},
			want:    Resources{},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := MeasureResources(test.capture); got != test.want {
				t.Errorf("MeasureResources = %+v, want %+v", got, test.want)
			}
		})
	}
}

// A reference is compared only with the inputs and the Go binary it was recorded with.
func TestCheckReference(t *testing.T) {
	inputs := map[string]string{"compare-defaults.yaml": "c1", "normalize.yaml": "n1", "platforms/local.env": "p1", "scenarios/logs.yaml": "s1"}
	source := Source{Commit: "abc", GoBinarySHA256: "go1"}
	changed := func(key, value string) map[string]string {
		out := maps.Clone(inputs)
		out[key] = value
		return out
	}
	for _, tc := range []struct {
		name      string
		reference Reference
		current   map[string]string
		wantErr   string
	}{
		{"current", Reference{Scenario: "logs", GoBinarySHA256: "go1", Inputs: inputs}, inputs, ""},
		{"normalize.yaml changed", Reference{Scenario: "logs", GoBinarySHA256: "go1", Inputs: inputs}, changed("normalize.yaml", "n2"),
			"the reference of logs is stale: normalize.yaml changed since it was recorded; run parity record --scenario logs"},
		{"the scenario changed", Reference{Scenario: "logs", GoBinarySHA256: "go1", Inputs: inputs}, changed("scenarios/logs.yaml", "s2"),
			"the reference of logs is stale: scenarios/logs.yaml changed since it was recorded; run parity record --scenario logs"},
		{"recorded before inputs were stored", Reference{Scenario: "logs", GoBinarySHA256: "go1"}, inputs,
			"the reference of logs is stale: compare-defaults.yaml changed since it was recorded; run parity record --scenario logs"},
		{"another Go binary", Reference{Scenario: "logs", GoBinarySHA256: "go0", Inputs: inputs}, inputs,
			"the reference of logs was recorded with the Go binary go0, but reference/SOURCE names go1; run parity record --scenario logs"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			err := Check(tc.reference, source, tc.current)
			if (err == nil) != (tc.wantErr == "") || (err != nil && err.Error() != tc.wantErr) {
				t.Fatalf("got %v, want %q", err, tc.wantErr)
			}
		})
	}
}

func TestHashInputs(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "normalize.yaml")
	if err := os.WriteFile(path, []byte("rules: []\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	got, err := HashInputs(map[string]string{"normalize.yaml": path})
	if err != nil {
		t.Fatal(err)
	}
	// The SHA-256 of "rules: []\n".
	want := map[string]string{"normalize.yaml": "e0dfa70eb69d47fe9cb2be8a4fcd53e74cf7fe26fcbb1f06912b57a9c028e4e0"}
	if _, err := HashInputs(map[string]string{"missing": filepath.Join(dir, "missing")}); err == nil {
		t.Error("HashInputs of a missing file succeeded")
	}
	if !maps.Equal(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

// Record refuses a Go binary that is not the one the references pin, because the references would
// then describe another build.
func TestCheckBuild(t *testing.T) {
	pinned := Source{Commit: "abc", GoBinarySHA256: "go1"}
	clean := build.Info{AgentPath: "/agent", Commit: "abc"}
	for _, tc := range []struct {
		name    string
		info    build.Info
		binary  string
		commit  string
		repin   bool
		wantErr string
	}{
		{"the pinned binary", clean, "go1", "abc", false, ""},
		{"another binary of the pinned commit", clean, "go2", "abc", false,
			"the Go binary's SHA-256 is go2, but reference/SOURCE pins go1; build the pinned binary, or run parity record --repin"},
		{"built from another commit", build.Info{AgentPath: "/agent", Commit: "def"}, "go1", "abc", false,
			"the Go binary was built from datadog-agent def, not from abc; build it again with parity build go"},
		{"built from a checkout with uncommitted changes", build.Info{AgentPath: "/agent", Commit: "abc", Dirty: true}, "go1", "abc", false,
			"the Go binary was built from a datadog-agent checkout with uncommitted changes; build it again from a clean checkout with parity build go"},
		{"repin accepts a new binary", clean, "go2", "abc", true, ""},
		{"repin still needs the binary of the commit", build.Info{AgentPath: "/agent", Commit: "def"}, "go2", "abc", true,
			"the Go binary was built from datadog-agent def, not from abc; build it again with parity build go"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			err := CheckBuild(tc.info, tc.binary, pinned, tc.commit, tc.repin)
			if (err == nil) != (tc.wantErr == "") || (err != nil && err.Error() != tc.wantErr) {
				t.Fatalf("got %v, want %q", err, tc.wantErr)
			}
		})
	}
}

func TestSourceRoundTripsTheFakeintakeCommit(t *testing.T) {
	path := filepath.Join(t.TempDir(), "SOURCE")
	want := Source{Commit: "abc", Release: "serverless-init 1.12.0", GoBinarySHA256: "go1", FakeintakeCommit: "def"}
	if err := WriteSource(path, want); err != nil {
		t.Fatal(err)
	}
	if got, err := ReadSource(path); err != nil || got != want {
		t.Fatalf("got %+v, %v, want %+v", got, err, want)
	}
}
