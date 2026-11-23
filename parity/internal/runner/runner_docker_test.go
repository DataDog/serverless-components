//go:build docker

package runner

import (
	"context"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/scenario"
)

// exitScenario is a minimal in-container scenario in which the child exits with code.
func exitScenario(code int) scenario.Scenario {
	return scenario.Scenario{
		Name: "process.exit-code", Priority: "P0", Mode: "in-container", Platform: "local",
		Workload: []scenario.Step{{Kind: "exit", Args: map[string]any{"value": code}}},
	}
}

// dockerRun runs s against out/bin/<label>/datadog-init, skipping when the binary is not built.
// The run directory is under parity/out, which Docker can mount from the host.
func dockerRun(t *testing.T, label string, s scenario.Scenario, opts RunOptions) Outcome {
	t.Helper()
	parityDir, err := filepath.Abs("../..")
	if err != nil {
		t.Fatal(err)
	}
	binary := filepath.Join(parityDir, "out/bin", label, "datadog-init")
	if _, err := os.Stat(binary); err != nil {
		t.Skipf("the %s binary %s is not built; run parity build %s first", label, binary, label)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()
	if err := Preflight(ctx); err != nil {
		t.Fatal(err)
	}
	runDir := filepath.Join(parityDir, "out/test", t.Name(), label)
	out, err := RunScenario(ctx, s, binary, runDir, opts)
	if err != nil {
		t.Fatalf("RunScenario: %v\nstdout:\n%s\nstderr:\n%s", err, out.Stdout, out.Stderr)
	}
	t.Logf("exit code %d, peak memory %d bytes (known %v), ran %v, clock offset %v\nstdout:\n%s\nstderr:\n%s",
		out.ExitCode, out.PeakMemoryBytes, out.PeakMemoryKnown, out.ExitedAt.Sub(out.StartedAt), out.ClockOffset, out.Stdout, out.Stderr)
	return out
}

func TestRunExitCodesAgainstGo(t *testing.T) {
	out := dockerRun(t, "go", exitScenario(3), RunOptions{})
	if out.ExitCode != 3 {
		t.Errorf("ExitCode = %d, want 3", out.ExitCode)
	}
}

func TestRunSmokeAgainstEmptyRust(t *testing.T) {
	out := dockerRun(t, "rust", exitScenario(3), RunOptions{BeforeTeardown: func(o Outcome) error {
		response, err := http.Get(o.FakeintakeURL + "/fakeintake/health")
		if err != nil {
			return err
		}
		defer response.Body.Close()
		if response.StatusCode != http.StatusOK {
			return fmt.Errorf("fakeintake health returned %d", response.StatusCode)
		}
		return nil
	}})
	// The placeholder exits 0 whatever its arguments.
	if out.ExitCode != 0 {
		t.Errorf("ExitCode = %d, want 0", out.ExitCode)
	}
}
