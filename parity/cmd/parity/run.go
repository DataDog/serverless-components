package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io/fs"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/DataDog/serverless-components/parity/internal/build"
	"github.com/DataDog/serverless-components/parity/internal/capture"
	"github.com/DataDog/serverless-components/parity/internal/compare"
	"github.com/DataDog/serverless-components/parity/internal/compose"
	"github.com/DataDog/serverless-components/parity/internal/normalize"
	"github.com/DataDog/serverless-components/parity/internal/reference"
	"github.com/DataDog/serverless-components/parity/internal/report"
	"github.com/DataDog/serverless-components/parity/internal/runner"
	"github.com/DataDog/serverless-components/parity/internal/scenario"
	"github.com/DataDog/serverless-components/parity/internal/timing"
)

// scenarioTimeout bounds one scenario run, including Docker start and teardown.
const scenarioTimeout = 5 * time.Minute

// stringList is a flag that can be given more than once.
type stringList []string

func (l *stringList) String() string     { return strings.Join(*l, ",") }
func (l *stringList) Set(v string) error { *l = append(*l, v); return nil }

// common holds the flags and settings that run, record and stability share.
type common struct {
	cfg          build.Config
	scenariosDir string
	names        stringList
	parallel     int
}

func (c *common) register(flags *flag.FlagSet) {
	flags.StringVar(&c.cfg.ComponentsDir, "components-dir", c.cfg.ComponentsDir, "serverless-components checkout")
	flags.StringVar(&c.scenariosDir, "scenarios-dir", "", "directory of scenario files (default <components-dir>/parity/scenarios)")
	flags.Var(&c.names, "scenario", "scenario to run; give it more than once for several (default every scenario)")
	flags.IntVar(&c.parallel, "parallel", 4, "how many scenarios whose timing is not compared run at once")
}

// harness is the loaded configuration of one invocation.
type harness struct {
	parityDir    string
	referenceDir string
	expectedPath string
	scenariosDir string
	// defaults are the comparator overrides of compare-defaults.yaml, for every scenario.
	defaults   []scenario.Override
	rules      []normalize.Rule
	tolerances timing.Tolerances
	scenarios  []scenario.Scenario
	// binary is the absolute path of the binary under test.
	binary string
	// outDir is parity/out/<run id>.
	outDir string
}

// setUp resolves the paths and the binary, loads the rules and scenarios, checks the platforms
// and the expected differences file, and removes Compose projects left by earlier runs.
func setUp(ctx context.Context, c *common, binary string) (*harness, error) {
	cfg, err := c.cfg.Resolve()
	if err != nil {
		return nil, err
	}
	c.cfg = cfg
	if c.parallel < 1 {
		return nil, fmt.Errorf("--parallel is %d; it must be at least 1", c.parallel)
	}
	h := &harness{parityDir: filepath.Join(cfg.ComponentsDir, "parity")}
	h.referenceDir = filepath.Join(h.parityDir, "reference")
	h.expectedPath = filepath.Join(h.parityDir, "expected-differences.yaml")
	if h.binary, err = resolveBinary(h.parityDir, binary); err != nil {
		return nil, err
	}
	if h.rules, h.tolerances, err = normalize.LoadRules(filepath.Join(h.parityDir, "normalize.yaml")); err != nil {
		return nil, err
	}
	if _, err := compare.ApplyExpected(nil, "", h.expectedPath); err != nil {
		return nil, err
	}
	if h.defaults, err = compare.LoadDefaults(filepath.Join(h.parityDir, "compare-defaults.yaml")); err != nil {
		return nil, err
	}
	h.scenariosDir = c.scenariosDir
	if h.scenariosDir == "" {
		h.scenariosDir = filepath.Join(h.parityDir, "scenarios")
	}
	if h.scenarios, err = selectScenarios(h.scenariosDir, c.names); err != nil {
		return nil, err
	}
	if err := scenario.ValidatePlatforms(h.scenarios, filepath.Join(h.parityDir, "platforms")); err != nil {
		return nil, err
	}
	if err := runner.Preflight(ctx, scenarioTimeout); err != nil {
		return nil, err
	}
	out := filepath.Join(h.parityDir, "out")
	if err := os.MkdirAll(out, 0o755); err != nil {
		return nil, err
	}
	// The random suffix keeps two invocations in the same second apart.
	h.outDir, err = os.MkdirTemp(out, time.Now().Format("20060102-150405")+"-")
	return h, err
}

// selectScenarios loads the scenarios in dir, keeping only the named ones, each once, when names
// is not empty.
func selectScenarios(dir string, names []string) ([]scenario.Scenario, error) {
	all, err := scenario.LoadDir(dir)
	if err != nil {
		return nil, err
	}
	if len(names) == 0 {
		if len(all) == 0 {
			return nil, fmt.Errorf("there are no scenario files in %s", dir)
		}
		return all, nil
	}
	var selected []scenario.Scenario
	for _, name := range names {
		if slices.ContainsFunc(selected, func(s scenario.Scenario) bool { return s.Name == name }) {
			continue
		}
		i := slices.IndexFunc(all, func(s scenario.Scenario) bool { return s.Name == name })
		if i < 0 {
			return nil, fmt.Errorf("there is no scenario named %q in %s", name, dir)
		}
		selected = append(selected, all[i])
	}
	return selected, nil
}

// resolveBinary returns the absolute path of the binary for go, rust or a path, and checks that
// it exists. The runner finds the workload driver and the platform files relative to the binary,
// so a path must be <parity>/out/bin/<label>/datadog-init.
func resolveBinary(parityDir, binary string) (string, error) {
	if binary == "go" || binary == "rust" {
		binary = filepath.Join(parityDir, "out/bin", binary, "datadog-init")
	}
	binary, err := filepath.Abs(binary)
	if err != nil {
		return "", err
	}
	if filepath.Base(binary) != "datadog-init" || filepath.Dir(filepath.Dir(filepath.Dir(binary))) != filepath.Join(parityDir, "out") {
		return "", fmt.Errorf("the binary %s must be at %s/out/bin/<label>/datadog-init", binary, parityDir)
	}
	if info, err := os.Stat(binary); err != nil || info.IsDir() {
		return "", fmt.Errorf("the binary %s does not exist; build it with parity build go or parity build rust", binary)
	}
	return binary, nil
}

// runsAlone reports whether s runs alone, so that other scenarios do not load the machine and make
// its timing noisy: it sets align or a stop signal, or its reference has a measurable flush
// interval or shutdown. ref may be nil.
func runsAlone(s scenario.Scenario, ref *reference.Reference, tol timing.Tolerances) bool {
	if s.Align != (scenario.Align{}) || s.Stop.Signal != "" {
		return true
	}
	if ref == nil {
		return false
	}
	m := timing.Measure(ref.Normalized.Timing, tol)
	measurable := func(byArea map[string]timing.Measurement) bool {
		for _, measurement := range byArea {
			if measurement.Measurable {
				return true
			}
		}
		return false
	}
	return m.Shutdown.Measurable || measurable(m.IntervalByArea) || measurable(m.FlushIntervalByArea)
}

// loadReferences loads the reference of every scenario. A scenario without one has nil, and one
// whose reference can't be read has the error.
func (h *harness) loadReferences() ([]*reference.Reference, []error) {
	refs, errs := make([]*reference.Reference, len(h.scenarios)), make([]error, len(h.scenarios))
	for i, s := range h.scenarios {
		refs[i], errs[i] = reference.Load(h.referenceDir, s.Name)
	}
	return refs, errs
}

// schedule calls job for every scenario: those that don't run alone up to parallel at a time, then
// those that do one at a time. refs holds the reference of each scenario, or nil.
func (h *harness) schedule(refs []*reference.Reference, parallel int, job func(i int)) {
	var alone []int
	slots := make(chan struct{}, parallel)
	var wg sync.WaitGroup
	for i, s := range h.scenarios {
		if runsAlone(s, refs[i], h.tolerances) {
			alone = append(alone, i)
			continue
		}
		wg.Add(1)
		slots <- struct{}{}
		go func() {
			defer wg.Done()
			defer func() { <-slots }()
			job(i)
		}()
	}
	wg.Wait()
	for _, i := range alone {
		job(i)
	}
}

// observation is what one scenario run against one binary produced.
type observation struct {
	normalized normalize.Normalized
	measures   timing.Measures
	resources  reference.Resources
	// killed is true when the agent was killed with SIGKILL.
	killed bool
}

// observe runs s against the binary in runDir, collects the capture before teardown, and writes
// capture.raw.json and normalized.json to runDir.
func (h *harness) observe(ctx context.Context, s scenario.Scenario, runDir string) (observation, error) {
	ctx, cancel := context.WithTimeout(ctx, scenarioTimeout)
	defer cancel()
	var c capture.Capture
	_, err := runner.RunScenario(ctx, s, h.binary, runDir, runner.RunOptions{RunID: filepath.Base(h.outDir), BeforeTeardown: func(o runner.Outcome) error {
		var err error
		if c, err = capture.Collect(o.FakeintakeURL, o); err != nil {
			return fmt.Errorf("collecting the capture: %w", err)
		}
		c.Scenario, c.Binary = s.Name, h.binary
		return c.WriteJSON(filepath.Join(runDir, "capture.raw.json"))
	}})
	if err != nil {
		return observation{}, err
	}
	n, err := normalize.Apply(c, h.rules)
	if err != nil {
		return observation{}, fmt.Errorf("normalizing the capture: %w", err)
	}
	if err := writeJSON(filepath.Join(runDir, "normalized.json"), n); err != nil {
		return observation{}, err
	}
	return observation{normalized: n, measures: timing.Measure(n.Timing, h.tolerances), resources: reference.MeasureResources(c), killed: c.Outcome.Killed}, nil
}

// inputs returns the hashes of the files that s's reference depends on, by their names relative
// to the parity directory.
func (h *harness) inputs(s scenario.Scenario) (map[string]string, error) {
	return reference.HashInputs(map[string]string{
		"scenarios/" + s.Name + ".yaml":    filepath.Join(h.scenariosDir, s.Name+".yaml"),
		"platforms/" + s.Platform + ".env": filepath.Join(h.parityDir, "platforms", s.Platform+".env"),
		"normalize.yaml":                   filepath.Join(h.parityDir, "normalize.yaml"),
		"compare-defaults.yaml":            filepath.Join(h.parityDir, "compare-defaults.yaml"),
	})
}

func writeJSON(path string, v any) error {
	data, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(data, '\n'), 0o644)
}

// signalContext is cancelled on an interrupt, so that the running scenarios tear down.
func signalContext() (context.Context, context.CancelFunc) {
	return signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
}

func runCommand(cfg build.Config, args []string) error {
	c := common{cfg: cfg}
	flags := flag.NewFlagSet("run", flag.ExitOnError)
	c.register(flags)
	binaryFlag := flags.String("binary", "rust", "binary to compare with the Go references: go, rust or a path to <parity>/out/bin/<label>/datadog-init")
	flags.Parse(args)
	ctx, stop := signalContext()
	defer stop()
	h, err := setUp(ctx, &c, *binaryFlag)
	if err != nil {
		return err
	}
	label := compose.BinaryLabel(h.binary)
	results := make([]report.Result, len(h.scenarios))
	refs, refErrs := h.loadReferences()
	source, sourceErr := reference.ReadSource(filepath.Join(h.referenceDir, "SOURCE"))
	h.schedule(refs, c.parallel, func(i int) {
		s := h.scenarios[i]
		fmt.Fprintf(os.Stderr, "running %s against %s\n", s.Name, label)
		results[i] = h.compareScenario(ctx, s, refs[i], errors.Join(refErrs[i], sourceErr), source, filepath.Join(h.outDir, s.Name, label))
		fmt.Fprintf(os.Stderr, "%s: %s\n", s.Name, results[i].Status)
	})
	r := report.Report{Binary: fmt.Sprintf("%s (%s)", label, h.binary), Source: h.source(), Results: results}
	if err := report.Write(h.outDir, r); err != nil {
		return err
	}
	fmt.Printf("%s\nReport: %s\n", report.ParityBar(results), filepath.Join(h.outDir, "report.md"))
	// Only pass and expected-only are passes; no-reference compared nothing.
	if slices.ContainsFunc(results, func(r report.Result) bool {
		return r.Status != compare.StatusPass && r.Status != compare.StatusExpectedOnly && r.Status != compare.StatusReported
	}) {
		stop()
		os.Exit(1)
	}
	return nil
}

// compareScenario runs s against the binary and decides its result against ref, which is nil
// without a reference. refErr is the error of loading the reference or SOURCE. That error, a
// reference recorded from another Go binary or other input files, or a failure to run or capture
// the scenario gives the status harness-error.
func (h *harness) compareScenario(ctx context.Context, s scenario.Scenario, ref *reference.Reference, refErr error, source reference.Source, runDir string) report.Result {
	if ref != nil && refErr == nil {
		var inputs map[string]string
		if inputs, refErr = h.inputs(s); refErr == nil {
			refErr = reference.Check(*ref, source, inputs)
		}
	}
	if refErr != nil && (ref != nil || !errors.Is(refErr, fs.ErrNotExist)) {
		return report.Result{Scenario: s, Status: compare.StatusHarnessError, Error: refErr.Error()}
	}
	observed, err := h.observe(ctx, s, runDir)
	if err != nil {
		return report.Result{Scenario: s, Status: compare.StatusHarnessError, Error: err.Error()}
	}
	return decide(ref, observed, s, h.defaults, h.tolerances, h.expectedPath)
}

// decide compares an observed run with the reference: content and timing differences together,
// then the expected differences, then the status. The reference's timing is measured again from
// its timing input with tol. Without a reference the status is no-reference. An invalid expected
// differences file gives harness-error. The scenario's own overrides take precedence over defaults.
func decide(ref *reference.Reference, observed observation, s scenario.Scenario, defaults []scenario.Override, tol timing.Tolerances, expectedPath string) report.Result {
	result := report.Result{
		Scenario:       s,
		UnmatchedRules: observed.normalized.UnmatchedRules,
		Actual:         &report.Run{Measures: observed.measures, Resources: observed.resources, Killed: observed.killed},
	}
	if ref == nil {
		result.Status = compare.Status(nil, nil)
		return result
	}
	referenceMeasures := timing.Measure(ref.Normalized.Timing, tol)
	result.Reference = &report.Run{Measures: referenceMeasures, Resources: ref.Resources}
	differences := compare.Content(ref.Normalized, observed.normalized, compare.WithDefaults(s.Compare.Overrides, defaults), tol)
	differences = append(differences, timing.Compare(referenceMeasures, observed.measures, tol, 0)...)
	differences, err := compare.ApplyExpected(differences, s.Name, expectedPath)
	if err != nil {
		result.Status, result.Error = compare.StatusHarnessError, err.Error()
		return result
	}
	result.Differences = differences
	result.Status = compare.Status(&ref.Normalized, differences)
	if s.ReportOnly {
		result.Status = compare.StatusReported
	}
	return result
}

// source names the Go commit and binary of the references for the report.
func (h *harness) source() string {
	s, err := reference.ReadSource(filepath.Join(h.referenceDir, "SOURCE"))
	if errors.Is(err, fs.ErrNotExist) {
		return "not recorded; reference/SOURCE does not exist"
	}
	if err != nil {
		return "unreadable: " + err.Error()
	}
	return s.String()
}

func recordCommand(cfg build.Config, args []string) error {
	c := common{cfg: cfg}
	flags := flag.NewFlagSet("record", flag.ExitOnError)
	c.register(flags)
	flags.StringVar(&c.cfg.AgentPath, "agent-path", c.cfg.AgentPath, "datadog-agent checkout that the Go binary was built from")
	repin := flags.Bool("repin", false, "pin the references to the current datadog-agent commit, and remove the references of scenarios that no longer exist")
	flags.Parse(args)
	if *repin && len(c.names) > 0 {
		return errors.New("--repin records every scenario, so it cannot be combined with --scenario")
	}
	resolved, err := c.cfg.Resolve()
	if err != nil {
		return err
	}
	parityDir := filepath.Join(resolved.ComponentsDir, "parity")
	sourceFile := filepath.Join(parityDir, "reference", "SOURCE")
	commit, err := reference.CheckSource(sourceFile, resolved.AgentPath, *repin)
	if err != nil {
		return err
	}
	pinned, err := reference.ReadSource(sourceFile)
	if err != nil && !(*repin && errors.Is(err, fs.ErrNotExist)) {
		return err
	}
	fakeintakeCommit, err := reference.FakeintakeCommit(parityDir)
	if err != nil {
		return fmt.Errorf("finding the fakeintake commit: %w", err)
	}
	ctx, stop := signalContext()
	defer stop()
	h, err := setUp(ctx, &c, "go")
	if err != nil {
		return err
	}
	binarySHA256, err := reference.FileSHA256(h.binary)
	if err != nil {
		return err
	}
	info, err := build.ReadInfo(filepath.Join(filepath.Dir(h.binary), build.InfoFile))
	if err != nil {
		return fmt.Errorf("reading what the Go binary was built from; build it with parity build go: %w", err)
	}
	if err := reference.CheckBuild(info, binarySHA256, pinned, commit, *repin); err != nil {
		return err
	}
	staging := filepath.Join(h.outDir, "staged-references")
	errs := make([]error, len(h.scenarios))
	refs, _ := h.loadReferences()
	h.schedule(refs, c.parallel, func(i int) {
		s := h.scenarios[i]
		fmt.Fprintf(os.Stderr, "recording %s\n", s.Name)
		inputs, err := h.inputs(s)
		var observed observation
		if err == nil {
			observed, err = h.observe(ctx, s, filepath.Join(h.outDir, s.Name, "go"))
		}
		if err == nil {
			err = reference.Save(staging, reference.Reference{
				Scenario: s.Name, GoBinarySHA256: binarySHA256, Inputs: inputs,
				Normalized: observed.normalized, Measures: observed.measures, Resources: observed.resources,
			})
		}
		if err != nil {
			errs[i] = fmt.Errorf("scenario %s was not recorded: %w", s.Name, err)
		}
	})
	if err := errors.Join(errs...); err != nil {
		return fmt.Errorf("no reference was changed:\n%w", err)
	}
	names := make([]string, len(h.scenarios))
	for i, s := range h.scenarios {
		names[i] = s.Name
	}
	if err := reference.Publish(staging, h.referenceDir, names, *repin, reference.Source{Commit: commit, GoBinarySHA256: binarySHA256, FakeintakeCommit: fakeintakeCommit}); err != nil {
		return err
	}
	fmt.Printf("Recorded %d references in %s\n", len(h.scenarios), h.referenceDir)
	return nil
}

func stabilityCommand(cfg build.Config, args []string) error {
	c := common{cfg: cfg}
	flags := flag.NewFlagSet("stability", flag.ExitOnError)
	c.register(flags)
	runs := flags.Int("runs", 5, "how many times to run each scenario against Go")
	flags.Parse(args)
	if *runs < 2 {
		return fmt.Errorf("--runs is %d; it must be at least 2 to compare runs", *runs)
	}
	ctx, stop := signalContext()
	defer stop()
	h, err := setUp(ctx, &c, "go")
	if err != nil {
		return err
	}
	sections := make([]string, len(h.scenarios))
	stable := make([]bool, len(h.scenarios))
	refs, _ := h.loadReferences()
	h.schedule(refs, c.parallel, func(i int) {
		s := h.scenarios[i]
		var captures []normalize.Normalized
		for run := 1; run <= *runs; run++ {
			fmt.Fprintf(os.Stderr, "running %s against go, run %d of %d\n", s.Name, run, *runs)
			observed, err := h.observe(ctx, s, filepath.Join(h.outDir, s.Name, "go", fmt.Sprintf("run-%d", run)))
			if err != nil {
				sections[i] = fmt.Sprintf("## %s\n\nHarness error in run %d: %v\n", s.Name, run, err)
				return
			}
			captures = append(captures, observed.normalized)
		}
		varying := report.Stability(captures, compare.WithDefaults(s.Compare.Overrides, h.defaults), h.tolerances)
		if len(varying) == 0 {
			sections[i], stable[i] = fmt.Sprintf("## %s\n\nStable over %d runs.\n", s.Name, *runs), true
			return
		}
		sections[i] = fmt.Sprintf("## %s\n\nVaries over %d runs:\n\n- %s\n", s.Name, *runs, strings.Join(varying, "\n- "))
	})
	text := "# Stability of Go\n\n" + strings.Join(sections, "\n")
	path := filepath.Join(h.outDir, "stability.md")
	if err := os.WriteFile(path, []byte(text), 0o644); err != nil {
		return err
	}
	fmt.Print(text)
	fmt.Printf("\nStability report: %s\n", path)
	if slices.Contains(stable, false) {
		stop()
		os.Exit(1)
	}
	return nil
}
