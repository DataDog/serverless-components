// Package runner runs one scenario against one binary in its own Docker Compose project.
package runner

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"gopkg.in/yaml.v3"

	"github.com/DataDog/serverless-components/parity/internal/compose"
	"github.com/DataDog/serverless-components/parity/internal/events"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
)

// Outcome is what one scenario run produced.
type Outcome struct {
	ExitCode       int
	Stdout, Stderr string
	// StartedAt and ExitedAt are the agent container's start and finish times from docker
	// inspect. They are on the Docker clock, which fakeintake and the workload driver also use.
	StartedAt, ExitedAt time.Time
	// SigtermAt is the host time just before the stop signal was sent. SigtermAt.Add(ClockOffset)
	// is the same moment on the Docker clock.
	SigtermAt time.Time
	// StopSignal is the signal sent at SigtermAt.
	StopSignal string
	// ClockOffset is the Docker clock minus the host clock, measured once during the run.
	ClockOffset time.Duration
	// Killed is true when the agent did not exit within the grace period after a SIGTERM or the
	// stop signal, and was killed with SIGKILL.
	Killed bool
	// NotExited says how long the agent outlived its deadline, when it did: the end of the
	// workload plus a grace period, or the stop signal plus the grace period. It is empty when the
	// agent exited in time.
	NotExited string
	// PeakMemoryBytes is the highest sampled memory usage. PeakMemoryKnown is false when no sample
	// succeeded, for example when the agent exits before docker stats answers.
	PeakMemoryBytes uint64
	PeakMemoryKnown bool
	EventLogPath    string
	// FakeintakeURL is the host address of fakeintake, valid until the project is torn down.
	FakeintakeURL string
}

// RunOptions holds the run id and optional hooks for RunScenario.
type RunOptions struct {
	// RunID names the invocation, so that its Compose projects do not collide with another's.
	RunID string
	// BeforeTeardown runs after the agent exits and before the project is torn down.
	BeforeTeardown func(o Outcome) error
}

// Result is the output of one docker command.
type Result struct {
	Stdout, Stderr string
	ExitCode       int
}

// Docker runs docker with arguments. It returns an error when the command cannot start or exits
// with a non-zero status.
type Docker interface {
	Run(ctx context.Context, args ...string) (Result, error)
}

// ExecDocker runs the docker command line. Commands starting with "compose" use the standalone
// docker-compose binary when the Compose plugin is not installed.
type ExecDocker struct {
	once          sync.Once
	composePrefix []string
}

// Run runs the command and captures its output.
func (d *ExecDocker) Run(ctx context.Context, args ...string) (Result, error) {
	name := "docker"
	if len(args) > 0 && args[0] == "compose" {
		d.once.Do(func() {
			d.composePrefix = []string{"docker", "compose"}
			if exec.CommandContext(ctx, "docker", "compose", "version").Run() != nil {
				if _, err := exec.LookPath("docker-compose"); err == nil {
					d.composePrefix = []string{"docker-compose"}
				}
			}
		})
		name, args = d.composePrefix[0], append(d.composePrefix[1:len(d.composePrefix):len(d.composePrefix)], args[1:]...)
	}
	cmd := exec.CommandContext(ctx, name, args...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err := cmd.Run()
	result := Result{Stdout: stdout.String(), Stderr: stderr.String(), ExitCode: cmd.ProcessState.ExitCode()}
	if err != nil {
		return result, fmt.Errorf("%s %s: %w: %s", name, strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	}
	return result, nil
}

var defaultDocker Docker = &ExecDocker{}

type timings struct {
	// sigtermGrace is how long the agent may take to exit after the stop signal.
	sigtermGrace time.Duration
	// killWait is how long the agent may take to exit after SIGKILL.
	killWait time.Duration
	// sampleEvery is the memory sampling interval.
	sampleEvery time.Duration
	// alignLead is how long before an aligned moment the agent container starts in a scenario
	// with align: enough for the container and the workload to start, so that the workload's
	// step clock starts at that moment.
	alignLead time.Duration
	// workloadGrace is how long after its last step the agent of a scenario without a stop
	// signal may run before the runner stops it.
	workloadGrace time.Duration
}

var defaultTimings = timings{sigtermGrace: 15 * time.Second, killWait: 15 * time.Second, sampleEvery: 250 * time.Millisecond, alignLead: time.Second, workloadGrace: 20 * time.Second}

// Preflight checks that Docker is running and removes the Compose projects that earlier runs left,
// as leftoverProject decides. maxAge is the longest a scenario run can take.
func Preflight(ctx context.Context, maxAge time.Duration) error {
	return preflight(ctx, defaultDocker, maxAge)
}

// composeProject is one entry of docker compose ls.
type composeProject struct {
	Name string
	// ConfigFiles lists the project's Compose files, separated by commas.
	ConfigFiles string
}

// runIDTime is the layout of the time that starts a run id.
const runIDTime = "20060102-150405"

// leftoverProject reports whether p is a parity project that no invocation can still be using:
// its run directory no longer exists, its name has no run id, which only projects from before run
// ids have, it has no container, or its newest container, created at newestContainer, was created
// more than maxAge before now. An invocation can last longer than maxAge, so the run id's time
// does not say how old a project is. exists reports whether a directory exists.
func leftoverProject(p composeProject, newestContainer, now time.Time, maxAge time.Duration, exists func(string) bool) bool {
	rest, ok := strings.CutPrefix(p.Name, "parity-")
	if !ok {
		return false
	}
	configFile, _, _ := strings.Cut(p.ConfigFiles, ",")
	if configFile == "" || !exists(filepath.Dir(configFile)) {
		return true
	}
	if len(rest) < len(runIDTime) {
		return true
	}
	if _, err := time.Parse(runIDTime, rest[:len(runIDTime)]); err != nil {
		return true
	}
	return newestContainer.IsZero() || now.Sub(newestContainer) > maxAge
}

// newestContainer returns when the newest container of the Compose project was created, or the
// zero time when the project has no container.
func newestContainer(ctx context.Context, d Docker, project string) (time.Time, error) {
	result, err := d.Run(ctx, "ps", "-a", "--filter", "label=com.docker.compose.project="+project, "--format", "{{.CreatedAt}}")
	if err != nil {
		return time.Time{}, err
	}
	var newest time.Time
	for _, line := range strings.Split(strings.TrimSpace(result.Stdout), "\n") {
		// docker ps prints, for example, 2026-11-23 12:00:00 +0100 CET; the zone name is left out,
		// because the offset says the same.
		fields := strings.Fields(line)
		if len(fields) == 0 {
			continue
		}
		if len(fields) < 3 {
			return time.Time{}, fmt.Errorf("reading the creation time %q of a container of %s", line, project)
		}
		created, err := time.Parse("2006-01-02 15:04:05 -0700", strings.Join(fields[:3], " "))
		if err != nil {
			return time.Time{}, fmt.Errorf("reading the creation time of a container of %s: %w", project, err)
		}
		if created.After(newest) {
			newest = created
		}
	}
	return newest, nil
}

func preflight(ctx context.Context, d Docker, maxAge time.Duration) error {
	if _, err := d.Run(ctx, "info", "--format", "{{.ServerVersion}}"); err != nil {
		return fmt.Errorf("cannot reach Docker; start Docker and try again: %w", err)
	}
	result, err := d.Run(ctx, "compose", "ls", "--all", "--format", "json")
	if err != nil {
		return fmt.Errorf("listing Docker Compose projects: %w", err)
	}
	var projects []composeProject
	if err := json.Unmarshal([]byte(strings.TrimSpace(result.Stdout)), &projects); err != nil {
		return fmt.Errorf("reading the Docker Compose project list: %w", err)
	}
	exists := func(dir string) bool {
		info, err := os.Stat(dir)
		return err == nil && info.IsDir()
	}
	var errs []error
	for _, p := range projects {
		if !strings.HasPrefix(p.Name, "parity-") {
			continue
		}
		created, err := newestContainer(ctx, d, p.Name)
		if err != nil {
			errs = append(errs, fmt.Errorf("finding the age of the project %s: %w", p.Name, err))
			continue
		}
		if leftoverProject(p, created, time.Now(), maxAge, exists) {
			if _, err := d.Run(ctx, "compose", "-p", p.Name, "down", "-v", "--remove-orphans", "--timeout", "0"); err != nil {
				errs = append(errs, fmt.Errorf("removing the leftover project %s: %w", p.Name, err))
			}
		}
	}
	return errors.Join(errs...)
}

// RunScenario runs s against binary, which must be at <parity>/out/bin/<label>/datadog-init. The
// workload driver is read from <parity>/out/bin/workload/parity-workload, next to the fake Google
// Cloud metadata server that Google Cloud platforms need, and the platform
// variables from <parity>/platforms/<platform>.env. The Compose file, the scenario copy and the
// event log are written under runDir.
func RunScenario(ctx context.Context, s scenario.Scenario, binary string, runDir string, opts RunOptions) (Outcome, error) {
	return runScenario(ctx, defaultDocker, defaultTimings, s, binary, runDir, opts)
}

func runScenario(ctx context.Context, d Docker, t timings, s scenario.Scenario, binary, runDir string, opts RunOptions) (out Outcome, err error) {
	composeFile, project, err := prepare(s, binary, runDir, opts.RunID)
	if err != nil {
		return Outcome{}, err
	}
	runDir, _ = filepath.Abs(runDir)
	out.EventLogPath = filepath.Join(compose.SharedHostDir(runDir), compose.EventLogFile)
	run := func(ctx context.Context, args ...string) (Result, error) {
		return d.Run(ctx, append([]string{"compose", "-p", project, "-f", composeFile}, args...)...)
	}
	defer func() {
		// Tear down even when ctx is cancelled.
		downCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), time.Minute)
		defer cancel()
		if _, downErr := run(downCtx, "down", "-v", "--remove-orphans", "--timeout", "0"); downErr != nil {
			err = errors.Join(err, fmt.Errorf("tearing down %s: %w", project, downErr))
		}
	}()

	// In sidecar mode the workload joins the agent's network namespace, so Docker cannot start it
	// when the agent has already exited. That is the binary's result, and the run goes on
	// without the workload.
	workloadStarted := s.Mode == "sidecar"
	offsetMeasured := false
	// intendedMoment is the aligned moment of the Docker clock at which the workload's step clock
	// is to start, in a scenario with align.
	var intendedMoment time.Time
	if s.Align != (scenario.Align{}) {
		// Start fakeintake first, so that the agent starts at the aligned time, and read the
		// Docker clock in it, which the workload's step clock uses.
		if _, err := run(ctx, "up", "-d", "--wait", "fakeintake"); err != nil {
			return out, fmt.Errorf("starting fakeintake of %s: %w", project, err)
		}
		fakeintakeID, err := containerID(ctx, run, "fakeintake")
		if err != nil {
			return out, err
		}
		if out.ClockOffset, err = measureClockOffset(ctx, d, fakeintakeID); err != nil {
			return out, err
		}
		offsetMeasured = true
		startAt := alignedStart(s.Align, time.Now(), out.ClockOffset, t.alignLead)
		intendedMoment = startAt.Add(out.ClockOffset + t.alignLead)
		select {
		case <-time.After(time.Until(startAt)):
		case <-ctx.Done():
			return out, ctx.Err()
		}
	}
	if _, err := run(ctx, "up", "-d"); err != nil {
		if s.Mode != "sidecar" || !agentExited(ctx, d, run) {
			return out, fmt.Errorf("starting %s: %w", project, err)
		}
		workloadStarted = false
	}
	agentID, err := containerID(ctx, run, "agent")
	if err != nil {
		return out, err
	}
	fakeintakeID, err := containerID(ctx, run, "fakeintake")
	if err != nil {
		return out, err
	}
	signalTargets := []string{agentID}
	if workloadStarted {
		workloadID, err := containerID(ctx, run, "workload")
		if err != nil {
			return out, err
		}
		signalTargets = append(signalTargets, workloadID)
	}
	result, err := run(ctx, "port", "fakeintake", "80")
	if err != nil {
		return out, fmt.Errorf("finding the fakeintake port of %s: %w", project, err)
	}
	out.FakeintakeURL = "http://" + strings.TrimSpace(result.Stdout)
	if !offsetMeasured {
		if out.ClockOffset, err = measureClockOffset(ctx, d, fakeintakeID); err != nil {
			return out, err
		}
	}
	if out.StartedAt, _, err = inspectTimes(ctx, d, agentID); err != nil {
		return out, err
	}

	sampleCtx, stopSampling := context.WithCancel(ctx)
	type memory struct {
		peak  uint64
		known bool
	}
	sampled := make(chan memory, 1)
	go func() {
		peak, known := sampleMemory(sampleCtx, d, agentID, t.sampleEvery)
		sampled <- memory{peak, known}
	}()
	waitErr := waitForExit(ctx, d, t, s, signalTargets, out.StartedAt.Add(-out.ClockOffset), &out)
	stopSampling()
	m := <-sampled
	out.PeakMemoryBytes, out.PeakMemoryKnown = m.peak, m.known
	if waitErr != nil {
		return out, waitErr
	}
	if out.StartedAt, out.ExitedAt, err = inspectTimes(ctx, d, agentID); err != nil {
		return out, err
	}

	logs, err := d.Run(ctx, "logs", agentID)
	if err != nil {
		return out, fmt.Errorf("reading the agent logs: %w", err)
	}
	out.Stdout, out.Stderr = logs.Stdout, logs.Stderr
	if workloadStarted {
		// The child runs in the workload container; its output follows the agent's.
		logs, err := d.Run(ctx, "logs", signalTargets[1])
		if err != nil {
			return out, fmt.Errorf("reading the workload logs: %w", err)
		}
		out.Stdout += logs.Stdout
		out.Stderr += logs.Stderr
	}
	var alignErr error
	if s.Align != (scenario.Align{}) {
		logged, _ := events.Read(out.EventLogPath)
		alignErr = alignmentError(s.Align, intendedMoment, out.StartedAt, logged)
	}
	if opts.BeforeTeardown != nil {
		if err := opts.BeforeTeardown(out); err != nil {
			return out, errors.Join(err, alignErr)
		}
	}
	return out, alignErr
}

// alignedStart returns, on the host clock, when to start the agent so that it starts lead before
// an aligned moment of the Docker clock, which is offset ahead of the host clock.
func alignedStart(a scenario.Align, hostNow time.Time, offset, lead time.Duration) time.Time {
	return a.Next(hostNow.Add(offset + lead)).Add(-lead - offset)
}

// alignmentError returns an error when the run is not at the phase that intended, the aligned
// moment the runner chose for the workload's step clock, sets: when the agent container started
// after intended, or when the clock_started event shows that the workload's step clock started
// after it, because the workload then waited for a later aligned moment. All times are on the
// Docker clock.
func alignmentError(a scenario.Align, intended, containerStart time.Time, logged []events.Event) error {
	if containerStart.After(intended) {
		return fmt.Errorf("the agent container started at %s, %s after %s, the aligned moment the workload's step clock was to start at; the run is not at its aligned phase",
			containerStart.UTC().Format(time.RFC3339Nano), containerStart.Sub(intended), intended.UTC().Format(time.RFC3339Nano))
	}
	for _, e := range logged {
		if e.Kind == "clock_started" && e.Time.After(intended) {
			return fmt.Errorf("the workload's step clock started at %s, after %s, the aligned moment it was to start at; the run is not at its aligned phase",
				e.Time.UTC().Format(time.RFC3339Nano), intended.UTC().Format(time.RFC3339Nano))
		}
	}
	return nil
}

// workloadEnd returns when the steps end, measured from the start of the step clock: each step
// runs at its At or after the step before it, and a sleep step lasts its duration.
func workloadEnd(steps []scenario.Step) time.Duration {
	var end time.Duration
	for _, step := range steps {
		end = max(end, step.At)
		if step.Kind == "sleep" {
			text, _ := step.Args["value"].(string)
			d, _ := time.ParseDuration(text)
			end += d
		}
	}
	return end
}

// seconds formats a duration as a number of seconds, for example "20 seconds".
func seconds(d time.Duration) string {
	return strconv.FormatFloat(d.Seconds(), 'f', -1, 64) + " seconds"
}

// prepare writes the scenario copy and the Compose file, and returns the Compose file path and the
// project name.
func prepare(s scenario.Scenario, binary, runDir, runID string) (composeFile, project string, err error) {
	binary, err = filepath.Abs(binary)
	if err != nil {
		return "", "", err
	}
	runDir, err = filepath.Abs(runDir)
	if err != nil {
		return "", "", err
	}
	parityDir := filepath.Dir(filepath.Dir(filepath.Dir(filepath.Dir(binary))))
	workloadBinary := filepath.Join(parityDir, "out/bin/workload/parity-workload")
	required := []string{binary, workloadBinary}
	if compose.GoogleCloudPlatform(s.Platform) {
		required = append(required, compose.MetadataBinary(workloadBinary))
	}
	// A missing file would make Docker mount an empty directory in its place.
	for _, path := range required {
		if info, err := os.Stat(path); err != nil || info.IsDir() {
			return "", "", fmt.Errorf("binary %s is missing; build it with parity build", path)
		}
	}
	platformEnv, err := compose.LoadPlatform(filepath.Join(parityDir, "platforms"), s.Platform)
	if err != nil {
		return "", "", err
	}
	shared := compose.SharedHostDir(runDir)
	if err := os.RemoveAll(shared); err != nil {
		return "", "", err
	}
	if err := os.MkdirAll(shared, 0o755); err != nil {
		return "", "", err
	}
	scenarioData, err := yaml.Marshal(s)
	if err != nil {
		return "", "", err
	}
	if err := os.WriteFile(compose.ScenarioFile(s, runDir), scenarioData, 0o644); err != nil {
		return "", "", err
	}
	project = compose.ProjectName(runID, s.Name, compose.BinaryLabel(binary))
	composeData, err := compose.Generate(s, project, binary, workloadBinary, runDir, platformEnv)
	if err != nil {
		return "", "", err
	}
	composeFile = filepath.Join(runDir, "compose.yaml")
	if err := os.WriteFile(composeFile, composeData, 0o644); err != nil {
		return "", "", err
	}
	return composeFile, project, nil
}

// agentExited reports whether the agent container exists and has exited.
func agentExited(ctx context.Context, d Docker, run func(context.Context, ...string) (Result, error)) bool {
	id, err := containerID(ctx, run, "agent")
	if err != nil {
		return false
	}
	result, err := d.Run(ctx, "inspect", "--format", "{{.State.Status}}", id)
	return err == nil && strings.TrimSpace(result.Stdout) == "exited"
}

// containerID returns the ID of the service's container, running or exited.
func containerID(ctx context.Context, run func(context.Context, ...string) (Result, error), service string) (string, error) {
	result, err := run(ctx, "ps", "-a", "-q", service)
	id := strings.TrimSpace(result.Stdout)
	if err != nil || id == "" {
		return "", fmt.Errorf("finding the %s container: %w", service, err)
	}
	return id, nil
}

// measureClockOffset reads the Docker clock inside the fakeintake container and compares it with
// the host time halfway through the call.
func measureClockOffset(ctx context.Context, d Docker, fakeintakeID string) (time.Duration, error) {
	before := time.Now()
	result, err := d.Run(ctx, "exec", fakeintakeID, "/parity-workload", "--print-clock")
	after := time.Now()
	if err != nil {
		return 0, fmt.Errorf("reading the Docker clock: %w", err)
	}
	return clockOffset(before, after, result.Stdout)
}

// clockOffset returns the Docker time in output, in Unix nanoseconds, minus the host time halfway
// between before and after.
func clockOffset(before, after time.Time, output string) (time.Duration, error) {
	nanoseconds, err := strconv.ParseInt(strings.TrimSpace(output), 10, 64)
	if err != nil {
		return 0, fmt.Errorf("the Docker clock reading %q is not a number of nanoseconds", output)
	}
	return time.Unix(0, nanoseconds).Sub(before.Add(after.Sub(before) / 2)), nil
}

func inspectTimes(ctx context.Context, d Docker, id string) (started, finished time.Time, err error) {
	result, err := d.Run(ctx, "inspect", "--format", "{{.State.StartedAt}} {{.State.FinishedAt}}", id)
	if err != nil {
		return time.Time{}, time.Time{}, fmt.Errorf("inspecting the agent container: %w", err)
	}
	return parseInspectTimes(result.Stdout)
}

// parseInspectTimes reads "<StartedAt> <FinishedAt>" from docker inspect. A container that has not
// finished has the zero finish time.
func parseInspectTimes(output string) (started, finished time.Time, err error) {
	fields := strings.Fields(output)
	if len(fields) != 2 {
		return time.Time{}, time.Time{}, fmt.Errorf("expected a start and a finish time from docker inspect, found %q", output)
	}
	if started, err = time.Parse(time.RFC3339Nano, fields[0]); err != nil {
		return time.Time{}, time.Time{}, fmt.Errorf("reading the container start time: %w", err)
	}
	if finished, err = time.Parse(time.RFC3339Nano, fields[1]); err != nil {
		return time.Time{}, time.Time{}, fmt.Errorf("reading the container finish time: %w", err)
	}
	return started, finished, nil
}

// stopPollInterval is how often the runner reads the event log while it waits to send the stop
// signal.
const stopPollInterval = 10 * time.Millisecond

// stopTime returns when to send the stop signal, on the host clock: after past the start of the
// workload's step clock, which the event log records on the Docker clock, so that the signal
// keeps its distance to the last step whatever the binary's startup time. Until the log has that
// event, it is after plus slack, the longest wait for an aligned step clock, past the container
// start, and anchored is false.
func stopTime(containerStart time.Time, logged []events.Event, clockOffset, after, slack time.Duration) (at time.Time, anchored bool) {
	for _, e := range logged {
		if e.Kind == "clock_started" {
			return e.Time.Add(-clockOffset).Add(after), true
		}
	}
	return containerStart.Add(after + slack), false
}

type exitResult struct {
	code int
	err  error
}

// waitForExit waits for the agent to exit. It sends the scenario's stop signal to signalTargets
// at the time stopTime gives, and SIGKILL to the agent when it outlives the grace period. A
// scenario without a stop signal has a deadline instead, workloadGrace after its last step: an
// agent that outlives it gets SIGTERM, then SIGKILL after the grace period, and NotExited says so.
func waitForExit(ctx context.Context, d Docker, t timings, s scenario.Scenario, signalTargets []string, hostStart time.Time, out *Outcome) error {
	agentID := signalTargets[0]
	waitCtx, cancelWait := context.WithCancel(ctx)
	defer cancelWait()
	exited := make(chan exitResult, 1)
	go func() {
		result, err := d.Run(waitCtx, "wait", agentID)
		if err != nil {
			exited <- exitResult{err: fmt.Errorf("waiting for the agent container: %w", err)}
			return
		}
		code, err := strconv.Atoi(strings.TrimSpace(result.Stdout))
		exited <- exitResult{code: code, err: err}
	}()
	finish := func(r exitResult) error {
		out.ExitCode = r.code
		return r.err
	}

	stopping := s.Stop.Signal != ""
	after, signal := workloadEnd(s.Workload)+t.workloadGrace, "SIGTERM"
	if stopping {
		after, signal = time.Duration(s.Stop.AfterSeconds*float64(time.Second)), s.Stop.Signal
	}
	poll := time.NewTicker(stopPollInterval)
	defer poll.Stop()
	// Once the event log gives the step clock start, a later read that fails, for example on a
	// line being written, must not fall back to the container start.
	var stopAt time.Time
	anchored := false
	for waiting := true; waiting; {
		select {
		case r := <-exited:
			return finish(r)
		case <-ctx.Done():
			return ctx.Err()
		case <-poll.C:
			if !anchored {
				logged, _ := events.Read(out.EventLogPath)
				stopAt, anchored = stopTime(hostStart, logged, out.ClockOffset, after, s.Align.Every)
			}
			waiting = time.Now().Before(stopAt)
		}
	}

	if stopping {
		out.SigtermAt, out.StopSignal = time.Now(), signal
	} else {
		out.NotExited = "the agent did not exit within " + seconds(t.workloadGrace) + " after the workload finished"
	}
	if _, err := d.Run(ctx, append([]string{"kill", "-s", signal}, signalTargets...)...); err != nil {
		out.SigtermAt = time.Time{}
		// The agent may have exited at the moment the signal was due.
		select {
		case r := <-exited:
			out.NotExited = ""
			return finish(r)
		case <-time.After(time.Second):
			return fmt.Errorf("sending %s to the agent: %w", signal, err)
		}
	}
	select {
	case r := <-exited:
		return finish(r)
	case <-ctx.Done():
		return ctx.Err()
	case <-time.After(t.sigtermGrace):
	}

	out.Killed = true
	if stopping {
		out.NotExited = "the agent did not exit within " + seconds(t.sigtermGrace) + " after the stop signal"
	}
	if _, err := d.Run(ctx, "kill", "-s", "SIGKILL", agentID); err != nil {
		return fmt.Errorf("killing the agent after %v: %w", t.sigtermGrace, err)
	}
	select {
	case r := <-exited:
		return finish(r)
	case <-ctx.Done():
		return ctx.Err()
	case <-time.After(t.killWait):
		return fmt.Errorf("the agent container did not exit within %v of SIGKILL", t.killWait)
	}
}

const (
	// maxSamplesInFlight bounds the docker stats commands running at once; a tick is skipped when
	// all are busy. docker stats --no-stream takes one to two seconds, so eight cover 250 milliseconds.
	maxSamplesInFlight = 8
	sampleTimeout      = 5 * time.Second
)

// sampleMemory starts a docker stats sample on every tick until ctx is done, waits for the samples
// in flight, and returns the highest usage seen and whether any sample returned a usage.
func sampleMemory(ctx context.Context, d Docker, containerID string, every time.Duration) (uint64, bool) {
	var (
		mu    sync.Mutex
		peak  uint64
		known bool
		wg    sync.WaitGroup
	)
	slots := make(chan struct{}, maxSamplesInFlight)
	sample := func() {
		defer wg.Done()
		defer func() { <-slots }()
		// A sample in flight when the agent exits may still hold the peak, so it is not cancelled with ctx.
		sampleCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), sampleTimeout)
		defer cancel()
		// Errors are expected once the container exits.
		result, err := d.Run(sampleCtx, "stats", "--no-stream", "--format", "{{.MemUsage}}", containerID)
		if err != nil {
			return
		}
		usage, err := parseMemoryUsage(strings.TrimSpace(result.Stdout))
		// A container that is not running reports zero; a running process always uses memory.
		if err != nil || usage == 0 {
			return
		}
		mu.Lock()
		peak, known = max(peak, usage), true
		mu.Unlock()
	}
	ticker := time.NewTicker(every)
	defer ticker.Stop()
	for {
		select {
		case slots <- struct{}{}:
			wg.Add(1)
			go sample()
		default:
		}
		select {
		case <-ctx.Done():
			wg.Wait()
			return peak, known
		case <-ticker.C:
		}
	}
}

var memoryUnits = map[string]float64{
	"B": 1, "KiB": 1 << 10, "MiB": 1 << 20, "GiB": 1 << 30, "TiB": 1 << 40,
	"kB": 1e3, "KB": 1e3, "MB": 1e6, "GB": 1e9, "TB": 1e12,
}

// parseMemoryUsage reads the usage half of docker stats' MemUsage column, for example
// "1.5MiB / 7.66GiB", in bytes.
func parseMemoryUsage(s string) (uint64, error) {
	usage, _, _ := strings.Cut(s, " / ")
	usage = strings.TrimSpace(usage)
	number := strings.TrimRight(usage, "kKMGTiB")
	value, err := strconv.ParseFloat(number, 64)
	if err != nil {
		return 0, fmt.Errorf("memory usage %q has no number", s)
	}
	unit, ok := memoryUnits[usage[len(number):]]
	if !ok {
		return 0, fmt.Errorf("memory usage %q has an unknown unit", s)
	}
	return uint64(value * unit), nil
}
