package runner

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
)

// fakeDocker answers docker commands from a handler and records every call.
type fakeDocker struct {
	mu      sync.Mutex
	calls   [][]string
	handler func(ctx context.Context, args []string) (Result, error)
}

func (f *fakeDocker) Run(ctx context.Context, args ...string) (Result, error) {
	f.mu.Lock()
	f.calls = append(f.calls, args)
	f.mu.Unlock()
	return f.handler(ctx, args)
}

func (f *fakeDocker) called(words ...string) bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	for _, call := range f.calls {
		if strings.Contains(strings.Join(call, " "), strings.Join(words, " ")) {
			return true
		}
	}
	return false
}

// paritySkeleton creates the parity directory layout that RunScenario expects and returns the
// path of the binary under test.
func paritySkeleton(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	for _, file := range []string{"out/bin/rust/datadog-init", "out/bin/workload/parity-workload", "platforms/local.env"} {
		path := filepath.Join(dir, file)
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, nil, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	return filepath.Join(dir, "out/bin/rust/datadog-init")
}

var stopScenario = scenario.Scenario{
	Name: "process.ignores-sigterm", Priority: "P0", Mode: "in-container", Platform: "local",
	Workload: []scenario.Step{{Kind: "wait_for_signal", Args: map[string]any{"signal": "SIGTERM"}}},
	Stop:     scenario.Stop{Signal: "SIGTERM", AfterSeconds: 0.05},
}

func TestRunnerKillsAgentThatIgnoresSigterm(t *testing.T) {
	// The Docker clock runs one hour ahead of the host clock.
	vmStart := time.Now().Add(time.Hour).UTC().Truncate(time.Millisecond)
	killed := make(chan struct{})
	var once sync.Once
	docker := &fakeDocker{handler: func(ctx context.Context, args []string) (Result, error) {
		joined := strings.Join(args, " ")
		switch {
		case strings.Contains(joined, "ps -a -q agent"):
			return Result{Stdout: "agent-id\n"}, nil
		case strings.Contains(joined, "ps -a -q fakeintake"):
			return Result{Stdout: "fakeintake-id\n"}, nil
		case strings.Contains(joined, "port fakeintake 80"):
			return Result{Stdout: "127.0.0.1:49153\n"}, nil
		case args[0] == "exec":
			return Result{Stdout: fmt.Sprintf("%d\n", time.Now().Add(time.Hour).UnixNano())}, nil
		case args[0] == "inspect":
			return Result{Stdout: vmStart.Format(time.RFC3339Nano) + " " + vmStart.Add(2*time.Second).Format(time.RFC3339Nano) + "\n"}, nil
		case args[0] == "stats":
			return Result{Stdout: "12MiB / 1GiB\n"}, nil
		case joined == "kill -s SIGKILL agent-id":
			once.Do(func() { close(killed) })
		case args[0] == "wait":
			// The agent ignores SIGTERM: it exits only when killed.
			select {
			case <-killed:
				return Result{Stdout: "137\n"}, nil
			case <-ctx.Done():
				return Result{}, ctx.Err()
			}
		}
		return Result{}, nil
	}}
	timing := timings{sigtermGrace: 100 * time.Millisecond, killWait: time.Second, sampleEvery: 10 * time.Millisecond}

	var beforeTeardown Outcome
	start := time.Now()
	out, err := runScenario(context.Background(), docker, timing, stopScenario, paritySkeleton(t), t.TempDir(),
		RunOptions{BeforeTeardown: func(o Outcome) error { beforeTeardown = o; return nil }})
	if err != nil {
		t.Fatal(err)
	}
	if elapsed := time.Since(start); elapsed > 5*time.Second {
		t.Errorf("runScenario took %v", elapsed)
	}
	if !out.Killed || out.ExitCode != 137 {
		t.Errorf("Killed = %v, ExitCode = %d, want true and 137", out.Killed, out.ExitCode)
	}
	if want := "the agent did not exit within 0.1 seconds after the stop signal"; out.NotExited != want {
		t.Errorf("NotExited = %q, want %q", out.NotExited, want)
	}
	if !out.StartedAt.Equal(vmStart) || !out.ExitedAt.Equal(vmStart.Add(2*time.Second)) {
		t.Errorf("StartedAt = %v, ExitedAt = %v, want the inspected %v and two seconds later", out.StartedAt, out.ExitedAt, vmStart)
	}
	if out.ClockOffset < time.Hour-time.Second || out.ClockOffset > time.Hour+time.Second {
		t.Errorf("ClockOffset = %v, want about one hour", out.ClockOffset)
	}
	if out.SigtermAt.Before(start) || out.SigtermAt.After(time.Now()) {
		t.Errorf("SigtermAt = %v is not a host time during the run", out.SigtermAt)
	}
	if !out.PeakMemoryKnown || out.PeakMemoryBytes != 12*1024*1024 {
		t.Errorf("PeakMemoryKnown = %v, PeakMemoryBytes = %d, want true and %d", out.PeakMemoryKnown, out.PeakMemoryBytes, 12*1024*1024)
	}
	if out.FakeintakeURL != "http://127.0.0.1:49153" {
		t.Errorf("FakeintakeURL = %q", out.FakeintakeURL)
	}
	if !beforeTeardown.Killed {
		t.Error("BeforeTeardown did not receive the final outcome")
	}
	for _, words := range [][]string{{"kill", "-s", "SIGTERM", "agent-id"}, {"kill", "-s", "SIGKILL", "agent-id"}, {"down", "-v"}} {
		if !docker.called(words...) {
			t.Errorf("no docker call contains %q", strings.Join(words, " "))
		}
	}
}

func TestSampleMemoryOverlapsSlowSamples(t *testing.T) {
	var mu sync.Mutex
	inFlight, maxInFlight, calls := 0, 0, 0
	docker := &fakeDocker{handler: func(ctx context.Context, args []string) (Result, error) {
		mu.Lock()
		inFlight++
		calls++
		maxInFlight = max(maxInFlight, inFlight)
		usage := fmt.Sprintf("%dMiB / 1GiB", calls)
		mu.Unlock()
		// Each sample takes far longer than the interval, like docker stats --no-stream.
		select {
		case <-time.After(100 * time.Millisecond):
		case <-ctx.Done():
		}
		mu.Lock()
		inFlight--
		mu.Unlock()
		return Result{Stdout: usage}, nil
	}}
	ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
	defer cancel()
	peak, known := sampleMemory(ctx, docker, "agent-id", 10*time.Millisecond)
	if maxInFlight < 2 || maxInFlight > maxSamplesInFlight {
		t.Errorf("%d samples ran at once, want between 2 and %d", maxInFlight, maxSamplesInFlight)
	}
	if !known || peak < 2*1024*1024 {
		t.Errorf("peak = %d, known = %v, want at least 2 MiB and known", peak, known)
	}

	// Before and after the container runs, docker stats fails or reports zero.
	var failingCalls atomic.Int32
	failing := &fakeDocker{handler: func(ctx context.Context, args []string) (Result, error) {
		if failingCalls.Add(1)%2 == 0 {
			return Result{Stdout: "0B / 0B"}, nil
		}
		return Result{}, errors.New("container is not running")
	}}
	shortCtx, cancelShort := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancelShort()
	if peak, known := sampleMemory(shortCtx, failing, "agent-id", 10*time.Millisecond); known || peak != 0 {
		t.Errorf("with no successful sample, peak = %d, known = %v, want 0 and false", peak, known)
	}
}

func TestRunnerRemovesLeftoverProjects(t *testing.T) {
	runDir := t.TempDir()
	// The run id is older than the scenario timeout, but the invocation started this scenario a
	// minute ago, so its containers are young and it is still running.
	current := "parity-" + time.Now().Add(-time.Hour).Format("20060102-150405") + "-1-logs-go"
	old := "parity-" + time.Now().Add(-time.Hour).Format("20060102-150405") + "-1-traces-go"
	docker := &fakeDocker{handler: func(ctx context.Context, args []string) (Result, error) {
		joined := strings.Join(args, " ")
		switch {
		case slices.Contains(args, "ls"):
			return Result{Stdout: `[{"Name":"parity-traces-basic-go","Status":"running(2)"},{"Name":"other-project","Status":"running(1)"},` +
				`{"Name":"` + current + `","Status":"running(2)","ConfigFiles":"` + runDir + `/compose.yaml"},` +
				`{"Name":"` + old + `","Status":"exited(2)","ConfigFiles":"` + runDir + `/compose.yaml"}]`}, nil
		case joined == "ps -a --filter label=com.docker.compose.project="+current+" --format {{.CreatedAt}}":
			return Result{Stdout: time.Now().Add(-time.Minute).Format("2006-01-02 15:04:05 -0700 MST") + "\n"}, nil
		case joined == "ps -a --filter label=com.docker.compose.project="+old+" --format {{.CreatedAt}}":
			created := time.Now().Add(-time.Hour)
			return Result{Stdout: created.Format("2006-01-02 15:04:05 -0700 MST") + "\n" + created.Add(time.Second).Format("2006-01-02 15:04:05 -0700 MST") + "\n"}, nil
		}
		return Result{}, nil
	}}
	if err := preflight(context.Background(), docker, 5*time.Minute); err != nil {
		t.Fatal(err)
	}
	for _, project := range []string{"parity-traces-basic-go", old} {
		if !docker.called("compose", "-p", project, "down", "-v") {
			t.Errorf("the leftover project %s was not removed", project)
		}
	}
	if docker.called("-p", "other-project") || docker.called("-p", current) {
		t.Error("a project of another tool or of a running invocation was removed")
	}
}

// Preflight removes a parity project only when no invocation can still be using it: its run
// directory is gone, its newest container is older than the scenario timeout, it has no
// container, or its name has no run id, which only projects from before run ids have.
func TestLeftoverProject(t *testing.T) {
	now := time.Date(2026, 11, 23, 12, 0, 0, 0, time.Local)
	existing := "/parity/out/20261123-115800-1/logs.stdout-capture/go/compose.yaml"
	exists := func(dir string) bool { return dir == filepath.Dir(existing) }
	recent, old := now.Add(-time.Minute), now.Add(-6*time.Minute)
	for _, tc := range []struct {
		name, project, configFiles string
		created                    time.Time
		want                       bool
	}{
		{"another tool's project", "other-project", existing, old, false},
		{"a running invocation", "parity-20261123-115800-1-logs-stdout-capture-go", existing, recent, false},
		{"a running invocation whose run id is older than the scenario timeout", "parity-20261123-115400-1-logs-stdout-capture-go", existing, recent, false},
		{"containers older than the scenario timeout", "parity-20261123-115800-1-logs-stdout-capture-go", existing, old, true},
		{"no container", "parity-20261123-115800-1-logs-stdout-capture-go", existing, time.Time{}, true},
		{"run directory removed", "parity-20261123-115800-1-logs-stdout-capture-go", "/parity/out/gone/compose.yaml", recent, true},
		{"no config file listed", "parity-20261123-115800-1-logs-stdout-capture-go", "", recent, true},
		{"a name without a run id", "parity-logs-stdout-capture-go", existing, recent, true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got := leftoverProject(composeProject{Name: tc.project, ConfigFiles: tc.configFiles}, tc.created, now, 5*time.Minute, exists)
			if got != tc.want {
				t.Fatalf("got %v, want %v", got, tc.want)
			}
		})
	}
}

func TestRunnerFailsFastWithoutDocker(t *testing.T) {
	docker := &fakeDocker{handler: func(ctx context.Context, args []string) (Result, error) {
		return Result{ExitCode: 1, Stderr: "Cannot connect to the Docker daemon"}, errors.New("exit status 1")
	}}
	start := time.Now()
	err := preflight(context.Background(), docker, time.Minute)
	if err == nil || !strings.Contains(err.Error(), "Docker") {
		t.Errorf("preflight error = %v, want an error naming Docker", err)
	}
	if len(docker.calls) != 1 {
		t.Errorf("preflight ran %d docker commands after Docker failed, want 1", len(docker.calls))
	}
	if _, err := runScenario(context.Background(), docker, defaultTimings, stopScenario, paritySkeleton(t), t.TempDir(), RunOptions{}); err == nil {
		t.Error("runScenario succeeded without Docker")
	}
	if !docker.called("down", "-v") {
		t.Error("runScenario did not try to tear down the project after up failed")
	}
	if elapsed := time.Since(start); elapsed > 2*time.Second {
		t.Errorf("failing took %v, want under 2 seconds", elapsed)
	}
}

func TestParseMemoryUsage(t *testing.T) {
	tests := []struct {
		input   string
		want    uint64
		wantErr bool
	}{
		{input: "0B / 0B", want: 0},
		{input: "512KiB / 1GiB", want: 524288},
		{input: "1.5MiB / 7.66GiB", want: 1572864},
		{input: "2GiB / 4GiB", want: 2147483648},
		{input: "10.2MB / 1GB", want: 10200000},
		{input: "3kB / 1GB", want: 3000},
		{input: "-- / --", wantErr: true},
		{input: "", wantErr: true},
	}
	for _, tt := range tests {
		got, err := parseMemoryUsage(tt.input)
		if (err != nil) != tt.wantErr {
			t.Errorf("parseMemoryUsage(%q) error = %v, want error %v", tt.input, err, tt.wantErr)
			continue
		}
		if got != tt.want {
			t.Errorf("parseMemoryUsage(%q) = %d, want %d", tt.input, got, tt.want)
		}
	}
}

func TestParseInspectTimes(t *testing.T) {
	tests := []struct {
		input             string
		started, finished time.Time
		wantErr           bool
	}{
		{
			input:    "2026-11-23T00:48:43.748862719Z 2026-11-23T00:48:50.916102374Z\n",
			started:  time.Date(2026, 11, 23, 0, 48, 43, 748862719, time.UTC),
			finished: time.Date(2026, 11, 23, 0, 48, 50, 916102374, time.UTC),
		},
		{
			// A running container has the zero finish time.
			input:   "2026-11-23T00:48:43Z 0001-01-01T00:00:00Z",
			started: time.Date(2026, 11, 23, 0, 48, 43, 0, time.UTC),
		},
		{input: "2026-11-23T00:48:43Z", wantErr: true},
		{input: "yesterday today", wantErr: true},
	}
	for _, tt := range tests {
		started, finished, err := parseInspectTimes(tt.input)
		if (err != nil) != tt.wantErr {
			t.Errorf("parseInspectTimes(%q) error = %v, want error %v", tt.input, err, tt.wantErr)
			continue
		}
		if !started.Equal(tt.started) || !finished.Equal(tt.finished) {
			t.Errorf("parseInspectTimes(%q) = %v, %v, want %v, %v", tt.input, started, finished, tt.started, tt.finished)
		}
	}
}

func TestClockOffset(t *testing.T) {
	before := time.Unix(1000, 0)
	tests := []struct {
		name    string
		after   time.Time
		output  string
		want    time.Duration
		wantErr bool
	}{
		{name: "same clock", after: time.Unix(1000, 200_000_000), output: "1000100000000\n", want: 0},
		{name: "docker ahead", after: time.Unix(1000, 200_000_000), output: "1002600000000", want: 2500 * time.Millisecond},
		{name: "docker behind", after: time.Unix(1001, 0), output: "999000000000", want: -1500 * time.Millisecond},
		{name: "not a number", after: time.Unix(1001, 0), output: "1000.5", wantErr: true},
	}
	for _, tt := range tests {
		got, err := clockOffset(before, tt.after, tt.output)
		if (err != nil) != tt.wantErr {
			t.Errorf("%s: error = %v, want error %v", tt.name, err, tt.wantErr)
			continue
		}
		if got != tt.want {
			t.Errorf("%s: clockOffset = %v, want %v", tt.name, got, tt.want)
		}
	}
}

// In sidecar mode Docker cannot start the workload when the agent has already exited, because
// the workload joins the agent's network namespace. That is the binary's result, not a harness
// error: the run goes on without the workload.
func TestSidecarAgentThatExitsAtOnce(t *testing.T) {
	vmStart := time.Now().UTC().Truncate(time.Millisecond)
	for _, tc := range []struct {
		name, agentState string
		wantErr          bool
	}{
		{"agent exited", "exited", false},
		{"agent running", "running", true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			docker := &fakeDocker{handler: func(ctx context.Context, args []string) (Result, error) {
				joined := strings.Join(args, " ")
				switch {
				case strings.HasSuffix(joined, "up -d"):
					return Result{}, errors.New("cannot join network namespace of a non running container")
				case strings.Contains(joined, "ps -a -q agent"):
					return Result{Stdout: "agent-id\n"}, nil
				case strings.Contains(joined, "ps -a -q fakeintake"):
					return Result{Stdout: "fakeintake-id\n"}, nil
				case strings.Contains(joined, "ps -a -q workload"):
					return Result{Stdout: "workload-id\n"}, nil
				case strings.Contains(joined, "port fakeintake 80"):
					return Result{Stdout: "127.0.0.1:49153\n"}, nil
				case args[0] == "exec":
					return Result{Stdout: fmt.Sprintf("%d\n", time.Now().UnixNano())}, nil
				case joined == "inspect --format {{.State.Status}} agent-id":
					return Result{Stdout: tc.agentState + "\n"}, nil
				case args[0] == "inspect":
					return Result{Stdout: vmStart.Format(time.RFC3339Nano) + " " + vmStart.Add(time.Second).Format(time.RFC3339Nano) + "\n"}, nil
				case args[0] == "wait":
					return Result{Stdout: "0\n"}, nil
				}
				return Result{}, nil
			}}
			s := scenario.Scenario{Name: "process.sidecar-exits", Priority: "P0", Mode: "sidecar", Platform: "local",
				Stop: scenario.Stop{Signal: "SIGTERM", AfterSeconds: 5}}
			timing := timings{sigtermGrace: 100 * time.Millisecond, killWait: time.Second, sampleEvery: 10 * time.Millisecond}
			out, err := runScenario(context.Background(), docker, timing, s, paritySkeleton(t), t.TempDir(), RunOptions{})
			if tc.wantErr {
				if err == nil {
					t.Fatal("a failed start with the agent still running gave no error")
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if out.ExitCode != 0 || !out.SigtermAt.IsZero() {
				t.Errorf("ExitCode = %d, SigtermAt = %v, want 0 and no SIGTERM", out.ExitCode, out.SigtermAt)
			}
			if docker.called("logs", "workload-id") || docker.called("kill") {
				t.Error("the runner read the logs of, or signaled, a workload that never started")
			}
		})
	}
}

// The stop signal is timed from the workload's step clock, which the event log records on the
// Docker clock, so that it keeps its distance to the last send whatever the binary's startup
// time. Without that event it is timed from the container start, plus the longest wait for an
// aligned step clock.
func TestStopTime(t *testing.T) {
	containerStart := time.Date(2026, 11, 22, 10, 0, 0, 0, time.UTC)
	offset := time.Hour // the Docker clock runs one hour ahead
	after, slack := 25*time.Second, 10*time.Second
	for _, tc := range []struct {
		name         string
		events       []events.Event
		want         time.Time
		wantAnchored bool
	}{
		{"no events", nil, containerStart.Add(after + slack), false},
		{"started only", []events.Event{{Kind: "started", Time: containerStart.Add(offset + time.Second)}}, containerStart.Add(after + slack), false},
		{"clock started", []events.Event{
			{Kind: "started", Time: containerStart.Add(offset + time.Second)},
			{Kind: "clock_started", Time: containerStart.Add(offset + 7*time.Second)},
		}, containerStart.Add(7*time.Second + after), true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, anchored := stopTime(containerStart, tc.events, offset, after, slack)
			if !got.Equal(tc.want) || anchored != tc.wantAnchored {
				t.Fatalf("got %v, %v, want %v, %v", got, anchored, tc.want, tc.wantAnchored)
			}
		})
	}
}

// With align, the runner starts fakeintake first, then the agent alignLead before an aligned
// moment of the Docker clock, which the workload's step clock uses, so that the agent's flush
// ticks keep the same phase against the wall clock.
func TestRunnerAlignsTheAgentStart(t *testing.T) {
	align := scenario.Align{Every: 300 * time.Millisecond, Offset: 100 * time.Millisecond}
	lead := 50 * time.Millisecond
	dockerAhead := 130 * time.Millisecond
	var mu sync.Mutex
	var upAt []time.Time
	var upArgs []string
	docker := &fakeDocker{handler: func(ctx context.Context, args []string) (Result, error) {
		joined := strings.Join(args, " ")
		switch {
		case strings.Contains(joined, " up -d"):
			mu.Lock()
			upAt, upArgs = append(upAt, time.Now()), append(upArgs, joined[strings.Index(joined, " up -d")+1:])
			mu.Unlock()
		case strings.Contains(joined, "ps -a -q"):
			return Result{Stdout: "id\n"}, nil
		case strings.Contains(joined, "port fakeintake 80"):
			return Result{Stdout: "127.0.0.1:49153\n"}, nil
		case args[0] == "exec":
			return Result{Stdout: fmt.Sprintf("%d\n", time.Now().Add(dockerAhead).UnixNano())}, nil
		case args[0] == "inspect":
			now := time.Now().UTC().Format(time.RFC3339Nano)
			return Result{Stdout: now + " " + now + "\n"}, nil
		case args[0] == "wait":
			return Result{Stdout: "0\n"}, nil
		}
		return Result{}, nil
	}}
	s := scenario.Scenario{Name: "metrics.aligned", Priority: "P0", Mode: "in-container", Platform: "local", Align: align}
	timing := timings{sigtermGrace: 100 * time.Millisecond, killWait: time.Second, sampleEvery: 10 * time.Millisecond, alignLead: lead}
	if _, err := runScenario(context.Background(), docker, timing, s, paritySkeleton(t), t.TempDir(), RunOptions{}); err != nil {
		t.Fatal(err)
	}
	if want := []string{"up -d --wait fakeintake", "up -d"}; !slices.Equal(upArgs, want) {
		t.Fatalf("up calls %q, want %q", upArgs, want)
	}
	phase := time.Duration(upAt[1].Add(dockerAhead+lead).UnixNano() % int64(align.Every))
	if phase < align.Offset || phase > align.Offset+20*time.Millisecond {
		t.Fatalf("the agent started at phase %v of its alignment on the Docker clock, want just after %v", phase, align.Offset)
	}
}

func TestAlignedStart(t *testing.T) {
	align := scenario.Align{Every: 10 * time.Second, Offset: 500 * time.Millisecond}
	hostNow := time.Date(2026, 11, 23, 10, 0, 3, 0, time.UTC)
	for _, tc := range []struct {
		name   string
		offset time.Duration
		want   time.Time
	}{
		{"same clocks", 0, time.Date(2026, 11, 23, 10, 0, 9, 500_000_000, time.UTC)},
		{"Docker clock 2 seconds ahead", 2 * time.Second, time.Date(2026, 11, 23, 10, 0, 7, 500_000_000, time.UTC)},
		{"Docker clock 7 seconds behind", -7 * time.Second, time.Date(2026, 11, 23, 10, 0, 6, 500_000_000, time.UTC)},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := alignedStart(align, hostNow, tc.offset, time.Second); !got.Equal(tc.want) {
				t.Fatalf("got %v, want %v", got, tc.want)
			}
		})
	}
}

// The workload's step clock must start at the aligned moment that the runner chose. An agent
// container that starts after that moment, or a workload that waits for a later one, puts the run
// at another phase and would shift every measure.
func TestAlignmentError(t *testing.T) {
	align := scenario.Align{Every: 10 * time.Second, Offset: 500 * time.Millisecond}
	intended := time.Date(2026, 11, 23, 10, 0, 10, 500_000_000, time.UTC)
	onTime := time.Date(2026, 11, 23, 10, 0, 9, 400_000_000, time.UTC)
	clockStarted := func(at time.Time) []events.Event {
		return []events.Event{{Kind: "started", Time: onTime}, {Kind: "clock_started", Time: at}}
	}
	for _, tc := range []struct {
		name           string
		containerStart time.Time
		logged         []events.Event
		wantErr        string
	}{
		{"the intended aligned moment", onTime, clockStarted(intended), ""},
		{"no clock_started event", onTime, nil, ""},
		{"the next aligned moment", onTime, clockStarted(intended.Add(10 * time.Second)),
			"the workload's step clock started at 2026-11-23T10:00:20.5Z, after 2026-11-23T10:00:10.5Z, the aligned moment it was to start at; the run is not at its aligned phase"},
		{"a late container start", time.Date(2026, 11, 23, 10, 0, 10, 700_000_000, time.UTC), clockStarted(intended.Add(10 * time.Second)),
			"the agent container started at 2026-11-23T10:00:10.7Z, 200ms after 2026-11-23T10:00:10.5Z, the aligned moment the workload's step clock was to start at; the run is not at its aligned phase"},
		{"a late container start without a workload", time.Date(2026, 11, 23, 10, 0, 11, 0, time.UTC), nil,
			"the agent container started at 2026-11-23T10:00:11Z, 500ms after 2026-11-23T10:00:10.5Z, the aligned moment the workload's step clock was to start at; the run is not at its aligned phase"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			err := alignmentError(align, intended, tc.containerStart, tc.logged)
			if (err == nil) != (tc.wantErr == "") || (err != nil && err.Error() != tc.wantErr) {
				t.Fatalf("got %v, want %q", err, tc.wantErr)
			}
		})
	}
}

// A container that Docker reports as started after the aligned moment the runner chose makes the
// run a harness error, even though the workload then waits for the next aligned moment.
func TestRunnerReportsALateAgentStart(t *testing.T) {
	align := scenario.Align{Every: 300 * time.Millisecond, Offset: 100 * time.Millisecond}
	late := 200 * time.Millisecond
	docker := &fakeDocker{handler: func(ctx context.Context, args []string) (Result, error) {
		joined := strings.Join(args, " ")
		switch {
		case strings.Contains(joined, "ps -a -q"):
			return Result{Stdout: "id\n"}, nil
		case strings.Contains(joined, "port fakeintake 80"):
			return Result{Stdout: "127.0.0.1:49153\n"}, nil
		case args[0] == "exec":
			return Result{Stdout: fmt.Sprintf("%d\n", time.Now().UnixNano())}, nil
		case args[0] == "inspect":
			// The agent was to start 50 milliseconds before the aligned moment.
			started := time.Now().Add(50*time.Millisecond + late).UTC().Format(time.RFC3339Nano)
			return Result{Stdout: started + " " + started + "\n"}, nil
		case args[0] == "wait":
			return Result{Stdout: "0\n"}, nil
		}
		return Result{}, nil
	}}
	s := scenario.Scenario{Name: "metrics.aligned", Priority: "P0", Mode: "in-container", Platform: "local", Align: align}
	timing := timings{sigtermGrace: 100 * time.Millisecond, killWait: time.Second, sampleEvery: 10 * time.Millisecond, alignLead: 50 * time.Millisecond}
	_, err := runScenario(context.Background(), docker, timing, s, paritySkeleton(t), t.TempDir(), RunOptions{})
	if err == nil || !strings.Contains(err.Error(), "the aligned moment the workload's step clock was to start at") {
		t.Fatalf("got %v, want an error naming the late agent container start", err)
	}
}

func TestWorkloadEnd(t *testing.T) {
	for _, tc := range []struct {
		name  string
		steps []scenario.Step
		want  time.Duration
	}{
		{"no steps", nil, 0},
		{"the latest step", []scenario.Step{{At: 3 * time.Second, Kind: "print"}, {At: 7 * time.Second, Kind: "exit"}}, 7 * time.Second},
		{"a sleep extends the end", []scenario.Step{{At: 2 * time.Second, Kind: "sleep", Args: map[string]any{"value": "4s"}}, {At: 5 * time.Second, Kind: "exit"}}, 6 * time.Second},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := workloadEnd(tc.steps); got != tc.want {
				t.Fatalf("got %v, want %v", got, tc.want)
			}
		})
	}
}

// An agent that outlives its workload is stopped as the stop path does, and the run is captured
// with the difference instead of failing.
func TestRunnerStopsAnAgentThatOutlivesTheWorkload(t *testing.T) {
	terminated := make(chan struct{})
	var once sync.Once
	docker := &fakeDocker{handler: func(ctx context.Context, args []string) (Result, error) {
		joined := strings.Join(args, " ")
		switch {
		case strings.Contains(joined, "ps -a -q"):
			return Result{Stdout: "agent-id\n"}, nil
		case strings.Contains(joined, "port fakeintake 80"):
			return Result{Stdout: "127.0.0.1:49153\n"}, nil
		case args[0] == "exec":
			return Result{Stdout: fmt.Sprintf("%d\n", time.Now().UnixNano())}, nil
		case args[0] == "inspect":
			now := time.Now().UTC().Format(time.RFC3339Nano)
			return Result{Stdout: now + " " + now + "\n"}, nil
		case joined == "kill -s SIGTERM agent-id":
			once.Do(func() { close(terminated) })
		case args[0] == "wait":
			select {
			case <-terminated:
				return Result{Stdout: "143\n"}, nil
			case <-ctx.Done():
				return Result{}, ctx.Err()
			}
		}
		return Result{}, nil
	}}
	s := scenario.Scenario{Name: "process.hangs", Priority: "P0", Mode: "in-container", Platform: "local",
		Workload: []scenario.Step{{At: 50 * time.Millisecond, Kind: "exit", Args: map[string]any{"value": 0}}}}
	timing := timings{sigtermGrace: time.Second, killWait: time.Second, sampleEvery: 10 * time.Millisecond, workloadGrace: 100 * time.Millisecond}
	captured := false
	out, err := runScenario(context.Background(), docker, timing, s, paritySkeleton(t), t.TempDir(),
		RunOptions{BeforeTeardown: func(Outcome) error { captured = true; return nil }})
	if err != nil {
		t.Fatal(err)
	}
	if want := "the agent did not exit within 0.1 seconds after the workload finished"; out.NotExited != want {
		t.Errorf("NotExited = %q, want %q", out.NotExited, want)
	}
	if out.Killed || out.ExitCode != 143 || !out.SigtermAt.IsZero() || !captured {
		t.Errorf("Killed = %v, ExitCode = %d, SigtermAt = %v, captured = %v; want false, 143, the zero time and true", out.Killed, out.ExitCode, out.SigtermAt, captured)
	}
}
